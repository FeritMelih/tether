//! The conformance scenarios in `spec/conformance`, through the Rust client against an
//! in-process host; the TypeScript SDK runs the same files.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tether_client::{Client, StreamItem, Subscription};
use tokio::sync::mpsc;

fn substitute(v: &Value, vars: &HashMap<String, String>) -> Value {
    match v {
        Value::String(s) if s.starts_with('$') => vars
            .get(&s[1..])
            .map(|x| json!(x))
            .unwrap_or_else(|| v.clone()),
        Value::Array(a) => Value::Array(a.iter().map(|x| substitute(x, vars)).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), substitute(x, vars)))
                .collect(),
        ),
        _ => v.clone(),
    }
}

fn at<'a>(v: &'a Value, path: &str) -> &'a Value {
    path.split('.')
        .fold(v, |v, key| match key.parse::<usize>() {
            Ok(i) if v.is_array() => &v[i],
            _ => &v[key],
        })
}

struct Run {
    client: Client,
    vars: HashMap<String, String>,
    events: mpsc::UnboundedReceiver<Value>,
    subs: Vec<Subscription>,
}

impl Run {
    async fn screen(&self, session: &str) -> String {
        let s = self
            .client
            .request("screen", json!({ "session": session }))
            .await
            .unwrap();
        s["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n")
    }

    async fn step(&mut self, name: &str, raw: &Value) {
        let step = substitute(raw, &self.vars);
        if let Some(op) = step["request"].as_str() {
            let r = self.client.request(op, step["args"].clone()).await;
            match (step["error"].as_str(), r) {
                (Some(code), Err(e)) => assert_eq!(e.code, code, "{name}: {op}"),
                (Some(code), Ok(v)) => {
                    panic!("{name}: {op} answered {v} where {code} was expected")
                }
                (None, Err(e)) => panic!("{name}: {op} failed: {e}"),
                (None, Ok(v)) => {
                    for (path, want) in step["expect"].as_object().into_iter().flatten() {
                        assert_eq!(at(&v, path), want, "{name}: {op} {path}");
                    }
                    for (var, path) in step["save"].as_object().into_iter().flatten() {
                        let got = at(&v, path.as_str().unwrap());
                        self.vars.insert(
                            var.clone(),
                            got.as_str()
                                .map(String::from)
                                .unwrap_or_else(|| got.to_string()),
                        );
                    }
                }
            }
        } else if let Some(session) = step["type"].as_str() {
            self.client
                .request(
                    "paste",
                    json!({ "session": session, "text": step["text"], "bracketed": false }),
                )
                .await
                .unwrap();
            self.client
                .request("keys", json!({ "session": session, "keys": ["Enter"] }))
                .await
                .unwrap();
        } else if let Some(session) = step["wait"].as_str() {
            let needle = step["contains"].as_str().unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let text = self.screen(session).await;
                if text.contains(needle) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "{name}: no {needle:?} on the screen:\n{text}"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        } else if step.get("subscribe").is_some() {
            let mut sub = self
                .client
                .subscribe(step["subscribe"].clone())
                .await
                .unwrap();
            if let Some(after) = raw.get("after") {
                Box::pin(self.step(name, after)).await;
            }
            let needle = step["until"].as_str().unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut text = String::new();
            let mut first = true;
            while !text.contains(needle) {
                let item = tokio::time::timeout(
                    deadline.saturating_duration_since(Instant::now()),
                    sub.rx.recv(),
                )
                .await
                .unwrap_or_else(|_| panic!("{name}: the stream never showed {needle:?}: {text:?}"));
                let data = match item {
                    Some(StreamItem::Resync { data, .. }) => data,
                    Some(StreamItem::Output { data, .. }) => {
                        assert!(
                            !(first && step["resync"] == true),
                            "{name}: the stream did not start with a resync"
                        );
                        data
                    }
                    None => panic!("{name}: the stream ended"),
                };
                first = false;
                text.push_str(&String::from_utf8_lossy(&data));
            }
            self.subs.push(sub);
        } else if let Some(ev) = step["event"].as_str() {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let e = tokio::time::timeout(
                    deadline.saturating_duration_since(Instant::now()),
                    self.events.recv(),
                )
                .await
                .unwrap_or_else(|_| panic!("{name}: no {ev} event"))
                .unwrap();
                // Another session's event (one a scenario before ended, say) is not this one.
                if e["ev"] != ev
                    || step["expect"]
                        .get("session")
                        .is_some_and(|s| *s != e["session"])
                {
                    continue;
                }
                for (path, want) in step["expect"].as_object().into_iter().flatten() {
                    assert_eq!(at(&e, path), want, "{name}: {ev} {path}");
                }
                break;
            }
        } else {
            panic!("{name}: a step of no known kind: {step}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_scenario_passes() {
    let dir = std::env::temp_dir().join(format!("tether-conformance-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let running = tether_server::start(tether_server::Config {
        dir,
        idle_exit: None,
        retain: Duration::from_secs(60),
        pty: Arc::new(tether_core::pty::NativePty),
        foreground: false,
    })
    .await
    .unwrap();
    let spec = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/conformance");
    let mut files: Vec<_> = std::fs::read_dir(&spec)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    assert!(files.len() >= 5, "the scenarios are in {}", spec.display());
    for file in files {
        let scenario: Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let name = format!(
            "{} ({})",
            scenario["name"].as_str().unwrap(),
            file.file_name().unwrap().to_string_lossy()
        );
        let client = Client::connect(&running.file, "conformance").await.unwrap();
        let events = client.events();
        let mut run = Run {
            client,
            vars: HashMap::from([(
                "TESTTUI".to_string(),
                env!("CARGO_BIN_EXE_testtui").to_string(),
            )]),
            events,
            subs: Vec::new(),
        };
        for step in scenario["steps"].as_array().unwrap() {
            run.step(&name, step).await;
        }
    }
    running.host.stop();
}
