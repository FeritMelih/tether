//! The tether host. `start` binds the user's endpoint, writes the discovery file, and serves
//! connections until the host is drained or idle; the `tether serve` command is a thin
//! wrapper around it, and tests run it in-process.

pub mod conn;
pub mod daemon;
pub mod host;
pub mod log;
pub mod transport;
#[cfg(windows)]
pub mod win;

pub use host::{start, version_cmp, Config, Host, Running, VERSION};
