//! The tether wire protocol, as `spec/protocol.md` defines it: frames, the handshake, the
//! message envelopes and the discovery files. No operating-system dependency beyond `std`,
//! so the host and every client share one definition.

pub mod discovery;
pub mod frame;
pub mod handshake;
pub mod msg;

/// The protocol version this crate speaks. Additions within a version go through capability
/// strings; a version changes only when an old client could misread a new host.
pub const PROTOCOL: u32 = 1;

/// What a version-1 host offers.
pub const HOST_CAPS: &[&str] = &[
    "paste",
    "keys",
    "screen.cells",
    "resync",
    "drain",
    "labels",
    "keep",
];
