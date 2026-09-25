//! `tether open`: a terminal window attached to a session, in whichever terminal the platform
//! has. Every application that wants a window gets the same launcher, and no window it starts
//! inherits the caller's handles.
//!
//! On Windows, Windows Terminal is preferred: it draws the symbols a TUI uses from fallback
//! fonts, where the classic console window shows `?`. It is reached through the shell (it is
//! an app execution alias, which CreateProcess cannot start), and it keeps the user's own
//! setting for new windows or tabs. `console` asks Windows for a new console, which is the
//! user's default terminal application; `conhost` names the classic window. On macOS,
//! Terminal.app runs a `.command` script and iTerm2 is told over AppleScript; on Linux the
//! first terminal program found runs a script. The scripts only hold the attach command.
//!
//! What was opened names the terminal used and, when `open` started the window's process
//! itself (a console window), its pid, so the caller can close exactly that window. Windows
//! Terminal's alias hands the window to a process of its own, and a Unix terminal program may
//! do the same, so those have none.

use std::io;
use std::path::Path;

pub struct Opened {
    pub terminal: String,
    pub pid: Option<u32>,
}

fn opened(terminal: &str, pid: Option<u32>) -> Opened {
    Opened {
        terminal: terminal.into(),
        pid,
    }
}

pub struct OpenRequest<'a> {
    pub exe: &'a Path,
    pub session: &'a str,
    pub title: &'a str,
    pub cwd: Option<&'a Path>,
    /// Extra arguments for `attach` (`--dir`, `--view`).
    pub attach_args: Vec<String>,
    pub terminal: &'a str,
}

fn attach_argv(req: &OpenRequest) -> Vec<String> {
    let mut argv = vec![req.exe.to_string_lossy().to_string()];
    argv.extend(req.attach_args.iter().cloned());
    argv.extend([
        "attach".to_string(),
        req.session.to_string(),
        "--title".to_string(),
        req.title.to_string(),
    ]);
    argv
}

#[cfg(windows)]
fn find_in_path(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| std::fs::symlink_metadata(p).is_ok())
}

/// The terminal `auto` means here.
#[cfg(windows)]
pub fn auto() -> &'static str {
    if find_in_path("wt.exe").is_some() {
        "wt"
    } else {
        "console"
    }
}

#[cfg(target_os = "macos")]
pub fn auto() -> &'static str {
    if Path::new("/Applications/iTerm.app").exists() {
        "iterm2"
    } else {
        "terminal"
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn auto() -> &'static str {
    "linux"
}

#[cfg(windows)]
pub fn open(req: &OpenRequest) -> io::Result<Opened> {
    use tether_server::win::{quote_arg, spawn_clean, wide};
    use windows_sys::Win32::System::Threading::{CREATE_NEW_CONSOLE, DETACHED_PROCESS};
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let terminal = if req.terminal == "auto" {
        auto()
    } else {
        req.terminal
    };
    let argv = attach_argv(req);
    match terminal {
        "wt" => {
            // `;` separates Windows Terminal's own commands, so every one in an argument is escaped.
            let wt = |s: &str| quote_arg(&s.replace(';', "\\;"));
            let mut params = vec!["new-tab".to_string(), "--title".into(), wt(req.title)];
            if let Some(d) = req.cwd {
                params.extend(["-d".to_string(), wt(&d.to_string_lossy())]);
            }
            params.push("--".into());
            params.extend(argv.iter().map(|a| wt(a)));
            let params = wide(params.join(" "));
            let file = wide("wt.exe");
            let verb = wide("open");
            let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
            info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
            info.fMask = SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
            info.lpVerb = verb.as_ptr();
            info.lpFile = file.as_ptr();
            info.lpParameters = params.as_ptr();
            info.nShow = SW_SHOWNORMAL;
            if unsafe { ShellExecuteExW(&mut info) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(opened("wt", None))
        }
        "console" => {
            let pid = spawn_clean(&argv, CREATE_NEW_CONSOLE, req.cwd)?;
            Ok(opened("console", Some(pid)))
        }
        "conhost" => {
            let mut with_host = vec!["conhost.exe".to_string()];
            with_host.extend(argv);
            let pid = spawn_clean(&with_host, DETACHED_PROCESS, req.cwd)?;
            Ok(opened("conhost", Some(pid)))
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("no terminal {other:?} here; try wt, console or conhost"),
        )),
    }
}

