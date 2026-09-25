//! `serve --daemonize`: the host started fully apart from whoever asked for it, and not
//! reported until it is listening. On Windows it is created detached, in a process group of
//! its own, outside the caller's job where the job allows, and with no handle inherited. On
//! macOS and Linux it is forked twice through a session of its own, so it has no controlling
//! terminal and can never acquire one, with every descriptor above stderr closed.

use std::io;
use std::path::Path;
use std::time::{Duration, Instant};
use tether_proto::discovery::{self, HostFile};

/// Waits for the host with this pid to announce itself.
fn await_host(
    dir: &Path,
    pid: u32,
    timeout: Duration,
    alive: impl Fn(u32) -> bool,
) -> io::Result<HostFile> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(f) = discovery::read_hosts(dir)
            .into_iter()
            .find(|h| h.pid == pid)
        {
            return Ok(f);
        }
        if !alive(pid) {
            return Err(io::Error::other(format!(
                "the host (pid {pid}) exited before it was ready; see {}",
                discovery::logs_dir(dir).display()
            )));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "the host (pid {pid}) was not ready within {}s",
                    timeout.as_secs()
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(windows)]
pub fn daemonize(
    exe: &Path,
    args: &[String],
    dir: &Path,
    timeout: Duration,
) -> io::Result<HostFile> {
    use windows_sys::Win32::System::Threading::DETACHED_PROCESS;
    discovery::ensure_dirs(dir)?;
    let mut argv = vec![exe.to_string_lossy().to_string(), "serve".to_string()];
    argv.extend(args.iter().cloned());
    let pid = crate::win::spawn_clean(&argv, DETACHED_PROCESS | crate::win::NEW_GROUP, Some(dir))?;
    await_host(dir, pid, timeout, crate::win::process_alive)
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    // Signal 0 only checks: the process exists, or exists as another user's (EPERM).
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    r == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(unix)]
fn close_inherited() {
    // Everything above stderr goes: whatever the starter held open is not the host's.
    for fd in 3..1024 {
        unsafe {
            libc::close(fd);
        }
    }
}

#[cfg(unix)]
pub fn daemonize(
    exe: &Path,
    args: &[String],
    dir: &Path,
    timeout: Duration,
) -> io::Result<HostFile> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    discovery::ensure_dirs(dir)?;
    let mut stage = Command::new(exe);
    stage
        .arg("serve")
        .arg("--stage2")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    unsafe {
        stage.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = stage.spawn()?;
    let mut out = String::new();
    child
        .stdout
        .take()
        .expect("piped")
        .read_to_string(&mut out)?;
    child.wait()?;
    let pid: u32 = out
        .trim()
        .parse()
        .map_err(|_| io::Error::other(format!("the host did not start: {out:?}")))?;
    await_host(dir, pid, timeout, alive)
}

/// The middle of the double fork: already a session leader, it starts the host, which is not
/// one, says its pid, and leaves.
#[cfg(unix)]
pub fn stage2(exe: &Path, args: &[String], dir: &Path) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let mut host = Command::new(exe);
    host.arg("serve")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        host.pre_exec(|| {
            close_inherited();
            Ok(())
        });
    }
    let child = host.spawn()?;
    println!("{}", child.id());
    Ok(())
}
