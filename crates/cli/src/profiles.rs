//! `tether profiles`: terminal profiles whose sessions start in tether, so a program the user
//! opens from their own terminal's menu is one any application can type into and watch. A
//! Windows Terminal fragment and an iTerm2 dynamic profile, each from the template of the
//! same name in `profiles/`; both terminals pick a new file up without a restart.

use std::io;
use std::path::PathBuf;

const WT_TEMPLATE: &str = include_str!("../../../profiles/windows-terminal.json");
const ITERM_TEMPLATE: &str = include_str!("../../../profiles/iterm2.json");

pub struct Profile {
    /// The folder a fragment goes in, and the name the terminal groups it under.
    pub app: String,
    /// The menu entry.
    pub name: String,
    /// What the profile's sessions run, as `tether run` takes it.
    pub command: Vec<String>,
    /// `run`'s own arguments before `--`.
    pub run_args: Vec<String>,
}

/// A JSON string's inside, for splicing into a template.
fn json_inner(s: &str) -> String {
    let quoted = serde_json::to_string(s).expect("a string serializes");
    quoted[1..quoted.len() - 1].to_string()
}

fn fnv(seed: u64, bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(seed, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x100000001b3))
}

/// A stable GUID for a profile, so reinstalling updates it rather than adding another.
fn guid(app: &str, name: &str) -> String {
    let key = format!("{app}\u{0}{name}");
    let a = fnv(0xcbf29ce484222325, key.as_bytes());
    let b = fnv(0x84222325cbf29ce4, key.as_bytes());
    let h = format!("{a:016X}{b:016X}");
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

fn run_line(exe: &str, p: &Profile, quote: impl Fn(&str) -> String) -> String {
    let mut words = vec![quote(exe)];
    words.extend(p.run_args.iter().map(|a| quote(a)));
    words.push("run".into());
    words.push("--".into());
    words.extend(p.command.iter().map(|a| quote(a)));
    words.join(" ")
}

pub fn windows_terminal(exe: &str, p: &Profile) -> String {
    #[cfg(windows)]
    let line = run_line(exe, p, tether_server::win::quote_arg);
    #[cfg(not(windows))]
    let line = run_line(exe, p, |s| s.to_string());
    WT_TEMPLATE
        .replace("{{name}}", &json_inner(&p.name))
        .replace("{{commandline}}", &json_inner(&line))
}

pub fn iterm2(exe: &str, p: &Profile) -> String {
    let line = run_line(exe, p, |s| {
        if s.contains([' ', '\'', '"']) {
            format!("'{}'", s.replace('\'', r"'\''"))
        } else {
            s.to_string()
        }
    });
    ITERM_TEMPLATE
        .replace("{{name}}", &json_inner(&p.name))
        .replace("{{guid}}", &guid(&p.app, &p.name))
        .replace("{{commandline}}", &json_inner(&line))
}

fn file_name(p: &Profile) -> String {
    let slug: String = p
        .name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("{}.json", slug.trim_matches('-'))
}

pub fn wt_path(p: &Profile) -> io::Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| io::Error::other("LOCALAPPDATA is not set"))?;
    Ok(PathBuf::from(base)
        .join("Microsoft")
        .join("Windows Terminal")
        .join("Fragments")
        .join(&p.app)
        .join(file_name(p)))
}

pub fn iterm_path(p: &Profile) -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| io::Error::other("HOME is not set"))?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("iTerm2")
        .join("DynamicProfiles")
        .join(format!("{}-{}", p.app, file_name(p))))
}

/// Writes a profile when its content changed; the path, and whether it was written.
pub fn install(path: PathBuf, content: &str) -> io::Result<(PathBuf, bool)> {
    if std::fs::read_to_string(&path).is_ok_and(|c| c == content) {
        return Ok((path, false));
    }
    tether_proto::discovery::write_atomic(&path, content.as_bytes())?;
    Ok((path, true))
}

/// The user's shell: what a profile runs when given nothing.
pub fn default_shell() -> Vec<String> {
    if cfg!(windows) {
        let pwsh = std::env::var_os("PATH").and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join("pwsh.exe"))
                .find(|p| p.is_file())
        });
        vec![pwsh
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "powershell.exe".into())]
    } else {
        vec![
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()),
            "-l".into(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> Profile {
        Profile {
            app: "Cophyla".into(),
            name: "Claude (tethered)".into(),
            command: vec![
                "claude".into(),
                "--settings".into(),
                r"C:\a b\s.json".into(),
            ],
            run_args: vec![],
        }
    }

    #[test]
    fn fills_the_templates_with_valid_json() {
        let wt: serde_json::Value =
            serde_json::from_str(&windows_terminal(r"C:\bin\tether.exe", &profile())).unwrap();
        let p = &wt["profiles"][0];
        assert_eq!(p["name"], "Claude (tethered)");
        let line = p["commandline"].as_str().unwrap();
        assert!(line.contains("run -- claude --settings"), "{line}");
        let it: serde_json::Value =
            serde_json::from_str(&iterm2("/usr/local/bin/tether", &profile())).unwrap();
        assert_eq!(it["Profiles"][0]["Guid"].as_str().unwrap().len(), 36);
        assert_eq!(guid("a", "b"), guid("a", "b"));
        assert_ne!(guid("a", "b"), guid("a", "c"));
        assert_eq!(file_name(&profile()), "claude--tethered.json");
    }
}
