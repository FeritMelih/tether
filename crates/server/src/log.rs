//! The host's log: one line per event, in `logs/<host>.log` under the state directory, and on
//! stderr when the host runs in the foreground. A log past 5 MB is moved aside when the host
//! starts.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

static LOG: OnceLock<Mutex<Option<File>>> = OnceLock::new();
static STDERR: OnceLock<bool> = OnceLock::new();

const ROTATE_BYTES: u64 = 5 * 1024 * 1024;

pub fn open(path: &Path, stderr: bool) {
    if fs::metadata(path)
        .map(|m| m.len() > ROTATE_BYTES)
        .unwrap_or(false)
    {
        let _ = fs::rename(path, path.with_extension("log.old"));
    }
    let file = OpenOptions::new().create(true).append(true).open(path).ok();
    let _ = LOG.set(Mutex::new(file));
    let _ = STDERR.set(stderr);
}

/// `2026-09-23T07:00:00.123Z` from epoch milliseconds.
pub fn iso(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    // Civil date from days since the epoch (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        ms % 1000
    )
}

pub fn line(msg: &str) {
    let text = format!("{} {msg}\n", iso(tether_proto::msg::now_ms()));
    if let Some(m) = LOG.get() {
        if let Ok(mut f) = m.lock() {
            if let Some(f) = f.as_mut() {
                let _ = f.write_all(text.as_bytes());
            }
        }
    }
    if *STDERR.get().unwrap_or(&true) {
        eprint!("{text}");
    }
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::log::line(&format!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    #[test]
    fn formats_dates() {
        assert_eq!(super::iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(super::iso(1_790_150_400_123), "2026-09-23T08:00:00.123Z");
    }
}
