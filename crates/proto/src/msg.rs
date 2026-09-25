//! The JSON envelopes: requests, responses and events, and the error codes a response can
//! carry. Fields are read from `serde_json::Value` rather than one struct per op, so a host
//! and a client of different minor versions skip what they do not know.

use serde_json::{json, Map, Value};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    BadRequest,
    NotFound,
    Exited,
    Denied,
    Draining,
    SpawnFailed,
    TooLarge,
    Unsupported,
    Internal,
    Auth,
    UnsupportedProtocol,
}

impl Code {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BadRequest => "bad_request",
            Self::NotFound => "not_found",
            Self::Exited => "exited",
            Self::Denied => "denied",
            Self::Draining => "draining",
            Self::SpawnFailed => "spawn_failed",
            Self::TooLarge => "too_large",
            Self::Unsupported => "unsupported",
            Self::Internal => "internal",
            Self::Auth => "auth",
            Self::UnsupportedProtocol => "unsupported_protocol",
        }
    }
}

/// An error a request is answered with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub code: String,
    pub message: String,
}

impl Error {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code: code.as_str().to_string(),
            message: message.into(),
        }
    }

    pub fn bad(message: impl Into<String>) -> Self {
        Self::new(Code::BadRequest, message)
    }

    pub fn is(&self, code: Code) -> bool {
        self.code == code.as_str()
    }

    pub fn to_value(&self) -> Value {
        json!({ "code": self.code, "message": self.message })
    }

    pub fn from_value(v: &Value) -> Self {
        Self {
            code: v["code"].as_str().unwrap_or("internal").to_string(),
            message: v["message"].as_str().unwrap_or("").to_string(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

pub fn request(id: u64, op: &str, fields: Value) -> Value {
    let mut m = match fields {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    m.insert("id".into(), json!(id));
    m.insert("op".into(), json!(op));
    Value::Object(m)
}

pub fn ok(re: u64, result: Value) -> Value {
    json!({ "re": re, "ok": result })
}

pub fn err(re: u64, e: &Error) -> Value {
    json!({ "re": re, "error": e.to_value() })
}

pub fn event(name: &str, session: Option<&str>, fields: Value) -> Value {
    let mut m = match fields {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    m.insert("ev".into(), json!(name));
    if let Some(s) = session {
        m.insert("session".into(), json!(s));
    }
    Value::Object(m)
}

/// A decoded message: what kind it is, told by which envelope key it carries.
pub enum Incoming {
    Request {
        id: u64,
        op: String,
        body: Value,
    },
    Response {
        re: u64,
        result: Result<Value, Error>,
    },
    Event {
        name: String,
        body: Value,
    },
}

pub fn classify(v: Value) -> Option<Incoming> {
    if let (Some(id), Some(op)) = (
        v.get("id").and_then(Value::as_u64),
        v.get("op").and_then(Value::as_str),
    ) {
        let op = op.to_string();
        return Some(Incoming::Request { id, op, body: v });
    }
    if let Some(re) = v.get("re").and_then(Value::as_u64) {
        if let Some(e) = v.get("error") {
            return Some(Incoming::Response {
                re,
                result: Err(Error::from_value(e)),
            });
        }
        return Some(Incoming::Response {
            re,
            result: Ok(v.get("ok").cloned().unwrap_or(Value::Null)),
        });
    }
    if let Some(name) = v.get("ev").and_then(Value::as_str) {
        let name = name.to_string();
        return Some(Incoming::Event { name, body: v });
    }
    None
}

/// Epoch milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
