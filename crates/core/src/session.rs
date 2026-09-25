//! One session: a program in a pseudo-terminal and the task that owns everything about it.
//! Three threads sit on the blocking handles (reading output, writing input, waiting for the
//! exit) and hand what they get to the task as messages; so do connections. The task feeds
//! output to the screen model and the ring, answers the terminal queries the host answers,
//! fans the rest out to subscribers within their budgets, and settles who sizes the terminal.
//! Nothing a client does can make it wait: a subscriber that cannot keep up is dropped from
//! the stream until it drains, then given the screen afresh.

use crate::outbox::{Budget, ConnId, Outbox};
use crate::pty::{Control, Exit, PtySystem, Signal};
use crate::queries::{self, Piece, Scanner};
use crate::ring::Ring;
use crate::spawn::{self, SpawnSpec};
use crate::{keys, screen};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;
use tether_proto::frame;
use tether_proto::msg::{self, now_ms, Code, Error};
use tokio::sync::{mpsc, oneshot};

/// Raw output kept for subscribers that resume from a `seq`.
const RING_BYTES: usize = 1024 * 1024;
/// A subscription's queued output before it counts as lagging.
pub const DEFAULT_BUDGET: usize = 1024 * 1024;
/// How long a partial terminal query is held back before it is passed on as plain output.
const HELD_QUERY_MS: u64 = 30;
/// After the program exits, when the terminal is let go of. ConPTY ends the output only then;
/// elsewhere the output ends by itself, and this is the backstop for a descendant that kept
/// the terminal open.
const CLOSE_AFTER_EXIT: Duration = if cfg!(windows) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(2)
};
/// After the terminal is let go of, when the session counts as exited whether or not the
/// output has ended.
const FINISH_AFTER_CLOSE: Duration = Duration::from_secs(1);
/// Input queued for the writer thread before a write is refused.
const INPUT_QUEUE: usize = 256;
/// Outside Windows, how long a program has between the hangup that ends it and the kill.
const KILL_GRACE: Duration = Duration::from_secs(2);

pub struct SessionOptions {
    pub id: String,
    pub spec: SpawnSpec,
    pub pty: Arc<dyn PtySystem>,
    /// Where the session reports to its host.
    pub events: mpsc::UnboundedSender<SessionEvent>,
    /// How long an exited session is kept, with its last screen.
    pub retain: Duration,
}

/// What a session tells its host.
pub enum SessionEvent {
    /// An event for watchers; `skip` names connections that already have it through a stream.
    Event {
        session: String,
        body: Value,
        skip: Vec<ConnId>,
    },
    /// The last window on the session closed, and the program is being ended with it.
    WindowsClosed { session: String },
    /// The program exited and the session's output is complete.
    Exited { session: String },
    /// The session is gone: removed, or kept past its time.
    Gone { session: String },
}

/// Who a connection is, as far as a session reports it.
#[derive(Clone, Debug)]
pub struct Client {
    pub conn: ConnId,
    pub pid: Option<u32>,
    pub name: Option<String>,
}

impl Client {
    pub fn id(&self) -> String {
        format!("c{}", self.conn)
    }
}

enum Cmd {
    Output(Vec<u8>),
    OutputEnd,
    Exited(Exit),
    Close,
    Finish,
    FlushHeld,
    KillNow,
    Expire,
    Request {
        client: Client,
        op: String,
        body: Value,
        reply: oneshot::Sender<Result<Value, Error>>,
    },
    Subscribe {
        client: Client,
        outbox: Outbox,
        re: u64,
        body: Value,
        stream: u32,
    },
    Input {
        conn: ConnId,
        stream: u32,
        data: Vec<u8>,
    },
    Drained(u32),
    ConnClosed(ConnId),
}

/// A handle on a session's task. Cheap to clone; every method is a message.
#[derive(Clone)]
pub struct Session {
    pub id: String,
    pub name: Option<String>,
    pub pid: Option<u32>,
    tx: mpsc::UnboundedSender<Cmd>,
}

