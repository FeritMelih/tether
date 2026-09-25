//! tether's sessions. A session is a program in a pseudo-terminal, a screen model fed from
//! its output, a ring of recent output, and the subscribers that watch it, all owned by one
//! task: every change to a session is a message to that task, so a snapshot and the stream
//! after it are taken in one step and a subscriber sees neither a gap nor a repeat.

pub mod keys;
pub mod outbox;
pub mod pty;
pub mod queries;
pub mod ring;
pub mod screen;
pub mod session;
pub mod spawn;

pub use outbox::{Budget, ConnId, Out, Outbox};
pub use session::{Session, SessionEvent, SessionOptions};