#[cfg(unix)]
fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// A script that runs the attach command in a terminal's place.
#[cfg(unix)]
fn launch_script(req: &OpenRequest, suffix: &str) -> io::Result<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let dir = tether_proto::discovery::state_dir().join("launch");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}{suffix}", req.session));
    let mut body = String::from("#!/bin/sh\n");
    if let Some(d) = req.cwd {
        body.push_str(&format!(
            "cd {} || true\n",
            shell_quote(&d.to_string_lossy())
        ));
    }
    body.push_str(&format!(
        "exec {}\n",
        attach_argv(req)
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    std::fs::write(&path, body)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

#[cfg(unix)]
fn spawn_detached(program: &str, args: &[String]) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            for fd in 3..1024 {
                libc::close(fd);
            }
            Ok(())
        });
    }
    cmd.spawn()?;
    Ok(())
}

#[cfg(unix)]
fn which(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
}

#[cfg(unix)]
pub fn open(req: &OpenRequest) -> io::Result<Opened> {
    let terminal = if req.terminal == "auto" {
        auto()
    } else {
        req.terminal
    };
    match terminal {
        "terminal" => {
            let script = launch_script(req, ".command")?;
            spawn_detached(
                "open",
                &[
                    "-a".into(),
                    "Terminal".into(),
                    script.to_string_lossy().to_string(),
                ],
            )?;
            Ok(opened("terminal", None))
        }
        "iterm2" => {
            let script = launch_script(req, ".sh")?;
            let apple = format!("tell application \"iTerm2\" to create window with default profile command \"/bin/sh {}\"", script.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\""));
            spawn_detached("osascript", &["-e".into(), apple])?;
            Ok(opened("iterm2", None))
        }
        _ => {
            if std::env::var_os("DISPLAY").is_none()
                && std::env::var_os("WAYLAND_DISPLAY").is_none()
            {
                return Err(io::Error::other("no display to open a terminal on"));
            }
            let script = launch_script(req, ".sh")?.to_string_lossy().to_string();
            let named = std::env::var("TERMINAL").ok().filter(|t| !t.is_empty());
            let candidates: Vec<(String, Vec<String>)> = named
                .iter()
                .map(|t| {
                    (
                        t.clone(),
                        vec!["-e".into(), "/bin/sh".into(), script.clone()],
                    )
                })
                .chain([
                    (
                        "x-terminal-emulator".to_string(),
                        vec!["-e".into(), "/bin/sh".into(), script.clone()],
                    ),
                    (
                        "gnome-terminal".into(),
                        vec!["--".into(), "/bin/sh".into(), script.clone()],
                    ),
                    (
                        "konsole".into(),
                        vec!["-e".into(), "/bin/sh".into(), script.clone()],
                    ),
                    (
                        "xfce4-terminal".into(),
                        vec!["-e".into(), format!("/bin/sh {script}")],
                    ),
                    (
                        "alacritty".into(),
                        vec!["-e".into(), "/bin/sh".into(), script.clone()],
                    ),
                    ("kitty".into(), vec!["/bin/sh".into(), script.clone()]),
                    (
                        "xterm".into(),
                        vec!["-e".into(), "/bin/sh".into(), script.clone()],
                    ),
                ])
                .collect();
            let wanted = if terminal == "linux" {
                None
            } else {
                Some(terminal)
            };
            for (program, args) in candidates {
                if wanted.is_some_and(|w| w != program) || !which(&program) {
                    continue;
                }
                spawn_detached(&program, &args)?;
                return Ok(opened(&program, None));
            }
            Err(io::Error::other("no terminal program found"))
        }
    }
}