impl Session {
    /// Starts the program and the session's task. Must be called inside a tokio runtime.
    pub fn start(opts: SessionOptions) -> Result<Session, Error> {
        let SessionOptions {
            id,
            spec,
            pty,
            events,
            retain,
        } = opts;
        let env = spawn::build_env(&spec.env, &id);
        let argv = spawn::resolve(&spec.argv, &env, spec.cwd.as_deref())
            .map_err(|e| Error::new(Code::SpawnFailed, e))?;
        let opened = pty
            .open(&argv, spec.cwd.as_deref(), &env, spec.cols, spec.rows)
            .map_err(|e| Error::new(Code::SpawnFailed, format!("{}: {e}", spec.argv[0])))?;
        let (tx, rx) = mpsc::unbounded_channel();

        let mut reader = opened.reader;
        let out = tx.clone();
        std::thread::Builder::new()
            .name(format!("tether-read-{id}"))
            .spawn(move || {
                let mut buf = vec![0u8; 16 * 1024];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if out.send(Cmd::Output(buf[..n].to_vec())).is_err() {
                                return;
                            }
                        }
                    }
                }
                let _ = out.send(Cmd::OutputEnd);
            })
            .map_err(|e| Error::new(Code::Internal, e.to_string()))?;

        let (input_tx, input_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(INPUT_QUEUE);
        let mut writer = opened.writer;
        std::thread::Builder::new()
            .name(format!("tether-write-{id}"))
            .spawn(move || {
                for bytes in input_rx {
                    if writer
                        .write_all(&bytes)
                        .and_then(|_| writer.flush())
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .map_err(|e| Error::new(Code::Internal, e.to_string()))?;

        let child = opened.child;
        let exit = tx.clone();
        std::thread::Builder::new()
            .name(format!("tether-wait-{id}"))
            .spawn(move || {
                let _ = exit.send(Cmd::Exited(child.wait()));
            })
            .map_err(|e| Error::new(Code::Internal, e.to_string()))?;

        let cwd = spec
            .cwd
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let handle = Session {
            id: id.clone(),
            name: spec.name.clone(),
            pid: opened.pid,
            tx: tx.clone(),
        };
        let actor = Actor {
            id,
            name: spec.name.clone(),
            argv: spec.argv.clone(),
            cwd,
            labels: spec.labels.iter().cloned().collect(),
            keep: spec.keep,
            started_at: now_ms(),
            pid: opened.pid,
            parser: vt100::Parser::new_with_callbacks(
                spec.rows,
                spec.cols,
                spec.scrollback,
                Callbacks::default(),
            ),
            scanner: Scanner::default(),
            ring: Ring::new(RING_BYTES),
            seq: 0,
            cols: spec.cols,
            rows: spec.rows,
            input: Some(input_tx),
            control: Some(opened.control),
            subs: Vec::new(),
            tick: 0,
            sized_by: None,
            exit: None,
            eof: false,
            finished: false,
            exited_at: None,
            flush_pending: false,
            events,
            tx,
            retain,
        };
        tokio::spawn(actor.run(rx));
        Ok(handle)
    }

    /// A request about this session; the answer, or `not_found` when the session is gone.
    pub async fn request(&self, client: &Client, op: &str, body: Value) -> Result<Value, Error> {
        let (reply, rx) = oneshot::channel();
        if self
            .tx
            .send(Cmd::Request {
                client: client.clone(),
                op: op.to_string(),
                body,
                reply,
            })
            .is_err()
        {
            return Err(Error::new(
                Code::NotFound,
                format!("no session {}", self.id),
            ));
        }
        rx.await.unwrap_or_else(|_| {
            Err(Error::new(
                Code::NotFound,
                format!("no session {}", self.id),
            ))
        })
    }

    /// Subscribes a connection. The session answers `re` itself, through the outbox, so the
    /// response goes out before the stream's first frame.
    pub fn subscribe(
        &self,
        client: Client,
        outbox: Outbox,
        re: u64,
        body: Value,
        stream: u32,
    ) -> bool {
        self.tx
            .send(Cmd::Subscribe {
                client,
                outbox,
                re,
                body,
                stream,
            })
            .is_ok()
    }

    pub fn input(&self, conn: ConnId, stream: u32, data: Vec<u8>) {
        let _ = self.tx.send(Cmd::Input { conn, stream, data });
    }

    pub fn conn_closed(&self, conn: ConnId) {
        let _ = self.tx.send(Cmd::ConnClosed(conn));
    }

    pub fn gone(&self) -> bool {
        self.tx.is_closed()
    }
}

#[derive(Default)]
struct Callbacks {
    title: Option<String>,
    title_changed: bool,
    bells: u32,
    cwd: Option<String>,
    cwd_changed: bool,
}

impl vt100::Callbacks for Callbacks {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = Some(String::from_utf8_lossy(title).to_string());
        self.title_changed = true;
    }

    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.bells += 1;
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        if params.first() == Some(&b"7".as_slice()) && params.len() > 1 {
            let url = params[1..]
                .iter()
                .map(|p| String::from_utf8_lossy(p))
                .collect::<Vec<_>>()
                .join(";");
            if let Some(path) = file_url_path(&url) {
                self.cwd = Some(path);
                self.cwd_changed = true;
            }
        }
    }
}

/// The path an OSC 7 `file://host/path` names, percent-decoded; a Windows drive path loses
/// the slash before its letter.
fn file_url_path(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    let path = &rest[rest.find('/')?..];
    let mut bytes = Vec::with_capacity(path.len());
    let raw = path.as_bytes();
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' && i + 2 < raw.len() {
            if let (Some(hi), Some(lo)) = (hex(raw[i + 1]), hex(raw[i + 2])) {
                bytes.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        bytes.push(raw[i]);
        i += 1;
    }
    let mut s = String::from_utf8_lossy(&bytes).to_string();
    let b = s.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        s.remove(0);
    }
    Some(s)
}

struct Sub {
    stream: u32,
    client: Client,
    outbox: Outbox,
    budget: Arc<Budget>,
    input: bool,
    sizing: bool,
    size: Option<(u16, u16)>,
    role: String,
    scrollback: bool,
    attached_at: u64,
    last_input: Option<u64>,
    /// When it last subscribed, typed or resized, as the session's own counter.
    active: u64,
}

impl Sub {
    fn info(&self) -> Value {
        let mut v = json!({
            "client": self.client.id(),
            "stream": self.stream,
            "role": self.role,
            "input": self.input,
            "sizing": self.sizing,
            "attachedAt": self.attached_at,
        });
        if let Some(pid) = self.client.pid {
            v["pid"] = json!(pid);
        }
        if let Some(name) = &self.client.name {
            v["name"] = json!(name);
        }
        if let Some((cols, rows)) = self.size {
            v["cols"] = json!(cols);
            v["rows"] = json!(rows);
        }
        if let Some(t) = self.last_input {
            v["lastInput"] = json!(t);
        }
        v
    }
}

enum From {
    Snapshot,
    Now,
    Seq(u64),
}

struct Actor {
    id: String,
    name: Option<String>,
    argv: Vec<String>,
    cwd: String,
    labels: BTreeMap<String, String>,
    /// The program runs on when its last window closes.
    keep: bool,
    started_at: u64,
    pid: Option<u32>,
    parser: vt100::Parser<Callbacks>,
    scanner: Scanner,
    ring: Ring,
    seq: u64,
    cols: u16,
    rows: u16,
    input: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    control: Option<Box<dyn Control>>,
    subs: Vec<Sub>,
    tick: u64,
    /// The sizing subscription whose size the terminal has; none after an explicit resize.
    sized_by: Option<u32>,
    exit: Option<Exit>,
    eof: bool,
    finished: bool,
    exited_at: Option<u64>,
    flush_pending: bool,
    events: mpsc::UnboundedSender<SessionEvent>,
    tx: mpsc::UnboundedSender<Cmd>,
    retain: Duration,
}

