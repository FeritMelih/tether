//! The host's endpoint, which only the user can reach. On Windows: a named pipe with a random
//! name, a DACL that grants the user's SID alone and refuses network logons, remote clients
//! rejected, and the SID of each connecting process checked again. On macOS and Linux: a Unix
//! socket in a directory only the user can enter, the socket itself 0600, and each peer's uid
//! checked. Either way the host never listens on a network.

use std::io;
use tokio::io::{AsyncRead, AsyncWrite};

pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// A connection, and the process on the other end as the operating system reports it.
pub struct Accepted {
    pub stream: Box<dyn Stream>,
    pub pid: Option<u32>,
}

#[cfg(windows)]
pub use windows::Listener;

#[cfg(unix)]
pub use unix::Listener;

#[cfg(windows)]
mod windows {
    use super::*;
    use crate::win;
    use std::os::windows::io::AsRawHandle;
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;

    pub struct Listener {
        pub endpoint: String,
        sid: String,
        attrs: *mut SECURITY_ATTRIBUTES,
        next: Option<NamedPipeServer>,
    }

    // The security attributes are read-only after creation and outlive the listener.
    unsafe impl Send for Listener {}

    impl Listener {
        /// A pipe named `\\.\pipe\<name>`.
        pub fn bind(name: &str) -> io::Result<Self> {
            let endpoint = format!(r"\\.\pipe\{name}");
            let sid = win::current_user_sid()?;
            let attrs = win::user_only_attributes(&sid)?;
            let first = Self::create(&endpoint, attrs, true)?;
            Ok(Self {
                endpoint,
                sid,
                attrs,
                next: Some(first),
            })
        }

        fn create(
            endpoint: &str,
            attrs: *mut SECURITY_ATTRIBUTES,
            first: bool,
        ) -> io::Result<NamedPipeServer> {
            let mut opts = ServerOptions::new();
            opts.first_pipe_instance(first).reject_remote_clients(true);
            unsafe {
                opts.create_with_security_attributes_raw(endpoint, attrs as *mut std::ffi::c_void)
            }
        }

        pub async fn accept(&mut self) -> io::Result<Accepted> {
            loop {
                let server = match self.next.take() {
                    Some(s) => s,
                    None => Self::create(&self.endpoint, self.attrs, false)?,
                };
                server.connect().await?;
                // The next instance is ready before this one is handed off, so a client never
                // finds the name missing.
                self.next = Some(Self::create(&self.endpoint, self.attrs, false)?);
                let mut pid = 0u32;
                let pid =
                    (unsafe { GetNamedPipeClientProcessId(server.as_raw_handle() as _, &mut pid) }
                        != 0)
                        .then_some(pid);
                if let Some(p) = pid {
                    match win::process_sid(p) {
                        Ok(sid) if sid != self.sid => {
                            crate::log!("refused a client running as {sid} (pid {p})");
                            continue;
                        }
                        // A process of the user's that cannot be opened (elevated, say) got
                        // through the DACL, which is the check that counts.
                        _ => {}
                    }
                }
                return Ok(Accepted {
                    stream: Box::new(server),
                    pid,
                });
            }
        }
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use tokio::net::UnixListener;

    pub struct Listener {
        pub endpoint: String,
        listener: UnixListener,
        uid: u32,
    }

    /// The longest path a Unix socket takes, less its terminating NUL.
    const SOCKET_PATH_MAX: usize = if cfg!(target_os = "macos") { 103 } else { 107 };
    /// What follows the directory: `/tether-<32 hex>.sock`.
    const SOCKET_NAME_LEN: usize = 45;

    /// The user's directory under the temporary one, or under `/tmp` when that would leave no
    /// room for a socket's name, as macOS's per-user `TMPDIR` does.
    fn tmp_socket_dir(tmp: &Path, uid: u32) -> PathBuf {
        let dir = tmp.join(format!("tether-{uid}"));
        if dir.as_os_str().len() + SOCKET_NAME_LEN > SOCKET_PATH_MAX {
            PathBuf::from("/tmp").join(format!("tether-{uid}"))
        } else {
            dir
        }
    }

    /// The directory sockets go in: the runtime directory, else one of the user's own in the
    /// temporary directory. It must belong to the user and admit no one else.
    pub fn socket_dir() -> io::Result<PathBuf> {
        let uid = unsafe { libc::getuid() };
        let dir = match std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
            Some(d) => PathBuf::from(d).join("tether"),
            None => {
                let tmp = std::env::var_os("TMPDIR")
                    .filter(|d| !d.is_empty())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/tmp"));
                tmp_socket_dir(&tmp, uid)
            }
        };
        std::fs::create_dir_all(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        let meta = std::fs::metadata(&dir)?;
        if meta.uid() != uid || meta.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} is not the user's own", dir.display()),
            ));
        }
        Ok(dir)
    }

    impl Listener {
        /// A socket `<name>.sock` in the user's socket directory.
        pub fn bind(name: &str) -> io::Result<Self> {
            let path = socket_dir()?.join(format!("{name}.sock"));
            let _ = std::fs::remove_file(&path);
            let listener = UnixListener::bind(&path)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            Ok(Self {
                endpoint: path.to_string_lossy().to_string(),
                listener,
                uid: unsafe { libc::getuid() },
            })
        }

        pub async fn accept(&mut self) -> io::Result<Accepted> {
            loop {
                let (stream, _) = self.listener.accept().await?;
                let cred = stream.peer_cred()?;
                if cred.uid() != self.uid {
                    crate::log!("refused a client running as uid {}", cred.uid());
                    continue;
                }
                let pid = cred.pid().map(|p| p as u32);
                return Ok(Accepted {
                    stream: Box::new(stream),
                    pid,
                });
            }
        }

        pub fn remove(&self) {
            let _ = std::fs::remove_file(Path::new(&self.endpoint));
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_socket_under_a_long_tmpdir_goes_to_tmp_instead() {
            let long = Path::new("/var/folders/zz/zyxvpxvq6csfxvn_n0000000000000/T/a/deeper/one/");
            assert_eq!(tmp_socket_dir(long, 501), PathBuf::from("/tmp/tether-501"));
            assert_eq!(
                tmp_socket_dir(Path::new("/tmp"), 1000),
                PathBuf::from("/tmp/tether-1000")
            );
            let name = format!("tether-{}.sock", "0".repeat(32));
            assert_eq!(name.len() + 1, SOCKET_NAME_LEN);
        }
    }
}

#[cfg(unix)]
pub use unix::socket_dir;
