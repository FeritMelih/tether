//! A host in-process, over its real endpoint and a real pseudo-terminal, with testtui as the
//! program: what a client sees end to end.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tether_client::{Client, StreamItem, Subscription};
use tether_proto::discovery::HostFile;

struct Rig {
    dir: PathBuf,
    file: HostFile,
    running: tether_server::Running,
}

async fn host(name: &str, idle_exit: Option<Duration>) -> Rig {
    let dir = std::env::temp_dir().join(format!("tether-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let running = tether_server::start(tether_server::Config {
        dir: dir.clone(),
        idle_exit,
        retain: Duration::from_secs(60),
        pty: Arc::new(tether_core::pty::NativePty),
        foreground: false,
    })
    .await
    .unwrap();
    Rig {
        dir,
        file: running.file.clone(),
        running,
    }
}

fn env() -> Value {
    let set: serde_json::Map<String, Value> =
        std::env::vars().map(|(k, v)| (k, json!(v))).collect();
    json!({ "base": "empty", "set": set })
}

async fn spawn(c: &Client, size: (u16, u16)) -> String {
    let r = c.request("spawn", json!({ "argv": [env!("CARGO_BIN_EXE_testtui")], "env": env(), "size": { "cols": size.0, "rows": size.1 }, "name": "tui" })).await.unwrap();
    r["session"].as_str().unwrap().to_string()
}

/// Reads a subscription into a model of the screen until `until` holds on its text.
async fn read_until(
    sub: &mut Subscription,
    model: &mut vt100::Parser,
    until: impl Fn(&str) -> bool,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut raw = String::new();
    loop {
        let text = model.screen().contents();
        if until(&text) || until(&raw) {
            return text;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, sub.rx.recv()).await {
            Ok(Some(StreamItem::Output { data, .. }))
            | Ok(Some(StreamItem::Resync { data, .. })) => {
                raw.push_str(&String::from_utf8_lossy(&data));
                model.process(&data);
            }
            Ok(None) => panic!("the stream ended; screen: {text}"),
            Err(_) => panic!("timed out; screen:\n{text}\nraw: {raw:?}"),
        }
    }
}