impl Actor {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>) {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                Cmd::Output(bytes) => self.output(&bytes),
                Cmd::OutputEnd => {
                    self.eof = true;
                    if self.exit.is_some() {
                        self.finish();
                    }
                }
                Cmd::Exited(exit) => {
                    self.exit = Some(exit);
                    if self.eof {
                        self.finish();
                    } else {
                        self.later(CLOSE_AFTER_EXIT, Cmd::Close);
                    }
                }
                Cmd::Close => {
                    self.close();
                    if !self.finished {
                        self.later(FINISH_AFTER_CLOSE, Cmd::Finish);
                    }
                }
                Cmd::Finish => self.finish(),
                Cmd::FlushHeld => {
                    self.flush_pending = false;
                    let held = self.scanner.flush();
                    if !held.is_empty() {
                        self.pass(held);
                    }
                }
                Cmd::KillNow => {
                    if !self.finished {
                        if let Some(c) = self.control.as_mut() {
                            let _ = c.signal(Signal::Kill);
                        }
                    }
                }
                Cmd::Expire => {
                    self.gone();
                    return;
                }
                Cmd::Request {
                    client,
                    op,
                    body,
                    reply,
                } => {
                    let removing = op == "remove";
                    let result = self.request(&client, &op, &body);
                    let removed = removing && result.is_ok();
                    let _ = reply.send(result);
                    if removed {
                        self.gone();
                        return;
                    }
                }
                Cmd::Subscribe {
                    client,
                    outbox,
                    re,
                    body,
                    stream,
                } => self.subscribe(client, outbox, re, &body, stream),
                Cmd::Input { conn, stream, data } => {
                    let Some(i) = self
                        .subs
                        .iter()
                        .position(|s| s.stream == stream && s.client.conn == conn)
                    else {
                        continue;
                    };
                    if !self.subs[i].input || self.finished {
                        continue;
                    }
                    let _ = self.type_bytes(data);
                    self.touch(i, true);
                }
                Cmd::Drained(stream) => self.drained(stream),
                Cmd::ConnClosed(conn) => {
                    let gone: Vec<u32> = self
                        .subs
                        .iter()
                        .filter(|s| s.client.conn == conn)
                        .map(|s| s.stream)
                        .collect();
                    let window_lost = self
                        .subs
                        .iter()
                        .any(|s| s.client.conn == conn && s.role == "window");
                    for stream in gone {
                        self.unsubscribe(stream);
                    }
                    // A window that went without unsubscribing was closed, not detached: when
                    // it was the last one, the program goes with it.
                    if window_lost
                        && !self.keep
                        && !self.finished
                        && !self.subs.iter().any(|s| s.role == "window")
                    {
                        let _ = self.events.send(SessionEvent::WindowsClosed {
                            session: self.id.clone(),
                        });
                        self.end_program(KILL_GRACE);
                    }
                }
            }
        }
    }

    /// Ends the program and everything it started: on Windows at once; elsewhere with a hangup
    /// first, then a kill once `grace` has passed.
    fn end_program(&mut self, grace: Duration) {
        if cfg!(windows) {
            if let Some(c) = self.control.as_mut() {
                let _ = c.signal(Signal::Kill);
            }
        } else {
            if let Some(c) = self.control.as_mut() {
                let _ = c.signal(Signal::Hup);
            }
            self.later(grace, Cmd::KillNow);
        }
    }

    fn later(&self, after: Duration, cmd: Cmd) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            let _ = tx.send(cmd);
        });
    }

    fn close(&mut self) {
        self.input.take();
        if let Some(mut c) = self.control.take() {
            // Closing a pseudo-console can block until its output is read, which the reader
            // thread is doing; it must not hold up the task.
            tokio::task::spawn_blocking(move || c.close());
        }
    }

    // --- output --------------------------------------------------------------------------

    fn output(&mut self, chunk: &[u8]) {
        for piece in self.scanner.feed(chunk) {
            match piece {
                Piece::Data(d) => self.pass(d),
                Piece::Query(q) => {
                    let answer = queries::answer(q, self.parser.screen());
                    let _ = self.type_bytes(answer);
                }
            }
        }
        if self.scanner.holding() && !self.flush_pending {
            self.flush_pending = true;
            self.later(Duration::from_millis(HELD_QUERY_MS), Cmd::FlushHeld);
        }
    }

    /// Output passed on: into the model, the ring and every subscriber that keeps up.
    fn pass(&mut self, data: Vec<u8>) {
        self.parser.process(&data);
        self.ring.push(&data);
        let seq = self.seq;
        self.seq += data.len() as u64;
        for sub in &self.subs {
            if sub.budget.lagged() {
                continue;
            }
            let f = frame::output(sub.stream, seq, &data);
            if sub.budget.admits(f.len()) {
                sub.outbox.send_stream(f, &sub.budget);
            } else {
                sub.budget.set_lagged(true);
            }
        }
        self.callbacks();
    }

    fn callbacks(&mut self) {
        let cb = self.parser.callbacks_mut();
        let title =
            std::mem::take(&mut cb.title_changed).then(|| cb.title.clone().unwrap_or_default());
        let bells = std::mem::take(&mut cb.bells);
        let cwd = std::mem::take(&mut cb.cwd_changed).then(|| cb.cwd.clone().unwrap_or_default());
        if let Some(t) = title {
            self.emit("title", json!({ "title": t }), true);
        }
        if bells > 0 {
            self.emit("bell", json!({}), true);
        }
        if let Some(c) = cwd {
            self.emit("cwd", json!({ "cwd": c }), true);
        }
    }

    /// An event to watchers, and when `streams`, to this session's subscribers too, once per
    /// connection.
    fn emit(&self, name: &str, fields: Value, streams: bool) {
        let body = msg::event(name, Some(&self.id), fields);
        let mut skip = Vec::new();
        if streams {
            let bytes = frame::message(&body);
            for sub in &self.subs {
                if !skip.contains(&sub.client.conn) {
                    skip.push(sub.client.conn);
                    sub.outbox.send(bytes.clone());
                }
            }
        }
        let _ = self.events.send(SessionEvent::Event {
            session: self.id.clone(),
            body,
            skip,
        });
    }

    fn type_bytes(&mut self, bytes: Vec<u8>) -> Result<(), Error> {
        let Some(input) = &self.input else {
            return Err(Error::new(
                Code::Exited,
                format!("session {} has exited", self.id),
            ));
        };
        input.try_send(bytes).map_err(|e| match e {
            std::sync::mpsc::TrySendError::Full(_) => {
                Error::new(Code::Internal, "the program is not reading its input")
            }
            std::sync::mpsc::TrySendError::Disconnected(_) => {
                Error::new(Code::Exited, "the program's input is closed")
            }
        })
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let held = self.scanner.flush();
        if !held.is_empty() {
            self.pass(held);
        }
        self.close();
        self.exited_at = Some(now_ms());
        let exit = self.exit.clone().unwrap_or(Exit {
            code: -1,
            signal: None,
        });
        let mut fields = json!({ "code": exit.code });
        if let Some(s) = &exit.signal {
            fields["signal"] = json!(s);
        }
        self.emit("exited", fields, true);
        let _ = self.events.send(SessionEvent::Exited {
            session: self.id.clone(),
        });
        self.later(self.retain, Cmd::Expire);
    }

    fn gone(&mut self) {
        self.close();
        self.emit("removed", json!({}), false);
        let _ = self.events.send(SessionEvent::Gone {
            session: self.id.clone(),
        });
    }

    // --- subscribers and sizing --------------------------------------------------------

    fn subscribe(&mut self, client: Client, outbox: Outbox, re: u64, body: &Value, stream: u32) {
        let parsed = (|| -> Result<_, Error> {
            let from = match &body["from"] {
                Value::Null => From::Snapshot,
                Value::String(s) if s == "snapshot" => From::Snapshot,
                Value::String(s) if s == "now" => From::Now,
                v => From::Seq(
                    v.as_u64()
                        .ok_or_else(|| Error::bad("from must be \"snapshot\", \"now\" or a seq"))?,
                ),
            };
            let size = match body.get("size") {
                None | Some(Value::Null) => None,
                Some(_) => Some(
                    spawn::parse_size(body.get("size"), (self.cols, self.rows))
                        .map_err(Error::bad)?,
                ),
            };
            let limit = body["buffer"]
                .as_u64()
                .map(|b| b as usize)
                .unwrap_or(DEFAULT_BUDGET)
                .max(1024);
            Ok((from, size, limit))
        })();
        let (from, size, limit) = match parsed {
            Ok(p) => p,
            Err(e) => {
                outbox.send(frame::message(&msg::err(re, &e)));
                return;
            }
        };
        let tx = self.tx.clone();
        let budget = Budget::new(stream, limit, move |s| {
            let _ = tx.send(Cmd::Drained(s));
        });
        self.tick += 1;
        let sub = Sub {
            stream,
            client,
            outbox: outbox.clone(),
            budget: budget.clone(),
            input: body["input"].as_bool().unwrap_or(false),
            sizing: body["sizing"].as_bool().unwrap_or(false),
            size,
            role: body["role"]
                .as_str()
                .filter(|r| matches!(*r, "window" | "viewer" | "control"))
                .unwrap_or("control")
                .to_string(),
            scrollback: body["scrollback"].as_bool().unwrap_or(false),
            attached_at: now_ms(),
            last_input: None,
            active: self.tick,
        };
        let scrollback = sub.scrollback;
        let info = sub.info();
        self.subs.push(sub);
        // The newest sizing subscriber takes the terminal to its size before it is painted.
        if !self.finished {
            let i = self.subs.len() - 1;
            self.touch(i, false);
        }
        outbox.send(frame::message(&msg::ok(
            re,
            json!({ "stream": stream, "seq": self.seq, "cols": self.cols, "rows": self.rows }),
        )));
        match from {
            From::Now => {}
            From::Seq(n) => match self.ring.since(n) {
                Some(bytes) if bytes.is_empty() => {}
                Some(bytes) => {
                    outbox.send_stream(frame::output(stream, n, &bytes), &budget);
                }
                None => self.resync(stream, scrollback, &outbox, &budget),
            },
            From::Snapshot => self.resync(stream, scrollback, &outbox, &budget),
        }
        self.emit("attached", json!({ "client": info }), false);
        if self.finished {
            let exit = self.exit.clone().unwrap_or(Exit {
                code: -1,
                signal: None,
            });
            outbox.send(frame::message(&msg::event(
                "exited",
                Some(&self.id),
                json!({ "code": exit.code }),
            )));
        }
    }

    fn resync(&mut self, stream: u32, scrollback: bool, outbox: &Outbox, budget: &Arc<Budget>) {
        let title = self.parser.callbacks().title.clone();
        let bytes = screen::repaint(self.parser.screen_mut(), scrollback, title.as_deref());
        outbox.send_stream(frame::resync(stream, self.seq, &bytes), budget);
    }

    fn drained(&mut self, stream: u32) {
        let Some(sub) = self.subs.iter().find(|s| s.stream == stream) else {
            return;
        };
        if !sub.budget.lagged() {
            return;
        }
        sub.budget.set_lagged(false);
        let (scrollback, outbox, budget) = (sub.scrollback, sub.outbox.clone(), sub.budget.clone());
        self.resync(stream, scrollback, &outbox, &budget);
    }

    fn unsubscribe(&mut self, stream: u32) -> bool {
        let Some(i) = self.subs.iter().position(|s| s.stream == stream) else {
            return false;
        };
        let sub = self.subs.remove(i);
        self.emit(
            "detached",
            json!({ "client": sub.client.id(), "stream": stream }),
            false,
        );
        if self.sized_by == Some(stream) && !self.finished {
            self.sized_by = None;
            // The sizing subscriber used most recently takes over the size.
            if let Some(next) = self
                .subs
                .iter()
                .enumerate()
                .filter(|(_, s)| s.sizing && s.size.is_some())
                .max_by_key(|(_, s)| s.active)
                .map(|(i, _)| i)
            {
                self.touch(next, false);
            }
        }
        true
    }

    /// A subscriber subscribed, typed or resized: it is now the most recent, and a sizing
    /// one takes the terminal to its size.
    fn touch(&mut self, i: usize, typed: bool) {
        self.tick += 1;
        let sub = &mut self.subs[i];
        sub.active = self.tick;
        if typed {
            sub.last_input = Some(now_ms());
        }
        if sub.sizing {
            if let Some(size) = sub.size {
                let (stream, by) = (sub.stream, sub.client.id());
                self.sized_by = Some(stream);
                self.set_size(size, Some(by));
            }
        }
    }

    fn set_size(&mut self, (cols, rows): (u16, u16), by: Option<String>) {
        if (cols, rows) == (self.cols, self.rows) {
            return;
        }
        if let Some(c) = self.control.as_mut() {
            let _ = c.resize(cols, rows);
        }
        self.parser.screen_mut().set_size(rows, cols);
        self.cols = cols;
        self.rows = rows;
        let mut fields = json!({ "cols": cols, "rows": rows });
        if let Some(by) = by {
            fields["by"] = json!(by);
        }
        self.emit("resized", fields, true);
    }

    /// The sizing subscriber a connection holds on this session, if any.
    fn sizer_of(&self, conn: ConnId) -> Option<usize> {
        self.subs
            .iter()
            .position(|s| s.client.conn == conn && s.sizing)
    }

    // --- requests ------------------------------------------------------------------------

    fn info(&self) -> Value {
        let mut v = json!({
            "session": self.id,
            "argv": self.argv,
            "cwd": self.cwd,
            "cols": self.cols,
            "rows": self.rows,
            "labels": self.labels,
            "keep": self.keep,
            "status": if self.finished { "exited" } else { "running" },
            "startedAt": self.started_at,
            "seq": self.seq,
            "clients": self.subs.iter().map(Sub::info).collect::<Vec<_>>(),
        });
        if let Some(n) = &self.name {
            v["name"] = json!(n);
        }
        if let Some(p) = self.pid {
            v["pid"] = json!(p);
        }
        let cb = self.parser.callbacks();
        if let Some(t) = &cb.title {
            v["title"] = json!(t);
        }
        if let Some(c) = &cb.cwd {
            v["cwdReported"] = json!(c);
        }
        if self.finished {
            let exit = self.exit.clone().unwrap_or(Exit {
                code: -1,
                signal: None,
            });
            let mut e = json!({ "code": exit.code });
            if let Some(s) = exit.signal {
                e["signal"] = json!(s);
            }
            v["exit"] = e;
            if let Some(t) = self.exited_at {
                v["exitedAt"] = json!(t);
            }
        }
        v
    }

    fn running(&self) -> Result<(), Error> {
        if self.finished {
            Err(Error::new(
                Code::Exited,
                format!("session {} has exited", self.id),
            ))
        } else {
            Ok(())
        }
    }

    fn request(&mut self, client: &Client, op: &str, body: &Value) -> Result<Value, Error> {
        match op {
            "info" => Ok(self.info()),
            "write" => {
                self.running()?;
                let bytes = match (body["data"].as_str(), body["b64"].as_str()) {
                    (Some(text), None) => text.as_bytes().to_vec(),
                    (None, Some(b)) => {
                        base64_decode(b).ok_or_else(|| Error::bad("b64 is not base64"))?
                    }
                    _ => return Err(Error::bad("write takes data or b64")),
                };
                self.type_bytes(bytes)?;
                self.touched_by(client.conn, true);
                Ok(json!({}))
            }
            "paste" => {
                self.running()?;
                let text = body["text"]
                    .as_str()
                    .ok_or_else(|| Error::bad("paste takes text"))?;
                let bracketed = match &body["bracketed"] {
                    Value::Bool(b) => *b,
                    Value::Null => self.parser.screen().bracketed_paste(),
                    Value::String(s) if s == "auto" => self.parser.screen().bracketed_paste(),
                    _ => return Err(Error::bad("bracketed is \"auto\", true or false")),
                };
                self.type_bytes(keys::paste(text, bracketed))?;
                self.touched_by(client.conn, true);
                Ok(json!({ "bracketed": bracketed }))
            }
            "keys" => {
                self.running()?;
                let names: Vec<String> = body["keys"]
                    .as_array()
                    .ok_or_else(|| Error::bad("keys takes an array of key names"))?
                    .iter()
                    .map(|k| k.as_str().unwrap_or("").to_string())
                    .collect();
                let bytes = keys::encode_all(&names, self.parser.screen().application_cursor())
                    .map_err(Error::bad)?;
                self.type_bytes(bytes)?;
                self.touched_by(client.conn, true);
                Ok(json!({}))
            }
            "resize" => {
                self.running()?;
                let (cols, rows) = spawn::parse_size(Some(body), (0, 0)).map_err(Error::bad)?;
                if cols == 0 || rows == 0 {
                    return Err(Error::bad("resize takes cols and rows"));
                }
                match self.sizer_of(client.conn) {
                    Some(i) => {
                        self.subs[i].size = Some((cols, rows));
                        self.touch(i, false);
                    }
                    None => {
                        self.sized_by = None;
                        self.set_size((cols, rows), Some(client.id()));
                    }
                }
                Ok(json!({}))
            }
            "screen" => {
                let scrollback = body["scrollback"].as_bool().unwrap_or(false);
                let format = body["format"].as_str().unwrap_or("text");
                let s = self.parser.screen();
                let (row, col) = s.cursor_position();
                let m = screen::modes(s);
                let mut v = json!({
                    "format": format,
                    "cols": self.cols,
                    "rows": self.rows,
                    "cursor": { "row": row, "col": col, "visible": m.cursor_visible },
                    "seq": self.seq,
                    "altScreen": m.alt_screen,
                    "bracketedPaste": m.bracketed_paste,
                    "appCursor": m.app_cursor,
                });
                let title = self.parser.callbacks().title.clone();
                if let Some(t) = &title {
                    v["title"] = json!(t);
                }
                match format {
                    "text" => {
                        v["lines"] = json!(screen::text(self.parser.screen_mut(), scrollback))
                    }
                    "vt" => {
                        v["data"] = json!(String::from_utf8_lossy(&screen::repaint(
                            self.parser.screen_mut(),
                            scrollback,
                            title.as_deref()
                        )))
                    }
                    "cells" => {
                        v["cells"] = json!(screen::cells(self.parser.screen_mut(), scrollback))
                    }
                    other => return Err(Error::bad(format!("no screen format {other:?}"))),
                }
                Ok(v)
            }
            "unsubscribe" => {
                let stream = body["stream"]
                    .as_u64()
                    .ok_or_else(|| Error::bad("unsubscribe takes a stream"))?
                    as u32;
                if !self
                    .subs
                    .iter()
                    .any(|s| s.stream == stream && s.client.conn == client.conn)
                {
                    return Err(Error::new(Code::NotFound, format!("no stream {stream}")));
                }
                self.unsubscribe(stream);
                Ok(json!({}))
            }
            "kill" => {
                self.running()?;
                let grace = body["graceMs"]
                    .as_u64()
                    .map(Duration::from_millis)
                    .unwrap_or(KILL_GRACE);
                self.end_program(grace);
                Ok(json!({}))
            }
            "signal" => {
                self.running()?;
                let sig = body["signal"]
                    .as_str()
                    .and_then(Signal::parse)
                    .ok_or_else(|| Error::bad("signal is INT, TERM, HUP or KILL"))?;
                if cfg!(windows) && sig == Signal::Int {
                    self.type_bytes(vec![3])?;
                } else if let Some(c) = self.control.as_mut() {
                    c.signal(sig)
                        .map_err(|e| Error::new(Code::Internal, e.to_string()))?;
                }
                Ok(json!({}))
            }
            "set-labels" => {
                let Some(Value::Object(m)) = body.get("labels") else {
                    return Err(Error::bad("set-labels takes labels"));
                };
                for (k, v) in m {
                    match v {
                        Value::Null => {
                            self.labels.remove(k);
                        }
                        Value::String(s) => {
                            self.labels.insert(k.clone(), s.clone());
                        }
                        _ => return Err(Error::bad(format!("label {k} must be a string or null"))),
                    }
                }
                let labels: Map<String, Value> = self
                    .labels
                    .iter()
                    .map(|(k, v)| (k.clone(), json!(v)))
                    .collect();
                self.emit("labels", json!({ "labels": labels }), false);
                Ok(json!({ "labels": labels }))
            }
            "remove" => {
                if !self.finished {
                    return Err(Error::bad(format!("session {} is still running", self.id)));
                }
                Ok(json!({}))
            }
            other => Err(Error::new(
                Code::Unsupported,
                format!("no op {other:?} on a session"),
            )),
        }
    }

    /// Typing through a control request counts for the connection's sizing subscriber too.
    fn touched_by(&mut self, conn: ConnId, typed: bool) {
        if let Some(i) = self.sizer_of(conn) {
            self.touch(i, typed);
        } else if typed {
            if let Some(i) = self.subs.iter().position(|s| s.client.conn == conn) {
                self.subs[i].last_input = Some(now_ms());
            }
        }
    }
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32)
    }
    let clean: Vec<u8> = s
        .bytes()
        .filter(|c| !c.is_ascii_whitespace() && *c != b'=')
        .collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= val(*c)? << (18 - 6 * i);
        }
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend(&bytes[..chunk.len().saturating_sub(1)]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outbox::Out;
    use crate::pty::fake::{FakePty, Program};
    use tether_proto::frame::{Decoder, Frame};

    struct Rig {
        session: Session,
        program: Program,
        events: mpsc::UnboundedReceiver<SessionEvent>,
    }

    fn rig(cols: u16, rows: u16) -> Rig {
        rig_spawned(json!({ "size": { "cols": cols, "rows": rows } }))
    }

    /// A session started with these `spawn` fields besides its program.
    fn rig_spawned(mut fields: Value) -> Rig {
        let pty = FakePty::default();
        let (etx, events) = mpsc::unbounded_channel();
        fields["argv"] = json!([std::env::current_exe().unwrap().to_string_lossy()]);
        let mut spec = spawn::parse_spawn(&fields).unwrap();
        spec.scrollback = 100;
        let session = Session::start(SessionOptions {
            id: "s1".into(),
            spec,
            pty: Arc::new(pty.clone()),
            events: etx,
            retain: Duration::from_secs(60),
        })
        .unwrap();
        let program = pty.take().unwrap();
        Rig {
            session,
            program,
            events,
        }
    }

    fn client(conn: ConnId) -> Client {
        Client {
            conn,
            pid: None,
            name: Some("test".into()),
        }
    }

    /// Everything a connection's outbox received, as frames, waiting until `until` holds.
    async fn collect(
        rx: &mut mpsc::UnboundedReceiver<Out>,
        until: impl Fn(&[Frame]) -> bool,
    ) -> Vec<Frame> {
        let mut frames = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !until(&frames) {
            let out = tokio::time::timeout_at(deadline, rx.recv())
                .await
                .expect("timed out")
                .expect("outbox open");
            let (bytes, budget) = match out {
                Out::Frame(b) => (b, None),
                Out::Stream { bytes, budget } => (bytes, Some(budget)),
            };
            let mut d = Decoder::new();
            d.push(&bytes);
            frames.push(d.next_frame().unwrap().unwrap());
            if let Some(b) = budget {
                b.written(bytes.len());
            }
        }
        frames
    }

    fn json_of(f: &Frame) -> Option<Value> {
        match f {
            Frame::Message(b) => serde_json::from_slice(b).ok(),
            _ => None,
        }
    }

    fn output_text(frames: &[Frame]) -> String {
        frames
            .iter()
            .filter_map(|f| match f {
                Frame::Output { data, .. } | Frame::Resync { data, .. } => {
                    Some(String::from_utf8_lossy(data).to_string())
                }
                _ => None,
            })
            .collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn answers_the_cursor_query_with_no_window_and_keeps_it_out_of_the_stream() {
        let mut r = rig(80, 24);
        let (outbox, mut rx) = Outbox::new(1);
        assert!(r
            .session
            .subscribe(client(1), outbox, 1, json!({ "from": "now" }), 7));
        collect(&mut rx, |f| !f.is_empty()).await;
        r.program.write(b"ab\x1b[6ncd");
        assert_eq!(r.program.typed(2000), b"\x1b[1;3R");
        let frames = collect(&mut rx, |f| output_text(f).contains("cd")).await;
        assert_eq!(output_text(&frames), "abcd");
        match &frames[0] {
            Frame::Output { stream, seq, .. } => assert_eq!((*stream, *seq), (7, 0)),
            other => panic!("{other:?}"),
        }
        let _ = r.events.try_recv();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_snapshot_joins_the_stream_with_no_gap() {
        let r = rig(40, 5);
        r.program.write(b"one\r\ntwo\r\n");
        // Let the output land before subscribing.
        let info = loop {
            let info = r
                .session
                .request(&client(9), "info", json!({}))
                .await
                .unwrap();
            if info["seq"].as_u64().unwrap() >= 10 {
                break info;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let (outbox, mut rx) = Outbox::new(2);
        r.session
            .subscribe(client(2), outbox, 5, json!({ "scrollback": true }), 3);
        let frames = collect(&mut rx, |f| f.len() >= 2).await;
        let resp = json_of(&frames[0]).unwrap();
        assert_eq!(resp["re"], 5);
        assert_eq!(resp["ok"]["seq"], info["seq"]);
        match &frames[1] {
            Frame::Resync { stream, seq, data } => {
                assert_eq!((*stream, *seq), (3, info["seq"].as_u64().unwrap()));
                let mut v = vt100::Parser::new(5, 40, 0);
                v.process(data);
                assert!(v.screen().contents().starts_with("one\ntwo"));
            }
            other => panic!("{other:?}"),
        }
        r.program.write(b"three");
        let frames = collect(&mut rx, |f| !f.is_empty()).await;
        match &frames[0] {
            Frame::Output { seq, data, .. } => {
                assert_eq!(*seq, info["seq"].as_u64().unwrap());
                assert_eq!(data, b"three");
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_viewer_that_falls_behind_gets_a_fresh_screen() {
        let r = rig(20, 3);
        let (outbox, mut rx) = Outbox::new(1);
        r.session.subscribe(
            client(1),
            outbox,
            1,
            json!({ "from": "now", "buffer": 1024 }),
            1,
        );
        collect(&mut rx, |f| !f.is_empty()).await;
        // The writer does not drain while the program floods: nothing is read from rx.
        for i in 0..200 {
            r.program.write(format!("line {i:04}\r\n").as_bytes());
        }
        r.program.write(b"last");
        tokio::time::sleep(Duration::from_millis(200)).await;
        let frames = collect(&mut rx, |f| {
            f.iter().any(|f| matches!(f, Frame::Resync { .. }))
        })
        .await;
        let outputs: usize = frames
            .iter()
            .filter(|f| matches!(f, Frame::Output { .. }))
            .count();
        assert!(outputs < 200, "the backlog was cut off ({outputs} chunks)");
        let Some(Frame::Resync { data, seq, .. }) =
            frames.iter().find(|f| matches!(f, Frame::Resync { .. }))
        else {
            unreachable!()
        };
        let mut v = vt100::Parser::new(3, 20, 0);
        v.process(data);
        assert!(
            v.screen().contents().ends_with("last"),
            "{:?}",
            v.screen().contents()
        );
        let info = r
            .session
            .request(&client(9), "info", json!({}))
            .await
            .unwrap();
        assert_eq!(*seq, info["seq"].as_u64().unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_latest_sizing_subscriber_sets_the_size() {
        let r = rig(80, 24);
        let (a, mut arx) = Outbox::new(1);
        let (b, mut brx) = Outbox::new(2);
        let (v, mut vrx) = Outbox::new(3);
        r.session.subscribe(client(1), a, 1, json!({ "from": "now", "sizing": true, "input": true, "size": { "cols": 100, "rows": 30 } }), 11);
        collect(&mut arx, |f| !f.is_empty()).await;
        assert_eq!(
            r.program
                .sizes
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            (100, 30)
        );
        r.session.subscribe(client(2), b, 1, json!({ "from": "now", "sizing": true, "input": true, "size": { "cols": 60, "rows": 20 } }), 12);
        collect(&mut brx, |f| !f.is_empty()).await;
        assert_eq!(
            r.program
                .sizes
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            (60, 20)
        );
        // A viewer that does not size changes nothing.
        r.session.subscribe(
            client(3),
            v,
            1,
            json!({ "from": "now", "size": { "cols": 40, "rows": 10 } }),
            13,
        );
        collect(&mut vrx, |f| !f.is_empty()).await;
        // Typing in the first window takes the size back.
        r.session.input(1, 11, b"x".to_vec());
        assert_eq!(
            r.program
                .sizes
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            (100, 30)
        );
        let resized = collect(&mut vrx, |f| {
            f.iter()
                .any(|f| json_of(f).is_some_and(|v| v["ev"] == "resized"))
        })
        .await;
        let ev = resized
            .iter()
            .filter_map(json_of)
            .find(|v| v["ev"] == "resized")
            .unwrap();
        assert_eq!(
            (ev["cols"].as_u64(), ev["by"].as_str()),
            (Some(100), Some("c1"))
        );
        // The window that set it leaves: the other sizing window takes over.
        r.session.conn_closed(1);
        assert_eq!(
            r.program
                .sizes
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            (60, 20)
        );
        assert_eq!(r.program.typed(1000), b"x");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn input_needs_the_right() {
        let r = rig(80, 24);
        let (a, mut arx) = Outbox::new(1);
        r.session
            .subscribe(client(1), a, 1, json!({ "from": "now" }), 1);
        collect(&mut arx, |f| !f.is_empty()).await;
        r.session.input(1, 1, b"nope".to_vec());
        r.session.input(2, 1, b"nope".to_vec());
        r.session
            .request(&client(1), "keys", json!({ "keys": ["Down", "Enter"] }))
            .await
            .unwrap();
        assert_eq!(r.program.typed(1000), b"\x1b[B\r");
        r.program.write(b"\x1b[?1h\x1b[?2004h");
        tokio::time::sleep(Duration::from_millis(50)).await;
        r.session
            .request(&client(1), "keys", json!({ "keys": ["Down"] }))
            .await
            .unwrap();
        let p = r
            .session
            .request(&client(1), "paste", json!({ "text": "hi\nthere" }))
            .await
            .unwrap();
        assert_eq!(p["bracketed"], true);
        assert_eq!(r.program.typed(1000), b"\x1bOB\x1b[200~hi\rthere\x1b[201~");
        assert!(r
            .session
            .request(&client(1), "keys", json!({ "keys": ["Nope"] }))
            .await
            .is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_exit_is_reported_after_the_last_output_and_the_session_is_kept() {
        let mut r = rig(80, 24);
        let (a, mut arx) = Outbox::new(1);
        r.session
            .subscribe(client(1), a, 1, json!({ "from": "now" }), 1);
        collect(&mut arx, |f| !f.is_empty()).await;
        r.program.write(b"bye");
        r.program.exit(3);
        let program = r.program;
        drop(program);
        let frames = collect(&mut arx, |f| {
            f.iter()
                .any(|f| json_of(f).is_some_and(|v| v["ev"] == "exited"))
        })
        .await;
        assert_eq!(output_text(&frames), "bye");
        let exited = frames
            .iter()
            .filter_map(json_of)
            .find(|v| v["ev"] == "exited")
            .unwrap();
        assert_eq!(exited["code"], 3);
        let info = r
            .session
            .request(&client(1), "info", json!({}))
            .await
            .unwrap();
        assert_eq!(info["status"], "exited");
        assert_eq!(info["exit"]["code"], 3);
        assert!(r
            .session
            .request(&client(1), "write", json!({ "data": "x" }))
            .await
            .unwrap_err()
            .is(Code::Exited));
        let mut exited_event = false;
        while let Ok(e) = r.events.try_recv() {
            if let SessionEvent::Exited { .. } = e {
                exited_event = true;
            }
        }
        assert!(exited_event);
        r.session
            .request(&client(1), "remove", json!({}))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(r.session.gone());
    }

    /// Subscribes connection `conn` as `role` on stream `conn`, once the answer is in.
    async fn attach(r: &Rig, conn: ConnId, role: &str) -> mpsc::UnboundedReceiver<Out> {
        let (outbox, mut rx) = Outbox::new(conn);
        r.session.subscribe(
            client(conn),
            outbox,
            1,
            json!({ "from": "now", "role": role }),
            conn as u32,
        );
        collect(&mut rx, |f| !f.is_empty()).await;
        rx
    }

    /// The signal the program got, once everything sent to the session before has been done.
    async fn signalled(r: &Rig) -> Option<Signal> {
        r.session
            .request(&client(99), "info", json!({}))
            .await
            .unwrap();
        r.program.signals.try_recv().ok()
    }

    const ENDED_BY: Signal = if cfg!(windows) {
        Signal::Kill
    } else {
        Signal::Hup
    };

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_last_window_to_close_ends_the_program() {
        let mut r = rig(80, 24);
        let _a = attach(&r, 1, "window").await;
        let _b = attach(&r, 2, "window").await;
        let _v = attach(&r, 3, "viewer").await;
        // A viewer going ends nothing, nor does a window while another is left.
        r.session.conn_closed(3);
        r.session.conn_closed(1);
        assert_eq!(signalled(&r).await, None);
        r.session.conn_closed(2);
        assert_eq!(signalled(&r).await, Some(ENDED_BY));
        let mut told = false;
        while let Ok(e) = r.events.try_recv() {
            told |= matches!(e, SessionEvent::WindowsClosed { .. });
        }
        assert!(told, "the host hears why");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_window_that_detaches_leaves_the_program_running() {
        let r = rig(80, 24);
        let _a = attach(&r, 1, "window").await;
        r.session
            .request(&client(1), "unsubscribe", json!({ "stream": 1 }))
            .await
            .unwrap();
        r.session.conn_closed(1);
        assert_eq!(signalled(&r).await, None);
        // Nor does anything but a window hold it: the last viewer and controller go too.
        let _v = attach(&r, 2, "viewer").await;
        let _c = attach(&r, 3, "control").await;
        r.session.conn_closed(2);
        r.session.conn_closed(3);
        assert_eq!(signalled(&r).await, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_kept_session_runs_on_when_its_last_window_closes() {
        let r = rig_spawned(json!({ "keep": true }));
        let _a = attach(&r, 1, "window").await;
        r.session.conn_closed(1);
        assert_eq!(signalled(&r).await, None);
        let info = r
            .session
            .request(&client(9), "info", json!({}))
            .await
            .unwrap();
        assert_eq!(info["keep"], true);
        assert!(spawn::parse_spawn(&json!({ "argv": ["x"], "keep": "yes" })).is_err());
    }

    #[test]
    fn reads_osc7_paths() {
        assert_eq!(
            file_url_path("file://host/home/me/a%20b").as_deref(),
            Some("/home/me/a b")
        );
        assert_eq!(
            file_url_path("file://host/C:/Users/me").as_deref(),
            Some("C:/Users/me")
        );
        assert_eq!(file_url_path("http://x/y"), None);
    }

    #[test]
    fn decodes_base64() {
        assert_eq!(base64_decode("aGk=").unwrap(), b"hi");
        assert_eq!(base64_decode("G1tB").unwrap(), b"\x1b[A");
        assert_eq!(base64_decode("!!"), None);
    }
}
