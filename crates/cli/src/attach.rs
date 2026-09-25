//! `tether attach`: a terminal window on a session. Output is written through as the session
//! sent it, after a repaint that includes the scrollback; keys go in as the window's terminal
//! sends them; the window's size is the session's while this window is the one in use. Ctrl-]
//! detaches (twice sends it through) and leaves the program running; closing the window instead
//! ends it when this was its last window, unless the session is kept. When the session exits
//! the window says so and exits with the program's code; when the host is lost it exits 75.

use crate::console;
use serde_json::json;
use std::io::{Read, Write};
use std::time::Duration;
use tether_client::{Client, StreamItem};
use tokio::sync::mpsc;

pub struct AttachOptions {
    /// Types nothing.
    pub read_only: bool,
    /// Types nothing and never sizes the session: a window that only watches.
    pub view: bool,
    /// Ctrl-] detaches.
    pub detach_key: bool,
    pub title: Option<String>,
}

pub const HOST_LOST: i32 = 75;
const DETACH: u8 = 0x1d;

enum Key {
    Bytes(Vec<u8>),
    Detach,
}

/// Finds the detach key in what the keyboard sent: the raw byte, or the key as Windows
/// Terminal and conhost encode it once the session turned on win32-input-mode
/// (`ESC [ Vk ; Sc ; Uc ; Kd ; Cs ; Rc _`, with `Uc` 29 for Ctrl-]). Pressing it twice sends
/// it through once.
#[derive(Default)]
struct DetachScanner {
    held: Vec<u8>,
    armed: bool,
}

fn win32_key(seq: &[u8]) -> Option<(u32, bool)> {
    let inner = seq.strip_prefix(b"\x1b[")?.strip_suffix(b"_")?;
    let parts: Vec<u32> = std::str::from_utf8(inner)
        .ok()?
        .split(';')
        .map(|p| p.parse().unwrap_or(0))
        .collect();
    (parts.len() >= 4).then(|| (parts[2], parts[3] == 1))
}

impl DetachScanner {
    fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        let mut data = std::mem::take(&mut self.held);
        data.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut pass = Vec::new();
        let mut i = 0;
        while i < data.len() {
            let b = data[i];
            if b == 0x1b && data.get(i + 1) == Some(&b'[') {
                // A win32-input-mode key: digits and semicolons up to `_`.
                let mut j = i + 2;
                while j < data.len() && (data[j].is_ascii_digit() || data[j] == b';') {
                    j += 1;
                }
                if j == data.len() && j - i < 64 {
                    self.held = data[i..].to_vec();
                    break;
                }
                if j < data.len() && data[j] == b'_' {
                    let seq = &data[i..=j];
                    if let Some((29, down)) = win32_key(seq) {
                        if down {
                            self.press(&mut out, &mut pass, seq);
                        } else if !self.armed {
                            // The release of a Ctrl-] that went through.
                            pass.extend_from_slice(seq);
                        }
                        i = j + 1;
                        continue;
                    }
                    self.armed = false;
                    pass.extend_from_slice(seq);
                    i = j + 1;
                    continue;
                }
            }
            if b == DETACH {
                self.press(&mut out, &mut pass, &[DETACH]);
            } else {
                self.armed = false;
                pass.push(b);
            }
            i += 1;
        }
        if !pass.is_empty() {
            out.push(Key::Bytes(pass));
        }
        out
    }

    fn press(&mut self, out: &mut Vec<Key>, pass: &mut Vec<u8>, seq: &[u8]) {
        if self.armed {
            self.armed = false;
            pass.extend_from_slice(seq);
        } else {
            self.armed = true;
            if !pass.is_empty() {
                out.push(Key::Bytes(std::mem::take(pass)));
            }
            out.push(Key::Detach);
        }
    }
}

