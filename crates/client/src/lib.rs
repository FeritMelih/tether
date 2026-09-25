//! A client for tether hosts. `Client::connect` proves the token to a host (and has the host
//! prove it back), then carries requests, events and subscriptions over one connection;
//! `connect_or_start` finds the host new sessions should go to, starting one when there is
//! none and telling an older one to drain.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tether_proto::discovery::{self, HostFile};
use tether_proto::frame::{self, Decoder, Frame};
use tether_proto::handshake::{self, hex, unhex, CLIENT_LABEL, HOST_LABEL};
use tether_proto::msg::{self, Error, Incoming};
use tether_proto::PROTOCOL;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

pub use tether_proto::discovery::state_dir;
pub use tether_proto::msg::Code;

pub type Result<T> = std::result::Result<T, Error>;

/// A failure on this side of the connection: the host is unreachable or broke the protocol.
pub fn unavailable(message: impl Into<String>) -> Error {
    Error {
        code: "unavailable".into(),
        message: message.into(),
    }
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

#[cfg(windows)]
async fn open(endpoint: &str) -> std::io::Result<Box<dyn Io>> {
    use tokio::net::windows::named_pipe::ClientOptions;
    const ERROR_PIPE_BUSY: i32 = 231;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match ClientOptions::new().open(endpoint) {
            Ok(c) => return Ok(Box::new(c)),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(unix)]
async fn open(endpoint: &str) -> std::io::Result<Box<dyn Io>> {
    Ok(Box::new(tokio::net::UnixStream::connect(endpoint).await?))
}

/// What arrives on a subscription.
#[derive(Debug)]
pub enum StreamItem {
    Output { seq: u64, data: Vec<u8> },
    Resync { seq: u64, data: Vec<u8> },
}

struct Pending {
    reply: oneshot::Sender<(Result<Value>, Option<mpsc::UnboundedReceiver<StreamItem>>)>,
    subscribe: bool,
}

struct Shared {
    pending: Mutex<HashMap<u64, Pending>>,
    streams: Mutex<HashMap<u32, mpsc::UnboundedSender<StreamItem>>>,
    events: Mutex<Option<mpsc::UnboundedSender<Value>>>,
}

#[derive(Clone)]
pub struct Client {
    pub host: HostFile,
    pub welcome: Value,
    out: mpsc::UnboundedSender<Vec<u8>>,
    shared: Arc<Shared>,
    next: Arc<AtomicU64>,
    closed: Arc<tokio::sync::Notify>,
    _reader: Arc<Reader>,
}

/// The task reading the connection, stopped when the last handle on the client goes, which
/// closes the connection: a reader left waiting would hold it open for good.
struct Reader(tokio::task::AbortHandle);

impl Drop for Reader {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub struct Subscription {
    pub stream: u32,
    pub seq: u64,
    pub cols: u16,
    pub rows: u16,
    pub rx: mpsc::UnboundedReceiver<StreamItem>,
}

async fn read_frame(
    io: &mut (impl AsyncRead + Unpin),
    dec: &mut Decoder,
    buf: &mut [u8],
) -> Result<Frame> {
    loop {
        if let Some(f) = dec.next_frame().map_err(|e| unavailable(e.to_string()))? {
            return Ok(f);
        }
        let n = io.read(buf).await.map_err(|e| unavailable(e.to_string()))?;
        if n == 0 {
            return Err(unavailable("the host closed the connection"));
        }
        dec.push(&buf[..n]);
    }
}

async fn read_handshake(
    io: &mut (impl AsyncRead + Unpin),
    dec: &mut Decoder,
    buf: &mut [u8],
) -> Result<Value> {
    match read_frame(io, dec, buf).await? {
        Frame::Handshake(b) => {
            let v: Value = serde_json::from_slice(&b).map_err(|e| unavailable(e.to_string()))?;
            if let Some(e) = v.get("error") {
                return Err(Error::from_value(e));
            }
            Ok(v)
        }
        _ => Err(unavailable("expected a handshake frame")),
    }
}

impl Client {
    /// Connects to a host and proves the token, `name` saying who this client is.
    pub async fn connect(host: &HostFile, name: &str) -> Result<Client> {
        let mut io = open(&host.endpoint)
            .await
            .map_err(|e| unavailable(format!("{}: {e}", host.endpoint)))?;
        let mut dec = Decoder::new();
        let mut buf = vec![0u8; 64 * 1024];
        let client_nonce = handshake::nonce();
        let hello = json!({
            "hello": "tether",
            "protocol": [PROTOCOL, PROTOCOL],
            "client": { "name": name, "version": env!("CARGO_PKG_VERSION"), "pid": std::process::id() },
            "caps": [],
            "nonce": hex(&client_nonce),
        });
        io.write_all(&frame::handshake(&hello))
            .await
            .map_err(|e| unavailable(e.to_string()))?;
        let challenge = read_handshake(&mut io, &mut dec, &mut buf).await?;
        let host_nonce = challenge["nonce"]
            .as_str()
            .and_then(unhex)
            .ok_or_else(|| unavailable("the host sent no nonce"))?;
        let proof = challenge["proof"]
            .as_str()
            .and_then(unhex)
            .unwrap_or_default();
        if !handshake::verify(&host.token, HOST_LABEL, &client_nonce, &host_nonce, &proof) {
            return Err(Error {
                code: "auth".into(),
                message: format!("{} did not prove it holds the token", host.endpoint),
            });
        }
        let mine = handshake::proof(&host.token, CLIENT_LABEL, &client_nonce, &host_nonce);
        io.write_all(&frame::handshake(&json!({ "proof": hex(&mine) })))
            .await
            .map_err(|e| unavailable(e.to_string()))?;
        let welcome = read_handshake(&mut io, &mut dec, &mut buf).await?["welcome"].clone();

        let (mut rd, mut wr) = tokio::io::split(io);
        let (out, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            while let Some(b) = out_rx.recv().await {
                if wr.write_all(&b).await.is_err() {
                    break;
                }
            }
        });
        let shared = Arc::new(Shared {
            pending: Mutex::new(HashMap::new()),
            streams: Mutex::new(HashMap::new()),
            events: Mutex::new(None),
        });
        let closed = Arc::new(tokio::sync::Notify::new());
        let (s, c) = (shared.clone(), closed.clone());
        let reader = tokio::spawn(async move {
            loop {
                let f = match read_frame(&mut rd, &mut dec, &mut buf).await {
                    Ok(f) => f,
                    Err(_) => break,
                };
                match f {
                    Frame::Message(b) => {
                        let Ok(v) = serde_json::from_slice::<Value>(&b) else {
                            continue;
                        };
                        match msg::classify(v) {
                            Some(Incoming::Response { re, result }) => {
                                let Some(p) = s.pending.lock().unwrap().remove(&re) else {
                                    continue;
                                };
                                // A stream's channel exists before its first frame is read.
                                let rx = match (&result, p.subscribe) {
                                    (Ok(r), true) => {
                                        let (tx, rx) = mpsc::unbounded_channel();
                                        s.streams
                                            .lock()
                                            .unwrap()
                                            .insert(r["stream"].as_u64().unwrap_or(0) as u32, tx);
                                        Some(rx)
                                    }
                                    _ => None,
                                };
                                let _ = p.reply.send((result, rx));
                            }
                            Some(Incoming::Event { body, .. }) => {
                                if let Some(tx) = s.events.lock().unwrap().as_ref() {
                                    let _ = tx.send(body);
                                }
                            }
                            _ => {}
                        }
                    }
                    Frame::Output { stream, seq, data } => {
                        if let Some(tx) = s.streams.lock().unwrap().get(&stream) {
                            let _ = tx.send(StreamItem::Output { seq, data });
                        }
                    }
                    Frame::Resync { stream, seq, data } => {
                        if let Some(tx) = s.streams.lock().unwrap().get(&stream) {
                            let _ = tx.send(StreamItem::Resync { seq, data });
                        }
                    }
                    _ => {}
                }
            }
            // Everything waiting learns the host is gone.
            for (_, p) in s.pending.lock().unwrap().drain() {
                let _ = p
                    .reply
                    .send((Err(unavailable("the host closed the connection")), None));
            }
            s.streams.lock().unwrap().clear();
            s.events.lock().unwrap().take();
            c.notify_waiters();
            c.notify_one();
        });
        Ok(Client {
            host: host.clone(),
            welcome,
            out,
            shared,
            next: Arc::new(AtomicU64::new(1)),
            closed,
            _reader: Arc::new(Reader(reader.abort_handle())),
        })
    }

    async fn call(
        &self,
        op: &str,
        fields: Value,
        subscribe: bool,
    ) -> (Result<Value>, Option<mpsc::UnboundedReceiver<StreamItem>>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (reply, rx) = oneshot::channel();
        self.shared
            .pending
            .lock()
            .unwrap()
            .insert(id, Pending { reply, subscribe });
        if self
            .out
            .send(frame::message(&msg::request(id, op, fields)))
            .is_err()
        {
            self.shared.pending.lock().unwrap().remove(&id);
            return (Err(unavailable("the connection is closed")), None);
        }
        rx.await
            .unwrap_or_else(|_| (Err(unavailable("the connection is closed")), None))
    }

    pub async fn request(&self, op: &str, fields: Value) -> Result<Value> {
        self.call(op, fields, false).await.0
    }

    pub async fn subscribe(&self, fields: Value) -> Result<Subscription> {
        let (result, rx) = self.call("subscribe", fields, true).await;
        let r = result?;
        Ok(Subscription {
            stream: r["stream"].as_u64().unwrap_or(0) as u32,
            seq: r["seq"].as_u64().unwrap_or(0),
            cols: r["cols"].as_u64().unwrap_or(0) as u16,
            rows: r["rows"].as_u64().unwrap_or(0) as u16,
            rx: rx.ok_or_else(|| unavailable("no stream"))?,
        })
    }

    /// Events from the host, from now on; one receiver per client.
    pub fn events(&self) -> mpsc::UnboundedReceiver<Value> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.shared.events.lock().unwrap() = Some(tx);
        rx
    }

    /// Types into a subscription opened with `input`.
    pub fn input(&self, stream: u32, data: &[u8]) -> bool {
        self.out.send(frame::input(stream, data)).is_ok()
    }

    /// Resolves when the connection closes.
    pub async fn closed(&self) {
        if self.out.is_closed() {
            return;
        }
        self.closed.notified().await
    }
}

/// Whether a process is running.
pub fn process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::{
            CloseHandle, GetLastError, ERROR_ACCESS_DENIED, STILL_ACTIVE,
        };
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        ok != 0 && code == STILL_ACTIVE as u32
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as libc::pid_t, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

/// Every host whose process is alive, newest first; files left by dead hosts are removed.
pub fn live_hosts(dir: &Path) -> Vec<HostFile> {
    discovery::read_hosts(dir)
        .into_iter()
        .filter(|h| {
            let alive = process_alive(h.pid);
            if !alive {
                discovery::remove_host(dir, &h.host);
            }
            alive
        })
        .collect()
}

/// The host new sessions should go to: `current`, else the newest live host not draining.
pub fn current_host(dir: &Path) -> Option<HostFile> {
    let hosts = live_hosts(dir);
    let current = discovery::read_current(dir);
    hosts
        .iter()
        .find(|h| Some(&h.host) == current.as_ref() && !h.draining)
        .or_else(|| hosts.iter().find(|h| !h.draining))
        .cloned()
}

fn version_newer(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.split(['.', '-', '+'])
            .take(3)
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    parse(a) > parse(b)
}

/// The exclusive right to start a host, held while the file exists; a lock older than 10 s
/// was left by a starter that died.
struct StartLock(PathBuf);

impl StartLock {
    async fn take(dir: &Path) -> Result<StartLock> {
        // The first start on a machine finds no state directory yet.
        discovery::ensure_dirs(dir).map_err(|e| unavailable(format!("{}: {e}", dir.display())))?;
        let path = dir.join("start.lock");
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(StartLock(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .map(|t| {
                            SystemTime::now().duration_since(t).unwrap_or_default()
                                > Duration::from_secs(10)
                        })
                        .unwrap_or(true);
                    if stale {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if Instant::now() >= deadline {
                        return Err(unavailable(format!("{} is held", path.display())));
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => return Err(unavailable(format!("{}: {e}", path.display()))),
            }
        }
    }
}

impl Drop for StartLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub struct StartOptions<'a> {
    /// The `tether` binary to start a host with.
    pub exe: &'a Path,
    pub dir: &'a Path,
    /// Who this client is, for the host's client list.
    pub name: &'a str,
    /// Passed to `serve` as `--idle-exit`.
    pub idle_exit: Option<u64>,
}

/// Runs `exe serve --daemonize` and returns the host file it prints.
pub async fn start_host(opts: &StartOptions<'_>) -> Result<HostFile> {
    let mut cmd = tokio::process::Command::new(opts.exe);
    cmd.arg("serve")
        .arg("--daemonize")
        .env("TETHER_DIR", opts.dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(s) = opts.idle_exit {
        cmd.arg("--idle-exit").arg(s.to_string());
    }
    let out = cmd
        .output()
        .await
        .map_err(|e| unavailable(format!("{}: {e}", opts.exe.display())))?;
    if !out.status.success() {
        return Err(unavailable(format!(
            "the host did not start: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    serde_json::from_slice(&out.stdout)
        .map_err(|e| unavailable(format!("the host's announcement did not parse: {e}")))
}

/// The host new sessions go to, started when there is none or the current one is older than
/// `exe`; an older one is then told to drain, keeping its sessions until they end.
pub async fn connect_or_start(opts: &StartOptions<'_>) -> Result<Client> {
    let mine = exe_version(opts.exe).await;
    let outdated = |h: &HostFile| {
        mine.as_deref()
            .is_some_and(|v| version_newer(v, &h.version))
    };
    if let Some(c) = connect_current(opts, &outdated).await {
        return Ok(c);
    }
    let _lock = StartLock::take(opts.dir).await?;
    // Someone else may have started one while this waited for the lock.
    if let Some(c) = connect_current(opts, &outdated).await {
        return Ok(c);
    }
    let older: Vec<HostFile> = live_hosts(opts.dir)
        .into_iter()
        .filter(|h| !h.draining && outdated(h))
        .collect();
    let started = start_host(opts).await?;
    let client = Client::connect(&started, opts.name).await?;
    for h in older.iter().filter(|h| h.host != started.host) {
        if let Ok(old) = Client::connect(h, opts.name).await {
            let _ = old.request("drain", json!({})).await;
        }
    }
    Ok(client)
}

/// The current host, unless it is outdated or cannot be reached.
async fn connect_current(
    opts: &StartOptions<'_>,
    outdated: &impl Fn(&HostFile) -> bool,
) -> Option<Client> {
    let h = current_host(opts.dir).filter(|h| !outdated(h))?;
    Client::connect(&h, opts.name).await.ok()
}

/// What `exe --version` says, its last word.
async fn exe_version(exe: &Path) -> Option<String> {
    let out = tokio::process::Command::new(exe)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .last()
        .map(String::from)
}

/// The host holding a session (by id or name), current host first, with the session's info.
pub async fn find_session(dir: &Path, name: &str, key: &str) -> Result<(Client, Value)> {
    let mut hosts = live_hosts(dir);
    let current = discovery::read_current(dir);
    hosts.sort_by_key(|h| Some(&h.host) != current.as_ref());
    for h in hosts {
        let Ok(c) = Client::connect(&h, name).await else {
            continue;
        };
        if let Ok(info) = c.request("info", json!({ "session": key })).await {
            return Ok((c, info));
        }
    }
    Err(Error::new(
        Code::NotFound,
        format!("no session {key} on any host"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_start_lock_is_taken_in_a_state_directory_not_made_yet() {
        let dir = std::env::temp_dir()
            .join(format!("tether-client-{}", std::process::id()))
            .join("state");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
        let lock = StartLock::take(&dir).await.unwrap();
        assert!(dir.join("start.lock").exists());
        drop(lock);
        assert!(!dir.join("start.lock").exists());
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }
}