async fn screen_text(c: &Client, s: &str) -> String {
    let v = c.request("screen", json!({ "session": s })).await.unwrap();
    v["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect::<Vec<_>>()
        .join("\n")
}

async fn wait_screen(c: &Client, s: &str, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let text = screen_text(c, s).await;
        if text.contains(needle) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "no {needle:?} on the screen:\n{text}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_with_no_window_gets_its_cursor_query_answered() {
    let rig = host("cpr", None).await;
    let c = Client::connect(&rig.file, "test").await.unwrap();
    let s = spawn(&c, (100, 30)).await;
    let text = wait_screen(&c, &s, "SIZE").await;
    assert!(text.contains("READY"), "{text}");
    assert!(
        text.contains("CPR 2 1"),
        "the host answered from its model: {text}"
    );
    assert!(text.contains("SIZE 100 30"), "{text}");
    let info = c
        .request("info", json!({ "session": "tui" }))
        .await
        .unwrap();
    assert_eq!(info["session"], s);
    assert_eq!(info["status"], "running");
    assert!(info["pid"].as_u64().is_some());
    c.request("kill", json!({ "session": s })).await.unwrap();
    rig.running.host.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keys_and_pastes_arrive_as_a_terminal_sends_them() {
    let rig = host("keys", None).await;
    let c = Client::connect(&rig.file, "test").await.unwrap();
    let s = spawn(&c, (100, 30)).await;
    wait_screen(&c, &s, "SIZE").await;
    c.request(
        "keys",
        json!({ "session": s, "keys": ["Down", "S-Tab", "C-c"] }),
    )
    .await
    .unwrap();
    wait_screen(&c, &s, "IN 1b5b42").await;
    let text = wait_screen(&c, &s, "03").await;
    assert!(text.contains("1b5b5a"), "{text}");
    c.request("paste", json!({ "session": s, "text": "bracketed" }))
        .await
        .unwrap();
    c.request("keys", json!({ "session": s, "keys": ["Enter"] }))
        .await
        .unwrap();
    wait_screen(&c, &s, "BRACKETED").await;
    let r = c
        .request("paste", json!({ "session": s, "text": "a\nb" }))
        .await
        .unwrap();
    assert_eq!(r["bracketed"], true);
    // ConPTY may hand the program the markers and the text in separate reads; the bytes are
    // what matter.
    let typed = "1b5b3230307e610d621b5b3230317e";
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let text = screen_text(&c, &s).await;
        let joined: String = text.lines().filter_map(|l| l.strip_prefix("IN ")).collect();
        if joined.contains(typed) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the paste did not arrive whole:\n{text}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    c.request("kill", json!({ "session": s })).await.unwrap();
    rig.running.host.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_window_attaching_late_gets_the_scrollback_then_the_stream() {
    let rig = host("repaint", None).await;
    let c = Client::connect(&rig.file, "test").await.unwrap();
    let s = spawn(&c, (60, 10)).await;
    wait_screen(&c, &s, "SIZE").await;
    c.request(
        "paste",
        json!({ "session": s, "text": "lines 40", "bracketed": false }),
    )
    .await
    .unwrap();
    c.request("keys", json!({ "session": s, "keys": ["Enter"] }))
        .await
        .unwrap();
    wait_screen(&c, &s, "line 40").await;

    let w = Client::connect(&rig.file, "window").await.unwrap();
    let mut sub = w
        .subscribe(json!({ "session": s, "role": "window", "scrollback": true }))
        .await
        .unwrap();
    let mut model = vt100::Parser::new(10, 60, 1000);
    read_until(&mut sub, &mut model, |t| t.contains("line 40")).await;
    let lines = tether_core::screen::text(model.screen_mut(), true);
    assert!(
        lines.iter().any(|l| l == "line 1"),
        "the scrollback came with the repaint: {lines:?}"
    );
    assert!(lines.iter().any(|l| l == "READY"), "{lines:?}");

    // Live output follows the repaint on the same model.
    c.request(
        "paste",
        json!({ "session": s, "text": "bell", "bracketed": false }),
    )
    .await
    .unwrap();
    c.request("keys", json!({ "session": s, "keys": ["Enter"] }))
        .await
        .unwrap();
    read_until(&mut sub, &mut model, |t| t.contains("RANG")).await;
    let info = c.request("info", json!({ "session": s })).await.unwrap();
    let clients = info["clients"].as_array().unwrap();
    assert_eq!(clients.len(), 1);
    assert_eq!(clients[0]["role"], "window");
    assert_eq!(clients[0]["name"], "window");
    assert_eq!(clients[0]["pid"], std::process::id());
    c.request("kill", json!({ "session": s })).await.unwrap();
    rig.running.host.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_last_window_to_type_sets_the_size_and_viewers_follow() {
    let rig = host("sizing", None).await;
    let c = Client::connect(&rig.file, "test").await.unwrap();
    let s = spawn(&c, (80, 24)).await;
    wait_screen(&c, &s, "SIZE 80 24").await;

    let a = Client::connect(&rig.file, "a").await.unwrap();
    let sa = a.subscribe(json!({ "session": s, "from": "now", "input": true, "sizing": true, "size": { "cols": 100, "rows": 30 } })).await.unwrap();
    wait_screen(&c, &s, "SIZE 100 30").await;
    let b = Client::connect(&rig.file, "b").await.unwrap();
    let _sb = b.subscribe(json!({ "session": s, "from": "now", "input": true, "sizing": true, "size": { "cols": 90, "rows": 25 } })).await.unwrap();
    wait_screen(&c, &s, "SIZE 90 25").await;
    let v = Client::connect(&rig.file, "viewer").await.unwrap();
    let mut events = v.events();
    let _sv = v
        .subscribe(json!({ "session": s, "from": "now", "size": { "cols": 40, "rows": 10 } }))
        .await
        .unwrap();

    a.input(sa.stream, b"x");
    wait_screen(&c, &s, "SIZE 100 30").await;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let e = tokio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            events.recv(),
        )
        .await
        .unwrap()
        .unwrap();
        if e["ev"] == "resized" && e["cols"] == 100 {
            break;
        }
    }
    let info = c.request("info", json!({ "session": s })).await.unwrap();
    assert_eq!(
        (info["cols"].as_u64(), info["rows"].as_u64()),
        (Some(100), Some(30))
    );
    c.request("kill", json!({ "session": s })).await.unwrap();
    rig.running.host.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exit_reaches_the_window_with_its_code_and_the_session_is_kept() {
    let rig = host("exit", None).await;
    let c = Client::connect(&rig.file, "test").await.unwrap();
    let mut events = c.events();
    c.request("watch", json!({})).await.unwrap();
    let s = spawn(&c, (80, 24)).await;
    wait_screen(&c, &s, "SIZE").await;
    c.request(
        "paste",
        json!({ "session": s, "text": "exit 7", "bracketed": false }),
    )
    .await
    .unwrap();
    c.request("keys", json!({ "session": s, "keys": ["Enter"] }))
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let exited = loop {
        let e = tokio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            events.recv(),
        )
        .await
        .expect("an exit")
        .unwrap();
        if e["ev"] == "exited" {
            break e;
        }
    };
    assert_eq!(exited["session"], s);
    assert_eq!(exited["code"], 7);
    let info = c.request("info", json!({ "session": s })).await.unwrap();
    assert_eq!(info["status"], "exited");
    assert!(screen_text(&c, &s).await.contains("BYE"));
    c.request("remove", json!({ "session": s })).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(c
        .request("info", json!({ "session": s }))
        .await
        .unwrap_err()
        .is(tether_proto::msg::Code::NotFound));
    rig.running.host.stop();
}

/// Types a command line into testtui.
async fn command(c: &Client, s: &str, line: &str) {
    c.request(
        "paste",
        json!({ "session": s, "text": line, "bracketed": false }),
    )
    .await
    .unwrap();
    c.request("keys", json!({ "session": s, "keys": ["Enter"] }))
        .await
        .unwrap();
}

/// Waits for a session's `exited` event.
async fn exited(events: &mut tokio::sync::mpsc::UnboundedReceiver<Value>, s: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let e = tokio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            events.recv(),
        )
        .await
        .expect("an exit")
        .unwrap();
        if e["ev"] == "exited" && e["session"] == s {
            return e;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_the_last_window_ends_the_program_and_what_it_started() {
    let rig = host("close", None).await;
    let c = Client::connect(&rig.file, "test").await.unwrap();
    let mut events = c.events();
    c.request("watch", json!({})).await.unwrap();
    let s = spawn(&c, (80, 24)).await;
    wait_screen(&c, &s, "SIZE").await;
    command(&c, &s, "child").await;
    let text = wait_screen(&c, &s, "CHILD").await;
    let child: u32 = text
        .lines()
        .find_map(|l| l.strip_prefix("CHILD "))
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or_else(|| panic!("no child started:\n{text}"));
    assert!(tether_client::process_alive(child));

    // A window that detaches leaves it running.
    let w = Client::connect(&rig.file, "window").await.unwrap();
    let sub = w
        .subscribe(json!({ "session": s, "role": "window" }))
        .await
        .unwrap();
    w.request("unsubscribe", json!({ "stream": sub.stream }))
        .await
        .unwrap();
    drop(w);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let info = c.request("info", json!({ "session": s })).await.unwrap();
    assert_eq!(info["status"], "running");

    // One that is closed, the last, takes the program with it, and what the program started
    // outside its terminal.
    let w = Client::connect(&rig.file, "window").await.unwrap();
    let _sub = w
        .subscribe(json!({ "session": s, "role": "window" }))
        .await
        .unwrap();
    drop(w);
    exited(&mut events, &s).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while tether_client::process_alive(child) {
        assert!(
            Instant::now() < deadline,
            "the child {child} outlived the session"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    rig.running.host.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kept_session_runs_on_when_its_last_window_closes() {
    let rig = host("keep", None).await;
    let c = Client::connect(&rig.file, "test").await.unwrap();
    let r = c
        .request(
            "spawn",
            json!({ "argv": [env!("CARGO_BIN_EXE_testtui")], "env": env(), "keep": true }),
        )
        .await
        .unwrap();
    let s = r["session"].as_str().unwrap().to_string();
    wait_screen(&c, &s, "SIZE").await;
    let w = Client::connect(&rig.file, "window").await.unwrap();
    let _sub = w
        .subscribe(json!({ "session": s, "role": "window" }))
        .await
        .unwrap();
    drop(w);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let info = c.request("info", json!({ "session": s })).await.unwrap();
    assert_eq!(
        (info["status"].as_str(), info["keep"].as_bool()),
        (Some("running"), Some(true))
    );
    c.request("kill", json!({ "session": s })).await.unwrap();
    rig.running.host.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_without_the_token_is_refused() {
    let rig = host("auth", None).await;
    let mut wrong = rig.file.clone();
    wrong.token = "00".repeat(32);
    let e = Client::connect(&wrong, "impostor")
        .await
        .err()
        .expect("refused");
    assert_eq!(e.code, "auth");
    rig.running.host.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_draining_host_starts_nothing_and_leaves_after_its_last_session() {
    let rig = host("drain", None).await;
    let c = Client::connect(&rig.file, "test").await.unwrap();
    let s = spawn(&c, (80, 24)).await;
    wait_screen(&c, &s, "SIZE").await;
    c.request("drain", json!({})).await.unwrap();
    let e = c
        .request("spawn", json!({ "argv": [env!("CARGO_BIN_EXE_testtui")] }))
        .await
        .unwrap_err();
    assert!(e.is(tether_proto::msg::Code::Draining));
    let file: HostFile = serde_json::from_slice(
        &std::fs::read(tether_proto::discovery::host_path(&rig.dir, &rig.file.host)).unwrap(),
    )
    .unwrap();
    assert!(file.draining);
    assert!(!rig.running.done.is_finished());
    c.request("kill", json!({ "session": s })).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), rig.running.done)
        .await
        .expect("the host left")
        .unwrap();
    assert!(!tether_proto::discovery::host_path(&rig.dir, &rig.file.host).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_host_leaves() {
    let rig = host("idle", Some(Duration::from_millis(500))).await;
    let c = Client::connect(&rig.file, "test").await.unwrap();
    c.request("ping", json!({})).await.unwrap();
    drop(c);
    tokio::time::timeout(Duration::from_secs(10), rig.running.done)
        .await
        .expect("the host left")
        .unwrap();
    assert_eq!(tether_proto::discovery::read_current(&rig.dir), None);
}
