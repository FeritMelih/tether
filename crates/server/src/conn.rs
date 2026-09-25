//! One connection: the handshake, then requests in and responses, events and streams out.
//! Everything bound for the client goes through one outbox, written in order by one writer;
//! the reader handles requests one at a time, so a client's requests are answered in the
//! order it sent them.

use crate::host::Host;
use crate::transport::Stream;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tether_core::outbox::{Out, Outbox};
use tether_core::session::{Client, Session};
use tether_proto::frame::{self, Decoder, Frame};
use tether_proto::handshake::{self, hex, unhex, CLIENT_LABEL, HOST_LABEL};
use tether_proto::msg::{self, Code, Error, Incoming};
use tether_proto::PROTOCOL;
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

async fn read_frame(
    rd: &mut ReadHalf<Box<dyn Stream>>,
    dec: &mut Decoder,
    buf: &mut [u8],
) -> Result<Option<Frame>, String> {
    loop {
        if let Some(f) = dec.next_frame().map_err(|e| e.to_string())? {
            return Ok(Some(f));
        }
        let n = rd.read(buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(None);
        }
        dec.push(&buf[..n]);
    }
}

async fn read_handshake(
    rd: &mut ReadHalf<Box<dyn Stream>>,
    dec: &mut Decoder,
    buf: &mut [u8],
) -> Result<Value, String> {
    match read_frame(rd, dec, buf).await? {
        Some(Frame::Handshake(b)) => serde_json::from_slice(&b).map_err(|e| e.to_string()),
        Some(_) => Err("expected a handshake frame".into()),
        None => Err("closed during the handshake".into()),
    }
}

/// The handshake, from the host's side. The client's hello, name and pid when it passes.
async fn handshake(
    host: &Host,
    rd: &mut ReadHalf<Box<dyn Stream>>,
    wr: &mut tokio::io::WriteHalf<Box<dyn Stream>>,
    dec: &mut Decoder,
    buf: &mut [u8],
) -> Result<Value, String> {
    let hello = read_handshake(rd, dec, buf).await?;
    if hello["hello"] != "tether" {
        return Err("not a tether client".into());
    }
    let (min, max) = (
        hello["protocol"][0].as_u64().unwrap_or(0),
        hello["protocol"][1].as_u64().unwrap_or(0),
    );
    if !(min..=max).contains(&u64::from(PROTOCOL)) {
        let e = Error::new(
            Code::UnsupportedProtocol,
            format!("this host speaks protocol {PROTOCOL}, the client {min} to {max}"),
        );
        let _ = wr
            .write_all(&frame::handshake(&json!({ "error": e.to_value() })))
            .await;
        return Err(e.message);
    }
    let client_nonce = hello["nonce"]
        .as_str()
        .and_then(unhex)
        .filter(|n| n.len() == handshake::NONCE_BYTES)
        .ok_or("the hello has no nonce")?;
    let host_nonce = handshake::nonce();
    let proof = handshake::proof(&host.token, HOST_LABEL, &client_nonce, &host_nonce);
    wr.write_all(&frame::handshake(
        &json!({ "protocol": PROTOCOL, "nonce": hex(&host_nonce), "proof": hex(&proof) }),
    ))
    .await
    .map_err(|e| e.to_string())?;
    let answer = read_handshake(rd, dec, buf).await?;
    let given = answer["proof"].as_str().and_then(unhex).unwrap_or_default();
    if !handshake::verify(
        &host.token,
        CLIENT_LABEL,
        &client_nonce,
        &host_nonce,
        &given,
    ) {
        let e = Error::new(Code::Auth, "the client's proof is wrong");
        let _ = wr
            .write_all(&frame::handshake(&json!({ "error": e.to_value() })))
            .await;
        return Err(e.message);
    }
    wr.write_all(&frame::handshake(&json!({ "welcome": host.welcome() })))
        .await
        .map_err(|e| e.to_string())?;
    Ok(hello)
}

pub async fn serve(host: Arc<Host>, stream: Box<dyn Stream>, peer_pid: Option<u32>) {
    let (mut rd, mut wr) = tokio::io::split(stream);
    let mut dec = Decoder::new();
    let mut buf = vec![0u8; 64 * 1024];
    let hello = match tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        handshake(&host, &mut rd, &mut wr, &mut dec, &mut buf),
    )
    .await
    {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            crate::log!("handshake refused (pid {peer_pid:?}): {e}");
            return;
        }
        Err(_) => {
            crate::log!("handshake timed out (pid {peer_pid:?})");
            return;
        }
    };
    let conn = host.next_conn();
    let client = Client {
        conn,
        pid: peer_pid.or_else(|| hello["client"]["pid"].as_u64().map(|p| p as u32)),
        name: hello["client"]["name"].as_str().map(String::from),
    };
    let (outbox, mut rx) = Outbox::new(conn);
    host.register(outbox.clone());
    crate::log!(
        "client c{conn} connected: {} pid {:?}",
        client.name.as_deref().unwrap_or("?"),
        client.pid
    );

    let writer = tokio::spawn(async move {
        while let Some(out) = rx.recv().await {
            let (bytes, budget) = match out {
                Out::Frame(b) => (b, None),
                Out::Stream { bytes, budget } => (bytes, Some(budget)),
            };
            let ok = wr.write_all(&bytes).await.is_ok();
            if let Some(b) = budget {
                b.written(bytes.len());
            }
            if !ok {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });

    let mut streams: HashMap<u32, Session> = HashMap::new();
    'read: loop {
        loop {
            let f = match dec.next_frame() {
                Ok(Some(f)) => f,
                Ok(None) => break,
                Err(e) => {
                    crate::log!("client c{conn}: {e}");
                    break 'read;
                }
            };
            match f {
                Frame::Message(b) => {
                    let Some(Incoming::Request { id, op, body }) =
                        serde_json::from_slice::<Value>(&b)
                            .ok()
                            .and_then(msg::classify)
                    else {
                        continue;
                    };
                    if op == "subscribe" {
                        match host.find(body["session"].as_str().unwrap_or("")) {
                            Ok(s) => {
                                let stream = host.new_stream();
                                if s.subscribe(client.clone(), outbox.clone(), id, body, stream) {
                                    streams.insert(stream, s);
                                } else {
                                    outbox.send(frame::message(&msg::err(
                                        id,
                                        &Error::new(Code::NotFound, "the session is gone"),
                                    )));
                                }
                            }
                            Err(e) => {
                                outbox.send(frame::message(&msg::err(id, &e)));
                            }
                        }
                        continue;
                    }
                    let result = if op == "unsubscribe" {
                        let stream = body["stream"].as_u64().unwrap_or(0) as u32;
                        match streams.remove(&stream) {
                            Some(s) => s.request(&client, "unsubscribe", body).await,
                            None => Err(Error::new(Code::NotFound, format!("no stream {stream}"))),
                        }
                    } else {
                        host.request(&client, &op, body).await
                    };
                    let reply = match result {
                        Ok(v) => msg::ok(id, v),
                        Err(e) => msg::err(id, &e),
                    };
                    outbox.send(frame::message(&reply));
                }
                Frame::Input { stream, data } => {
                    if let Some(s) = streams.get(&stream) {
                        s.input(conn, stream, data);
                    }
                }
                _ => {}
            }
        }
        match rd.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => dec.push(&buf[..n]),
        }
    }
    host.closed(conn);
    drop(outbox);
    writer.abort();
    crate::log!("client c{conn} disconnected");
}