/// Runs until the session exits, the user detaches, or the host is lost; the exit code.
pub async fn attach(client: Client, session: &str, opts: AttachOptions) -> i32 {
    let restore = match console::raw() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("tether: {e}");
            return 1;
        }
    };
    if let Some(t) = &opts.title {
        console::set_title(t);
    }
    let size = console::size();
    let mut events = client.events();
    let mut sub = match client
        .subscribe(json!({
            "session": session,
            "from": "snapshot",
            "input": !(opts.read_only || opts.view),
            "sizing": !opts.view,
            "size": size.map(|(c, r)| json!({ "cols": c, "rows": r })),
            "role": "window",
            "scrollback": true,
        }))
        .await
    {
        Ok(s) => s,
        Err(e) => {
            drop(restore);
            eprintln!("tether: {e}");
            return 1;
        }
    };

    // Output goes to the window from a thread of its own, so a slow console never holds up
    // the connection.
    let (out_tx, out_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut stdout = std::io::stdout().lock();
        for bytes in out_rx {
            if stdout
                .write_all(&bytes)
                .and_then(|_| stdout.flush())
                .is_err()
            {
                break;
            }
        }
    });

    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<Key>();
    let detach_key = opts.detach_key;
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 4096];
        let mut scanner = DetachScanner::default();
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let keys = if detach_key {
                        scanner.feed(&buf[..n])
                    } else {
                        vec![Key::Bytes(buf[..n].to_vec())]
                    };
                    for k in keys {
                        if key_tx.send(k).is_err() {
                            return;
                        }
                    }
                }
            }
        }
    });

    let (size_tx, mut size_rx) = mpsc::unbounded_channel::<(u16, u16)>();
    if !opts.view {
        let mut last = size;
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(200));
            let now = console::size();
            if now != last {
                last = now;
                if let Some(s) = now {
                    if size_tx.send(s).is_err() {
                        break;
                    }
                }
            }
        });
    }

    let typing = !(opts.read_only || opts.view);
    let (code, note) = loop {
        tokio::select! {
            item = sub.rx.recv() => match item {
                Some(StreamItem::Output { data, .. }) | Some(StreamItem::Resync { data, .. }) => {
                    let _ = out_tx.send(data);
                }
                None => break (HOST_LOST, "the host is gone".to_string()),
            },
            ev = events.recv() => match ev {
                Some(ev) if ev["ev"] == "exited" && ev["session"].as_str().is_some() => {
                    let code = ev["code"].as_i64().unwrap_or(-1);
                    break (code as i32, format!("exited {code}"));
                }
                Some(_) => {}
                None => break (HOST_LOST, "the host is gone".to_string()),
            },
            key = key_rx.recv() => match key {
                Some(Key::Bytes(b)) if typing => { client.input(sub.stream, &b); }
                Some(Key::Bytes(_)) => {}
                Some(Key::Detach) => {
                    // Said to the host, so it knows the window left on purpose: a window that
                    // just goes was closed, and the last one to close ends the program.
                    let _ = client.request("unsubscribe", json!({ "stream": sub.stream })).await;
                    break (0, "detached".to_string());
                }
                // The keyboard's input ended: the window is going.
                None => break (0, "detached".to_string()),
            },
            s = size_rx.recv() => {
                if let Some((cols, rows)) = s {
                    let _ = client.request("resize", json!({ "session": session, "cols": cols, "rows": rows })).await;
                }
            }
        }
    };
    // Let the last output reach the window before the note.
    tokio::time::sleep(Duration::from_millis(50)).await;
    while let Ok(StreamItem::Output { data, .. } | StreamItem::Resync { data, .. }) =
        sub.rx.try_recv()
    {
        let _ = out_tx.send(data);
    }
    drop(out_tx);
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(restore);
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "\x1b[0m\r\n[{note}]\r\n");
    let _ = stdout.flush();
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(s: &mut DetachScanner, bytes: &[u8]) -> Vec<String> {
        s.feed(bytes)
            .into_iter()
            .map(|k| match k {
                Key::Bytes(b) => String::from_utf8_lossy(&b).to_string(),
                Key::Detach => "<detach>".to_string(),
            })
            .collect()
    }

    #[test]
    fn detaches_on_ctrl_bracket_and_sends_it_through_when_doubled() {
        let mut s = DetachScanner::default();
        assert_eq!(run(&mut s, b"ab\x1dc"), ["ab", "<detach>", "c"]);
        let mut s = DetachScanner::default();
        assert_eq!(run(&mut s, b"\x1d"), ["<detach>"]);
        assert_eq!(run(&mut s, b"\x1d"), ["\x1d"]);
    }

    #[test]
    fn reads_the_win32_encoding_across_reads() {
        let mut s = DetachScanner::default();
        assert_eq!(
            run(&mut s, b"\x1b[65;30;97;1;0;1_\x1b[221;27;2"),
            ["\x1b[65;30;97;1;0;1_"]
        );
        assert_eq!(run(&mut s, b"9;1;8;1_\x1b[221;27;29;0;8;1_"), ["<detach>"]);
        // Arrows and other CSI sequences pass untouched.
        assert_eq!(run(&mut s, b"\x1b[A\x1b[1;5C"), ["\x1b[A\x1b[1;5C"]);
    }
}
