//! The host: its sessions by id and name, its connections and what each watches, and the
//! requests that are about the host rather than one session. It also decides when to go: once
//! draining with nothing left running, or idle (no session running, no connection) for as long
//! as it was told to wait.

use crate::conn;
use crate::transport::Listener;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tether_core::outbox::{ConnId, Outbox};
use tether_core::pty::PtySystem;
use tether_core::session::{Client, Session, SessionEvent, SessionOptions};
use tether_core::spawn;
use tether_proto::discovery::{self, HostFile};
use tether_proto::handshake::{hex, new_token, random_bytes};
use tether_proto::msg::{self, now_ms, Code, Error};
use tether_proto::{frame, PROTOCOL};
use tokio::sync::{mpsc, Notify};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct Config {
    /// The state directory: discovery files and logs.
    pub dir: PathBuf,
    /// Exit after this long with no session running and no connection; `None` never.
    pub idle_exit: Option<Duration>,
    /// How long an exited session is kept.
    pub retain: Duration,
    pub pty: Arc<dyn PtySystem>,
    /// Also log to stderr.
    pub foreground: bool,
}

struct ConnEntry {
    outbox: Outbox,
    /// Watching: every session (`Some(None)`), some (`Some(Some(ids))`), or none.
    watch: Option<Option<HashSet<String>>>,
}

pub struct Host {
    pub id: String,
    pub pid: u32,
    pub token: String,
    pub started_at: u64,
    dir: PathBuf,
    file: Mutex<HostFile>,
    pty: Arc<dyn PtySystem>,
    retain: Duration,
    sessions: Mutex<BTreeMap<String, Session>>,
    running: Mutex<HashSet<String>>,
    conns: Mutex<HashMap<ConnId, ConnEntry>>,
    next_conn: AtomicU64,
    next_stream: AtomicU32,
    draining: AtomicBool,
    events: mpsc::UnboundedSender<SessionEvent>,
    shutdown: Notify,
    stopping: AtomicBool,
}

/// A host that is up: its discovery file, and a task that ends when the host decides to.
pub struct Running {
    pub file: HostFile,
    pub host: Arc<Host>,
    pub done: tokio::task::JoinHandle<()>,
}

/// Semantic versions compared by their numbers; anything unparsable is oldest.
pub fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |v: &str| -> Vec<u64> {
        v.split(['.', '-', '+'])
            .take(3)
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    parse(a).cmp(&parse(b))
}

