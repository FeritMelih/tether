//! What a connection is to a session: an outbox of frames, written in order by the
//! connection's writer. Output for a subscription is accounted against its budget, so the
//! session can tell a viewer that keeps up from one that has fallen behind without ever
//! waiting on either.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

pub type ConnId = u64;

pub enum Out {
    /// A frame that is not a subscription's output: a response, an event.
    Frame(Vec<u8>),
    /// A subscription's output or resync, counted against its budget until written.
    Stream { bytes: Vec<u8>, budget: Arc<Budget> },
}

#[derive(Clone)]
pub struct Outbox {
    pub conn: ConnId,
    tx: mpsc::UnboundedSender<Out>,
}

impl Outbox {
    pub fn new(conn: ConnId) -> (Self, mpsc::UnboundedReceiver<Out>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { conn, tx }, rx)
    }

    /// False once the connection has gone.
    pub fn send(&self, frame: Vec<u8>) -> bool {
        self.tx.send(Out::Frame(frame)).is_ok()
    }

    pub fn send_stream(&self, bytes: Vec<u8>, budget: &Arc<Budget>) -> bool {
        budget.queued.fetch_add(bytes.len(), Ordering::AcqRel);
        self.tx
            .send(Out::Stream {
                bytes,
                budget: budget.clone(),
            })
            .is_ok()
    }

    pub fn closed(&self) -> bool {
        self.tx.is_closed()
    }
}

/// A subscription's queued output. Past `limit` the subscription is lagging: the session
/// stops queueing for it, and once the writer has drained what was queued, `drained` fires
/// and the session sends a fresh screen instead of the backlog.
pub struct Budget {
    pub stream: u32,
    pub limit: usize,
    queued: AtomicUsize,
    lagged: AtomicBool,
    drained: Box<dyn Fn(u32) + Send + Sync>,
}

impl Budget {
    pub fn new(
        stream: u32,
        limit: usize,
        drained: impl Fn(u32) + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            stream,
            limit,
            queued: AtomicUsize::new(0),
            lagged: AtomicBool::new(false),
            drained: Box::new(drained),
        })
    }

    pub fn queued(&self) -> usize {
        self.queued.load(Ordering::Acquire)
    }

    pub fn lagged(&self) -> bool {
        self.lagged.load(Ordering::Acquire)
    }

    /// Whether `n` more bytes may be queued: always into an empty queue, else within the limit.
    pub fn admits(&self, n: usize) -> bool {
        let q = self.queued();
        q == 0 || q + n <= self.limit
    }

    pub fn set_lagged(&self, lagged: bool) {
        self.lagged.store(lagged, Ordering::Release);
    }

    /// The writer wrote `n` bytes of this subscription's output.
    pub fn written(&self, n: usize) {
        let left = self.queued.fetch_sub(n, Ordering::AcqRel) - n;
        if left == 0 && self.lagged() {
            (self.drained)(self.stream);
        }
    }
}
