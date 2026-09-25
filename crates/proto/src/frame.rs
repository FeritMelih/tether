//! Frames: `u32 LE length | u8 type | payload`. The encoder builds whole frames; the decoder
//! takes bytes as they arrive and yields frames as they complete, so a transport never has
//! to know where one ends.

use std::fmt;

/// The largest payload a frame may carry.
pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;
/// The largest JSON payload (handshake or message).
pub const MAX_JSON: usize = 1024 * 1024;
/// Length and type.
pub const HEADER: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    Handshake = 0x00,
    Message = 0x01,
    Output = 0x02,
    Input = 0x03,
    Resync = 0x04,
}

impl FrameType {
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            0x00 => Some(Self::Handshake),
            0x01 => Some(Self::Message),
            0x02 => Some(Self::Output),
            0x03 => Some(Self::Input),
            0x04 => Some(Self::Resync),
            _ => None,
        }
    }
}

/// A decoded frame. A type the decoder does not know is kept as `Unknown`, for the caller to
/// ignore, as the protocol asks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    Handshake(Vec<u8>),
    Message(Vec<u8>),
    Output {
        stream: u32,
        seq: u64,
        data: Vec<u8>,
    },
    Input {
        stream: u32,
        data: Vec<u8>,
    },
    Resync {
        stream: u32,
        seq: u64,
        data: Vec<u8>,
    },
    Unknown(u8),
}

#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    TooLarge(usize),
    /// A binary frame too short for its own header.
    Short(FrameType),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge(n) => write!(f, "frame of {n} bytes is over the limit"),
            Self::Short(t) => write!(f, "{t:?} frame is shorter than its header"),
        }
    }
}

impl std::error::Error for FrameError {}

fn header(out: &mut Vec<u8>, ty: FrameType, len: usize) {
    out.extend_from_slice(&(len as u32).to_le_bytes());
    out.push(ty as u8);
}

pub fn encode_json(ty: FrameType, json: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + json.len());
    header(&mut out, ty, json.len());
    out.extend_from_slice(json);
    out
}

pub fn message(value: &serde_json::Value) -> Vec<u8> {
    encode_json(
        FrameType::Message,
        &serde_json::to_vec(value).expect("a JSON value serializes"),
    )
}

pub fn handshake(value: &serde_json::Value) -> Vec<u8> {
    encode_json(
        FrameType::Handshake,
        &serde_json::to_vec(value).expect("a JSON value serializes"),
    )
}

pub fn output(stream: u32, seq: u64, data: &[u8]) -> Vec<u8> {
    seq_frame(FrameType::Output, stream, seq, data)
}

pub fn resync(stream: u32, seq: u64, data: &[u8]) -> Vec<u8> {
    seq_frame(FrameType::Resync, stream, seq, data)
}

fn seq_frame(ty: FrameType, stream: u32, seq: u64, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + 12 + data.len());
    header(&mut out, ty, 12 + data.len());
    out.extend_from_slice(&stream.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(data);
    out
}

pub fn input(stream: u32, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + 4 + data.len());
    header(&mut out, FrameType::Input, 4 + data.len());
    out.extend_from_slice(&stream.to_le_bytes());
    out.extend_from_slice(data);
    out
}

/// Incremental decoding: `push` bytes as they are read, then take frames with `next_frame`.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
    start: usize,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if self.start > 0 && self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        } else if self.start > 64 * 1024 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete frame, `Ok(None)` when more bytes are needed, an error for a frame
    /// the connection must be closed over.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        let avail = &self.buf[self.start..];
        if avail.len() < HEADER {
            return Ok(None);
        }
        let len = u32::from_le_bytes([avail[0], avail[1], avail[2], avail[3]]) as usize;
        let ty = avail[4];
        let json = matches!(
            FrameType::from_u8(ty),
            Some(FrameType::Handshake | FrameType::Message)
        );
        if len > MAX_PAYLOAD || (json && len > MAX_JSON) {
            return Err(FrameError::TooLarge(len));
        }
        if avail.len() < HEADER + len {
            return Ok(None);
        }
        let payload = &avail[HEADER..HEADER + len];
        let frame = decode(ty, payload)?;
        self.start += HEADER + len;
        Ok(Some(frame))
    }

    /// Bytes held that are not yet a whole frame.
    pub fn pending(&self) -> usize {
        self.buf.len() - self.start
    }
}