/// Binds, announces itself, and serves until it is time to go.
pub async fn start(config: Config) -> std::io::Result<Running> {
    discovery::ensure_dirs(&config.dir)?;
    let id = hex(&random_bytes::<8>());
    crate::log::open(
        &discovery::logs_dir(&config.dir).join(format!("{id}.log")),
        config.foreground,
    );
    let mut listener = Listener::bind(&format!("tether-{}", hex(&random_bytes::<16>())))?;
    let file = HostFile {
        host: id.clone(),
        pid: std::process::id(),
        version: VERSION.to_string(),
        protocol: PROTOCOL,
        endpoint: listener.endpoint.clone(),
        token: new_token(),
        started_at: now_ms(),
        draining: false,
    };
    let (etx, erx) = mpsc::unbounded_channel();
    let host = Arc::new(Host {
        id: id.clone(),
        pid: file.pid,
        token: file.token.clone(),
        started_at: file.started_at,
        dir: config.dir.clone(),
        file: Mutex::new(file.clone()),
        pty: config.pty,
        retain: config.retain,
        sessions: Mutex::new(BTreeMap::new()),
        running: Mutex::new(HashSet::new()),
        conns: Mutex::new(HashMap::new()),
        next_conn: AtomicU64::new(1),
        next_stream: AtomicU32::new(1),
        draining: AtomicBool::new(false),
        events: etx,
        shutdown: Notify::new(),
        stopping: AtomicBool::new(false),
    });
    discovery::write_host(&config.dir, &file)?;
    // New sessions come here unless a newer host is already taking them.
    let newer = discovery::read_current(&config.dir)
        .and_then(|c| discovery::read_host(&config.dir, &c))
        .filter(|h| h.host != id && !h.draining && version_cmp(&h.version, VERSION).is_gt());
    if newer.is_none() {
        discovery::write_current(&config.dir, &id)?;
    }
    crate::log!(
        "host {id} {VERSION} (protocol {PROTOCOL}) pid {} listening on {}",
        file.pid,
        file.endpoint
    );

    tokio::spawn(events(host.clone(), erx));
    let h = host.clone();
    let idle_exit = config.idle_exit;
    let done = tokio::spawn(async move {
        let accept = async {
            loop {
                match listener.accept().await {
                    Ok(a) => {
                        tokio::spawn(conn::serve(h.clone(), a.stream, a.pid));
                    }
                    Err(e) => {
                        crate::log!("accept failed: {e}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        };
        tokio::select! {
            _ = accept => {}
            _ = h.shutdown.notified() => {}
            _ = watch_lifetime(h.clone(), idle_exit) => {}
        }
        h.stopping.store(true, Ordering::Release);
        discovery::remove_host(&h.dir, &h.id);
        #[cfg(unix)]
        listener.remove();
        crate::log!("host {} exiting", h.id);
    });
    Ok(Running { file, host, done })
}

/// Resolves when the host should go: drained, or idle long enough.
async fn watch_lifetime(host: Arc<Host>, idle_exit: Option<Duration>) {
    let mut idle_since: Option<Instant> = None;
    loop {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let running = host.running.lock().unwrap().len();
        if host.draining.load(Ordering::Acquire) && running == 0 {
            crate::log!("drained");
            return;
        }
        let Some(limit) = idle_exit else { continue };
        let idle = running == 0 && host.conns.lock().unwrap().is_empty();
        match (idle, idle_since) {
            (false, _) => idle_since = None,
            (true, None) => idle_since = Some(Instant::now()),
            (true, Some(t)) if t.elapsed() >= limit => {
                crate::log!("idle for {}s", limit.as_secs());
                return;
            }
            _ => {}
        }
    }
}

/// Sessions' events to the connections watching them.
async fn events(host: Arc<Host>, mut rx: mpsc::UnboundedReceiver<SessionEvent>) {
    while let Some(e) = rx.recv().await {
        match e {
            SessionEvent::Event {
                session,
                body,
                skip,
            } => host.broadcast(&session, &body, &skip),
            SessionEvent::WindowsClosed { session } => {
                crate::log!("session {session}: its last window closed; ending it");
            }
            SessionEvent::Exited { session } => {
                host.running.lock().unwrap().remove(&session);
                crate::log!("session {session} exited");
            }
            SessionEvent::Gone { session } => {
                host.sessions.lock().unwrap().remove(&session);
                host.running.lock().unwrap().remove(&session);
            }
        }
    }
}

impl Host {
    pub fn stop(&self) {
        self.shutdown.notify_one();
    }

    fn broadcast(&self, session: &str, body: &Value, skip: &[ConnId]) {
        let bytes = frame::message(body);
        for (id, c) in self.conns.lock().unwrap().iter() {
            if skip.contains(id) {
                continue;
            }
            let wants = match &c.watch {
                None => false,
                Some(None) => true,
                Some(Some(ids)) => ids.contains(session),
            };
            if wants {
                c.outbox.send(bytes.clone());
            }
        }
    }

    pub fn register(&self, outbox: Outbox) -> ConnId {
        let id = outbox.conn;
        self.conns.lock().unwrap().insert(
            id,
            ConnEntry {
                outbox,
                watch: None,
            },
        );
        id
    }

    pub fn next_conn(&self) -> ConnId {
        self.next_conn.fetch_add(1, Ordering::Relaxed)
    }

    pub fn closed(&self, conn: ConnId) {
        self.conns.lock().unwrap().remove(&conn);
        let sessions: Vec<Session> = self.sessions.lock().unwrap().values().cloned().collect();
        for s in sessions {
            s.conn_closed(conn);
        }
    }

    pub fn welcome(&self) -> Value {
        json!({ "host": self.id, "version": VERSION, "protocol": PROTOCOL, "pid": self.pid, "caps": tether_proto::HOST_CAPS })
    }

    /// A session by id, else by name, a running one before an exited one.
    pub fn find(&self, key: &str) -> Result<Session, Error> {
        let sessions = self.sessions.lock().unwrap();
        if let Some(s) = sessions.get(key) {
            return Ok(s.clone());
        }
        let running = self.running.lock().unwrap();
        let mut named: Vec<&Session> = sessions
            .values()
            .filter(|s| s.name.as_deref() == Some(key))
            .collect();
        named.sort_by_key(|s| !running.contains(&s.id));
        named
            .first()
            .map(|s| (*s).clone())
            .ok_or_else(|| Error::new(Code::NotFound, format!("no session {key}")))
    }

    fn session_of(&self, body: &Value) -> Result<Session, Error> {
        let key = body["session"]
            .as_str()
            .ok_or_else(|| Error::bad("the request names no session"))?;
        self.find(key)
    }

    pub fn new_stream(&self) -> u32 {
        self.next_stream.fetch_add(1, Ordering::Relaxed)
    }

    fn info(&self) -> Value {
        json!({
            "host": self.id,
            "pid": self.pid,
            "version": VERSION,
            "protocol": PROTOCOL,
            "startedAt": self.started_at,
            "draining": self.draining.load(Ordering::Acquire),
            "sessions": self.running.lock().unwrap().len(),
            "clients": self.conns.lock().unwrap().len(),
        })
    }

    /// Answers a request that is not a subscription. `subscribe` is the connection's own,
    /// because the session answers it through the outbox.
    pub async fn request(&self, client: &Client, op: &str, body: Value) -> Result<Value, Error> {
        match op {
            "host" => Ok(self.info()),
            "ping" => Ok(json!({ "now": now_ms() })),
            "spawn" => self.spawn(body).await,
            "list" => {
                let want = body.get("labels").and_then(Value::as_object).cloned();
                let sessions: Vec<Session> =
                    self.sessions.lock().unwrap().values().cloned().collect();
                let mut out = Vec::new();
                for s in sessions {
                    let Ok(info) = s.request(client, "info", json!({})).await else {
                        continue;
                    };
                    if let Some(want) = &want {
                        if !want.iter().all(|(k, v)| info["labels"].get(k) == Some(v)) {
                            continue;
                        }
                    }
                    out.push(info);
                }
                Ok(json!({ "sessions": out }))
            }
            "watch" => {
                let ids = match body.get("sessions") {
                    None | Some(Value::Null) => None,
                    Some(Value::Array(a)) => Some(
                        a.iter()
                            .filter_map(|v| v.as_str())
                            .map(|k| self.find(k).map(|s| s.id).unwrap_or_else(|_| k.to_string()))
                            .collect(),
                    ),
                    Some(_) => return Err(Error::bad("sessions must be an array")),
                };
                if let Some(c) = self.conns.lock().unwrap().get_mut(&client.conn) {
                    c.watch = Some(ids);
                }
                Ok(json!({}))
            }
            "drain" => {
                if !self.draining.swap(true, Ordering::AcqRel) {
                    let file = {
                        let mut f = self.file.lock().unwrap();
                        f.draining = true;
                        f.clone()
                    };
                    let _ = discovery::write_host(&self.dir, &file);
                    crate::log!(
                        "draining: {} session(s) running",
                        self.running.lock().unwrap().len()
                    );
                    let bytes = frame::message(&msg::event(
                        "host.draining",
                        None,
                        json!({ "host": self.id }),
                    ));
                    for c in self.conns.lock().unwrap().values() {
                        c.outbox.send(bytes.clone());
                    }
                }
                Ok(json!({}))
            }
            _ => {
                let session = self.session_of(&body)?;
                session.request(client, op, body).await
            }
        }
    }

    async fn spawn(&self, body: Value) -> Result<Value, Error> {
        if self.draining.load(Ordering::Acquire) || self.stopping.load(Ordering::Acquire) {
            return Err(Error::new(
                Code::Draining,
                format!("host {} is draining and starts nothing new", self.id),
            ));
        }
        let spec = spawn::parse_spawn(&body).map_err(Error::bad)?;
        if let Some(name) = &spec.name {
            // Sessions before running, as everywhere, so two locks never wait on each other.
            let sessions = self.sessions.lock().unwrap();
            let running = self.running.lock().unwrap();
            if sessions
                .values()
                .any(|s| s.name.as_deref() == Some(name) && running.contains(&s.id))
            {
                return Err(Error::bad(format!(
                    "a running session is already named {name:?}"
                )));
            }
        }
        let id = hex(&random_bytes::<6>());
        let argv = spec.argv.clone();
        let keep = if spec.keep {
            ", kept without a window"
        } else {
            ""
        };
        let session = Session::start(SessionOptions {
            id: id.clone(),
            spec,
            pty: self.pty.clone(),
            events: self.events.clone(),
            retain: self.retain,
        })?;
        self.sessions
            .lock()
            .unwrap()
            .insert(id.clone(), session.clone());
        self.running.lock().unwrap().insert(id.clone());
        crate::log!(
            "session {id} started: {argv:?}, pid {:?}{keep}",
            session.pid
        );
        let observer = Client {
            conn: 0,
            pid: None,
            name: None,
        };
        if let Ok(info) = session.request(&observer, "info", json!({})).await {
            self.broadcast(
                &id,
                &msg::event("created", Some(&id), json!({ "info": info })),
                &[],
            );
        }
        let mut out = json!({ "session": id });
        if let Some(pid) = session.pid {
            out["pid"] = json!(pid);
        }
        Ok(out)
    }
}
