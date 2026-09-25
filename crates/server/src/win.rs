//! The Windows parts: who the user is (a SID), a security descriptor that lets only the user
//! in, who is on the other end of a pipe, and starting a process that inherits no handle. The
//! last matters because whoever starts the host may hold sockets that must not outlive it: a
//! handle inherited by a host that runs for hours keeps the caller's port bound after the
//! caller has gone.

use std::ffi::{c_void, OsStr};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_ACCESS_DENIED, HANDLE, STILL_ACTIVE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetCurrentProcess, GetExitCodeProcess, OpenProcess, OpenProcessToken,
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_UNICODE_ENVIRONMENT,
    PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, STARTUPINFOW,
};

pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

fn from_wide_ptr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0;
    unsafe {
        while *p.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
    }
}

/// The user SID of a process's token, as a string (`S-1-5-21-…`).
fn token_sid(process: HANDLE) -> io::Result<String> {
    unsafe {
        let mut token: HANDLE = ptr::null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut len = 0u32;
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut len);
        let mut buf = vec![0u8; len as usize];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr() as *mut c_void,
            len,
            &mut len,
        );
        CloseHandle(token);
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s: *mut u16 = ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut s) == 0 {
            return Err(io::Error::last_os_error());
        }
        let out = from_wide_ptr(s);
        LocalFree(s as *mut c_void);
        Ok(out)
    }
}

pub fn current_user_sid() -> io::Result<String> {
    token_sid(unsafe { GetCurrentProcess() })
}

/// The SID a process runs as, or why it cannot be read.
pub fn process_sid(pid: u32) -> io::Result<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return Err(io::Error::last_os_error());
        }
        let r = token_sid(h);
        CloseHandle(h);
        r
    }
}

/// Security attributes that grant the user alone and refuse network logons; the descriptor
/// lives as long as the process, which is as long as the pipe that uses it.
pub fn user_only_attributes(sid: &str) -> io::Result<*mut SECURITY_ATTRIBUTES> {
    let sddl = wide(format!("D:P(D;;GA;;;NU)(A;;GA;;;{sid})"));
    let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Box::into_raw(Box::new(SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    })))
}

pub fn process_alive(pid: u32) -> bool {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            // A process that exists but will not be opened is still alive.
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        ok != 0 && code == STILL_ACTIVE as u32
    }
}

/// One argument quoted the way the C runtime splits a command line.
pub fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

/// Starts `argv` with no inherited handle, the flags given, in `cwd`; its pid. Leaving the
/// caller's job is asked for and, where the job forbids it, done without.
pub fn spawn_clean(argv: &[String], flags: u32, cwd: Option<&std::path::Path>) -> io::Result<u32> {
    let line = argv
        .iter()
        .map(|a| quote_arg(a))
        .collect::<Vec<_>>()
        .join(" ");
    let cwd_w = cwd.map(wide);
    let attempt = |flags: u32| -> io::Result<u32> {
        let mut cmd = wide(&line);
        let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            CreateProcessW(
                ptr::null(),
                cmd.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                0,
                flags | CREATE_UNICODE_ENVIRONMENT,
                ptr::null(),
                cwd_w.as_ref().map(|c| c.as_ptr()).unwrap_or(ptr::null()),
                &si,
                &mut pi,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        unsafe {
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
        }
        Ok(pi.dwProcessId)
    };
    match attempt(flags | CREATE_BREAKAWAY_FROM_JOB) {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => attempt(flags),
        r => r,
    }
}

/// A process group of its own, so a console's Ctrl+C never reaches it.
pub const NEW_GROUP: u32 = CREATE_NEW_PROCESS_GROUP;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_like_the_c_runtime() {
        assert_eq!(quote_arg("plain"), "plain");
        assert_eq!(quote_arg("with space"), "\"with space\"");
        assert_eq!(quote_arg(r"C:\dir\"), r"C:\dir\");
        assert_eq!(quote_arg(r"C:\a b\"), r#""C:\a b\\""#);
        assert_eq!(quote_arg(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote_arg(""), "\"\"");
    }

    #[test]
    fn knows_the_user_and_this_process() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-"));
        assert_eq!(process_sid(std::process::id()).unwrap(), sid);
        assert!(process_alive(std::process::id()));
        assert!(!user_only_attributes(&sid).unwrap().is_null());
    }
}
