//! testtui: a program for tether's tests to run in a real pseudo-terminal. It starts by
//! printing `READY`, asking the terminal where the cursor is and printing the answer as
//! `CPR row col`, and printing its size as `SIZE cols rows`, and prints the size again whenever
//! it changes. Every read of its input is printed as `IN <hex>`. A line typed and ended with
//! Enter is a command:
//!
//! | Command | Does |
//! |---|---|
//! | `lines N` | prints `line 1` … `line N` |
//! | `title T` | sets the window title (OSC 2) |
//! | `bell` | rings the bell |
//! | `cwd P` | reports a working directory (OSC 7) |
//! | `alt` | enters the alternate screen |
//! | `bracketed` | turns bracketed paste on |
//! | `appcursor` | turns application cursor keys on |
//! | `cpr` | asks for the cursor position again |
//! | `flood N` | prints N KiB as fast as it can |
//! | `child` | starts a copy of itself that only sleeps, outside the terminal (on Windows in a console of its own), and prints `CHILD pid` |
//! | `exit N` | exits with code N |
//!
//! `testtui --sleep` only sleeps, for two minutes.

use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::Duration;

#[cfg(windows)]
mod term {
    use windows_sys::Win32::System::Console::*;

    pub fn raw() {
        unsafe {
            let input = GetStdHandle(STD_INPUT_HANDLE);
            let output = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut mode = 0u32;
            GetConsoleMode(input, &mut mode);
            SetConsoleMode(
                input,
                (mode & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
                    | ENABLE_VIRTUAL_TERMINAL_INPUT,
            );
            GetConsoleMode(output, &mut mode);
            SetConsoleMode(
                output,
                mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN,
            );
            SetConsoleCP(65001);
            SetConsoleOutputCP(65001);
        }
    }

    pub fn size() -> (u16, u16) {
        unsafe {
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
            GetConsoleScreenBufferInfo(GetStdHandle(STD_OUTPUT_HANDLE), &mut info);
            (
                (info.srWindow.Right - info.srWindow.Left + 1) as u16,
                (info.srWindow.Bottom - info.srWindow.Top + 1) as u16,
            )
        }
    }
}

#[cfg(unix)]
mod term {
    pub fn raw() {
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            libc::tcgetattr(0, &mut t);
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(0, libc::TCSANOW, &t);
        }
    }

    pub fn size() -> (u16, u16) {
        unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            libc::ioctl(1, libc::TIOCGWINSZ, &mut ws);
            (ws.ws_col, ws.ws_row)
        }
    }
}

fn say(out: &mut impl Write, s: &str) {
    let _ = out.write_all(s.as_bytes());
    let _ = out.write_all(b"\r\n");
    let _ = out.flush();
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A cursor position report in what was read, if any: `ESC [ row ; col R`.
fn cpr(b: &[u8]) -> Option<(u32, u32)> {
    let s = std::str::from_utf8(b).ok()?;
    let start = s.find("\x1b[")?;
    let rest = &s[start + 2..];
    let end = rest.find('R')?;
    let (r, c) = rest[..end].split_once(';')?;
    Some((r.parse().ok()?, c.parse().ok()?))
}

/// A copy of this program that only sleeps, with nothing of the terminal's: its pid.
fn child() -> std::io::Result<u32> {
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("--sleep")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // A console of its own, so closing the session's never reaches it.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    Ok(cmd.spawn()?.id())
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--sleep") {
        std::thread::sleep(Duration::from_secs(120));
        return;
    }
    term::raw();
    let mut out = std::io::stdout();
    say(&mut out, "READY");

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let ask_cpr = |out: &mut std::io::Stdout, rx: &mpsc::Receiver<Vec<u8>>| {
        let _ = out.write_all(b"\x1b[6n");
        let _ = out.flush();
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(b) => match cpr(&b) {
                Some((r, c)) => say(out, &format!("CPR {r} {c}")),
                None => say(out, &format!("CPR? {}", hex(&b))),
            },
            Err(_) => say(out, "CPR none"),
        }
    };
    ask_cpr(&mut out, &rx);

    let mut size = term::size();
    say(&mut out, &format!("SIZE {} {}", size.0, size.1));
    let mut line = String::new();
    loop {
        let now = term::size();
        if now != size {
            size = now;
            say(&mut out, &format!("SIZE {} {}", size.0, size.1));
        }
        let bytes = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(b) => b,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        };
        say(&mut out, &format!("IN {}", hex(&bytes)));
        // Keys and pastes with escape sequences are only echoed; a command is typed plain.
        if bytes.contains(&0x1b) {
            line.clear();
            continue;
        }
        for &b in &bytes {
            if b != b'\r' {
                if b.is_ascii_graphic() || b == b' ' {
                    line.push(b as char);
                }
                continue;
            }
            let cmd = std::mem::take(&mut line);
            let (verb, arg) = cmd.split_once(' ').unwrap_or((cmd.as_str(), ""));
            match verb {
                "lines" => {
                    for i in 1..=arg.parse::<u32>().unwrap_or(0) {
                        say(&mut out, &format!("line {i}"));
                    }
                }
                "title" => {
                    let _ = write!(out, "\x1b]2;{arg}\x07");
                    say(&mut out, "TITLED");
                }
                "bell" => {
                    let _ = out.write_all(b"\x07");
                    say(&mut out, "RANG");
                }
                "cwd" => {
                    let _ = write!(out, "\x1b]7;file://host{arg}\x07");
                    say(&mut out, "CWD");
                }
                "alt" => say(&mut out, "\x1b[?1049hALT"),
                "bracketed" => say(&mut out, "\x1b[?2004hBRACKETED"),
                "appcursor" => say(&mut out, "\x1b[?1hAPPCURSOR"),
                "cpr" => ask_cpr(&mut out, &rx),
                "flood" => {
                    let chunk = "0123456789abcdef".repeat(64);
                    for _ in 0..arg.parse::<u32>().unwrap_or(0) {
                        let _ = out.write_all(chunk.as_bytes());
                    }
                    say(&mut out, "\r\nFLOODED");
                }
                "child" => match child() {
                    Ok(pid) => say(&mut out, &format!("CHILD {pid}")),
                    Err(e) => say(&mut out, &format!("CHILD? {e}")),
                },
                "exit" => {
                    say(&mut out, "BYE");
                    std::process::exit(arg.parse().unwrap_or(0));
                }
                _ => {}
            }
        }
    }
}