fn decode(ty: u8, p: &[u8]) -> Result<Frame, FrameError> {
    let Some(t) = FrameType::from_u8(ty) else {
        return Ok(Frame::Unknown(ty));
    };
    Ok(match t {
        FrameType::Handshake => Frame::Handshake(p.to_vec()),
        FrameType::Message => Frame::Message(p.to_vec()),
        FrameType::Output | FrameType::Resync => {
            if p.len() < 12 {
                return Err(FrameError::Short(t));
            }
            let stream = u32::from_le_bytes(p[0..4].try_into().unwrap());
            let seq = u64::from_le_bytes(p[4..12].try_into().unwrap());
            let data = p[12..].to_vec();
            if t == FrameType::Output {
                Frame::Output { stream, seq, data }
            } else {
                Frame::Resync { stream, seq, data }
            }
        }
        FrameType::Input => {
            if p.len() < 4 {
                return Err(FrameError::Short(t));
            }
            Frame::Input {
                stream: u32::from_le_bytes(p[0..4].try_into().unwrap()),
                data: p[4..].to_vec(),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_type_split_at_every_byte() {
        let mut bytes = Vec::new();
        bytes.extend(message(&serde_json::json!({"id": 1, "op": "ping"})));
        bytes.extend(output(3, 1 << 40, b"\x1b[31mhi"));
        bytes.extend(input(3, b"\r"));
        bytes.extend(resync(3, 7, b""));
        bytes.extend(handshake(&serde_json::json!({"proof": "00"})));
        let expected = vec![
            Frame::Message(br#"{"id":1,"op":"ping"}"#.to_vec()),
            Frame::Output {
                stream: 3,
                seq: 1 << 40,
                data: b"\x1b[31mhi".to_vec(),
            },
            Frame::Input {
                stream: 3,
                data: b"\r".to_vec(),
            },
            Frame::Resync {
                stream: 3,
                seq: 7,
                data: vec![],
            },
            Frame::Handshake(br#"{"proof":"00"}"#.to_vec()),
        ];
        for split in 0..bytes.len() {
            let mut d = Decoder::new();
            let mut got = Vec::new();
            for chunk in [&bytes[..split], &bytes[split..]] {
                d.push(chunk);
                while let Some(f) = d.next_frame().unwrap() {
                    got.push(f);
                }
            }
            assert_eq!(got, expected, "split at {split}");
            assert_eq!(d.pending(), 0);
        }
    }

    #[test]
    fn refuses_oversized_frames() {
        let mut d = Decoder::new();
        let mut head = ((MAX_JSON + 1) as u32).to_le_bytes().to_vec();
        head.push(FrameType::Message as u8);
        d.push(&head);
        assert_eq!(d.next_frame(), Err(FrameError::TooLarge(MAX_JSON + 1)));

        let mut d = Decoder::new();
        let mut head = ((MAX_PAYLOAD + 1) as u32).to_le_bytes().to_vec();
        head.push(FrameType::Output as u8);
        d.push(&head);
        assert!(d.next_frame().is_err());
    }

    #[test]
    fn keeps_unknown_types_for_the_caller_to_skip() {
        let mut d = Decoder::new();
        d.push(&[2, 0, 0, 0, 0x7f, 1, 2]);
        d.push(&message(&serde_json::json!({})));
        assert_eq!(d.next_frame().unwrap(), Some(Frame::Unknown(0x7f)));
        assert_eq!(
            d.next_frame().unwrap(),
            Some(Frame::Message(b"{}".to_vec()))
        );
    }

    /// The fixtures the TypeScript SDK checks too.
    #[test]
    fn decodes_the_shared_fixtures() {
        let doc: serde_json::Value =
            serde_json::from_str(include_str!("../../../spec/fixtures/frames.json")).unwrap();
        for f in doc["fixtures"].as_array().unwrap() {
            let bytes = crate::handshake::unhex(f["hex"].as_str().unwrap()).unwrap();
            let mut d = Decoder::new();
            d.push(&bytes);
            let got = d.next_frame().unwrap().unwrap();
            let want = &f["frame"];
            let hex = crate::handshake::hex;
            let seq = || want["seq"].as_u64().unwrap();
            let stream = || want["stream"].as_u64().unwrap() as u32;
            match (want["type"].as_str().unwrap(), &got) {
                ("message", Frame::Message(b)) | ("handshake", Frame::Handshake(b)) => {
                    assert_eq!(
                        &serde_json::from_slice::<serde_json::Value>(b).unwrap(),
                        &want["json"]
                    );
                    assert_eq!(
                        b,
                        &serde_json::to_vec(&want["json"]).unwrap(),
                        "{}",
                        f["name"]
                    );
                }
                (
                    "output",
                    Frame::Output {
                        stream: s,
                        seq: q,
                        data,
                    },
                )
                | (
                    "resync",
                    Frame::Resync {
                        stream: s,
                        seq: q,
                        data,
                    },
                ) => {
                    assert_eq!(
                        (*s, *q, hex(data)),
                        (stream(), seq(), want["data"].as_str().unwrap().to_string())
                    );
                }
                ("input", Frame::Input { stream: s, data }) => {
                    assert_eq!(
                        (*s, hex(data)),
                        (stream(), want["data"].as_str().unwrap().to_string())
                    );
                    assert_eq!(input(*s, data), bytes);
                }
                ("unknown", Frame::Unknown(code)) => {
                    assert_eq!(u64::from(*code), want["code"].as_u64().unwrap())
                }
                (t, got) => panic!("{}: expected {t}, got {got:?}", f["name"]),
            }
        }
    }

    #[test]
    fn a_short_binary_frame_is_an_error() {
        let mut d = Decoder::new();
        d.push(&[3, 0, 0, 0, FrameType::Output as u8, 1, 2, 3]);
        assert_eq!(d.next_frame(), Err(FrameError::Short(FrameType::Output)));
    }
}
