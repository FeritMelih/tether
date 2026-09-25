//! Where hosts announce themselves: a per-user state directory holding one file per running
//! host, and a pointer to the newest. The host writes its file once it is listening and
//! removes it on the way out; a file left by a host that died is found stale by the client
//! that fails to connect to it.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// What a host writes about itself: everything a client needs to reach it and prove itself.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HostFile {
    pub host: String,
    pub pid: u32,
    pub version: String,
    pub protocol: u32,
    /// A named pipe path on Windows, a socket path elsewhere.
    pub endpoint: String,
    pub token: String,
    pub started_at: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub draining: bool,
}

/// The state directory: `TETHER_DIR`, else the platform's per-user place for state.
pub fn state_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("TETHER_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    platform_dir().join("tether")
}

#[cfg(windows)]
fn platform_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("AppData").join("Local"))
}

#[cfg(target_os = "macos")]
fn platform_dir() -> PathBuf {
    home().join("Library").join("Application Support")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local").join("state"))
}

fn home() -> PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn hosts_dir(dir: &Path) -> PathBuf {
    dir.join("hosts")
}

pub fn logs_dir(dir: &Path) -> PathBuf {
    dir.join("logs")
}

pub fn host_path(dir: &Path, host: &str) -> PathBuf {
    hosts_dir(dir).join(format!("{host}.json"))
}

/// Every host file that parses, newest first.
pub fn read_hosts(dir: &Path) -> Vec<HostFile> {
    let Ok(entries) = fs::read_dir(hosts_dir(dir)) else {
        return Vec::new();
    };
    let mut out: Vec<HostFile> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| fs::read(e.path()).ok())
        .filter_map(|b| serde_json::from_slice::<HostFile>(&b).ok())
        .collect();
    out.sort_by_key(|h| std::cmp::Reverse(h.started_at));
    out
}

pub fn read_host(dir: &Path, host: &str) -> Option<HostFile> {
    serde_json::from_slice(&fs::read(host_path(dir, host)).ok()?).ok()
}

/// The host new sessions should go to, as the newest host recorded it.
pub fn read_current(dir: &Path) -> Option<String> {
    let s = fs::read_to_string(dir.join("current")).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// Writes through a temporary file and a rename, so a reader never sees half a file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        use std::io::Write;
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

pub fn write_host(dir: &Path, file: &HostFile) -> io::Result<()> {
    write_atomic(
        &host_path(dir, &file.host),
        &serde_json::to_vec_pretty(file).expect("a host file serializes"),
    )
}

pub fn write_current(dir: &Path, host: &str) -> io::Result<()> {
    write_atomic(&dir.join("current"), host.as_bytes())
}

pub fn remove_host(dir: &Path, host: &str) {
    let _ = fs::remove_file(host_path(dir, host));
    if read_current(dir).as_deref() == Some(host) {
        let _ = fs::remove_file(dir.join("current"));
    }
}

/// The directory and its `hosts/` and `logs/`, readable by the user alone where the platform
/// has modes; on Windows the per-user local application data folder already is.
pub fn ensure_dirs(dir: &Path) -> io::Result<()> {
    for d in [dir.to_path_buf(), hosts_dir(dir), logs_dir(dir)] {
        fs::create_dir_all(&d)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&d, fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_reads_and_removes() {
        let dir = std::env::temp_dir().join(format!("tether-disc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        ensure_dirs(&dir).unwrap();
        let a = HostFile {
            host: "a".into(),
            pid: 1,
            version: "0.1.0".into(),
            protocol: 1,
            endpoint: "e".into(),
            token: "t".into(),
            started_at: 1,
            draining: false,
        };
        let b = HostFile {
            host: "b".into(),
            started_at: 2,
            draining: true,
            ..a.clone()
        };
        write_host(&dir, &a).unwrap();
        write_host(&dir, &b).unwrap();
        write_current(&dir, "b").unwrap();
        assert_eq!(read_hosts(&dir), vec![b.clone(), a.clone()]);
        assert_eq!(read_current(&dir).as_deref(), Some("b"));
        assert_eq!(read_host(&dir, "a"), Some(a));
        remove_host(&dir, "b");
        assert_eq!(read_current(&dir), None);
        assert_eq!(read_hosts(&dir).len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
