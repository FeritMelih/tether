//! `tether`: the host, a terminal window on a session, and a command line over the protocol.
//! Every command that talks to a host finds it through the discovery directory; `run` and
//! `spawn` start one when there is none.

mod attach;
mod console;
mod open;
mod profiles;

use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tether_client::{Client, StartOptions};

#[derive(Parser)]
#[command(
    name = "tether",
    version,
    about = "Hold programs in pseudo-terminals that any application can type into, read and watch, while terminal windows attach and detach."
)]
struct Cli {
    /// The state directory (default: TETHER_DIR, else the platform's per-user state folder).
    #[arg(long, global = true)]
    dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a host.
    Serve {
        /// Start it detached and return once it is listening, printing its discovery file.
        #[arg(long)]
        daemonize: bool,
        /// Exit after this many seconds with no session running and no client (0: never).
        #[arg(long, default_value_t = 600)]
        idle_exit: u64,
        /// Keep an exited session, with its last screen, this many seconds.
        #[arg(long, default_value_t = 600)]
        retain: u64,
        #[arg(long, hide = true)]
        stage2: bool,
    },
    /// Start a program in a new session and attach this terminal to it. Closing the terminal
    /// ends the program, unless --keep; Ctrl-] detaches and leaves it running.
    Run {
        #[arg(long)]
        name: Option<String>,
        /// Keep the program running when its last window closes.
        #[arg(long)]
        keep: bool,
        /// A label, `key=value`; repeatable.
        #[arg(long = "label", value_parser = parse_label)]
        labels: Vec<(String, String)>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        title: Option<String>,
        #[arg(trailing_var_arg = true, required = true)]
        argv: Vec<String>,
    },
    /// Attach this terminal to a session.
    Attach {
        session: String,
        /// Watch only: type nothing, size nothing.
        #[arg(long)]
        view: bool,
        /// Type nothing.
        #[arg(long)]
        read_only: bool,
        /// Ctrl-] does not detach.
        #[arg(long)]
        no_detach_key: bool,
        #[arg(long)]
        title: Option<String>,
    },
    /// Open a terminal window attached to a session.
    Open {
        session: String,
        /// auto, wt, console or conhost on Windows; terminal or iterm2 on macOS; a terminal program's name on Linux.
        #[arg(long, default_value = "auto")]
        terminal: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        view: bool,
    },
    /// Start a program in a new session, with no window; prints its id. Once a window has
    /// attached, closing the last one ends the program, unless --keep.
    Spawn {
        #[arg(long)]
        name: Option<String>,
        /// Keep the program running when its last window closes.
        #[arg(long)]
        keep: bool,
        #[arg(long = "label", value_parser = parse_label)]
        labels: Vec<(String, String)>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long, default_value_t = 120)]
        cols: u16,
        #[arg(long, default_value_t = 32)]
        rows: u16,
        #[arg(trailing_var_arg = true, required = true)]
        argv: Vec<String>,
    },
    /// List sessions on every host.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// A session's details.
    Info { session: String },
    /// Paste text into a session (bracketed when it asked for that), then Enter with --enter.
    Send {
        session: String,
        text: String,
        #[arg(long)]
        enter: bool,
    },
    /// Press keys by name: Enter, S-Tab, Down, C-c, 1 …
    Keys {
        session: String,
        #[arg(required = true)]
        keys: Vec<String>,
    },
    /// Print a session's screen.
    Screen {
        session: String,
        /// text, vt or cells.
        #[arg(long, default_value = "text")]
        format: String,
        #[arg(long)]
        scrollback: bool,
    },
    /// End a session's program.
    Kill { session: String },
    /// The current host's details.
    Host,
    /// Tell a host to start nothing new and exit after its last session.
    Drain { host: Option<String> },
    /// Where the hosts' logs are, and the current host's last lines.
    Logs {
        #[arg(long, default_value_t = 40)]
        lines: usize,
    },
    /// Terminal profiles whose sessions start in tether.
    Profiles {
        #[command(subcommand)]
        action: ProfilesAction,
    },
}

#[derive(Subcommand)]
enum ProfilesAction {
    /// Write a profile for Windows Terminal (--wt) or iTerm2 (--iterm2).
    Install {
        #[arg(long)]
        wt: bool,
        #[arg(long)]
        iterm2: bool,
        #[arg(long, default_value = "tether")]
        app: String,
        #[arg(long, default_value = "tether")]
        name: String,
        /// What the profile runs (default: your shell).
        #[arg(trailing_var_arg = true)]
        argv: Vec<String>,
    },
    /// Print a profile instead of writing it.
    Print {
        #[arg(long)]
        iterm2: bool,
        #[arg(long, default_value = "tether")]
        app: String,
        #[arg(long, default_value = "tether")]
        name: String,
        #[arg(trailing_var_arg = true)]
        argv: Vec<String>,
    },
}

fn parse_label(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("{s:?} is not key=value"))
}

fn fail(e: impl std::fmt::Display) -> ! {
    eprintln!("tether: {e}");
    std::process::exit(1)
}

fn exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|e| fail(e))
}

fn print(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

/// The environment this command runs in, for a session that should start with it.
fn own_env() -> Value {
    let set: serde_json::Map<String, Value> = std::env::vars()
        .filter(|(k, _)| k != "TETHER_SESSION")
        .map(|(k, v)| (k, json!(v)))
        .collect();
    json!({ "base": "empty", "set": set })
}

async fn host(dir: &Path) -> Client {
    tether_client::connect_or_start(&StartOptions {
        exe: &exe(),
        dir,
        name: "tether",
        idle_exit: None,
    })
    .await
    .unwrap_or_else(|e| fail(e))
}

async fn session_host(dir: &Path, key: &str) -> (Client, Value) {
    tether_client::find_session(dir, "tether", key)
        .await
        .unwrap_or_else(|e| fail(e))
}

/// Arguments that carry this invocation's state directory to another `tether`.
fn dir_args(dir: &Path, explicit: bool) -> Vec<String> {
    if explicit || std::env::var_os("TETHER_DIR").is_some() {
        vec!["--dir".into(), dir.to_string_lossy().to_string()]
    } else {
        Vec::new()
    }
}

fn main() {
    let cli = Cli::parse();
    let explicit_dir = cli.dir.is_some();
    if let Some(d) = &cli.dir {
        std::env::set_var("TETHER_DIR", d);
    }
    let dir = tether_client::state_dir();

    if let Command::Serve {
        daemonize,
        idle_exit,
        retain,
        stage2,
    } = &cli.command
    {
        let mut args = vec![
            "--idle-exit".to_string(),
            idle_exit.to_string(),
            "--retain".into(),
            retain.to_string(),
        ];
        if explicit_dir {
            args.extend(dir_args(&dir, true));
        }
        #[cfg(unix)]
        if *stage2 {
            tether_server::daemon::stage2(&exe(), &args, &dir).unwrap_or_else(|e| fail(e));
            return;
        }
        #[cfg(windows)]
        let _ = stage2;
        if *daemonize {
            let file =
                tether_server::daemon::daemonize(&exe(), &args, &dir, Duration::from_secs(15))
                    .unwrap_or_else(|e| fail(e));
            println!("{}", serde_json::to_string(&file).unwrap_or_default());
            return;
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap_or_else(|e| fail(e));
        rt.block_on(async {
            let config = tether_server::Config {
                dir: dir.clone(),
                idle_exit: (*idle_exit > 0).then(|| Duration::from_secs(*idle_exit)),
                retain: Duration::from_secs(*retain),
                pty: Arc::new(tether_core::pty::NativePty),
                foreground: true,
            };
            let running = tether_server::start(config)
                .await
                .unwrap_or_else(|e| fail(e));
            let _ = running.done.await;
        });
        // Sessions die with the host; nothing is left to wait for.
        std::process::exit(0);
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap_or_else(|e| fail(e));
    let code = rt.block_on(run(cli.command, dir, explicit_dir));
    std::process::exit(code);
}

async fn run(command: Command, dir: PathBuf, explicit_dir: bool) -> i32 {
    match command {
        Command::Serve { .. } => unreachable!(),
        Command::Run {
            name,
            keep,
            labels,
            cwd,
            title,
            argv,
        } => {
            let client = host(&dir).await;
            let cwd = cwd.or_else(|| std::env::current_dir().ok());
            let size = console::size().unwrap_or((120, 32));
            let labels: serde_json::Map<String, Value> =
                labels.into_iter().map(|(k, v)| (k, json!(v))).collect();
            let r = client
                .request("spawn", json!({ "argv": argv, "cwd": cwd, "env": own_env(), "size": { "cols": size.0, "rows": size.1 }, "name": name, "labels": labels, "keep": keep }))
                .await
                .unwrap_or_else(|e| fail(e));
            let session = r["session"].as_str().unwrap_or_default().to_string();
            attach::attach(
                client,
                &session,
                attach::AttachOptions {
                    read_only: false,
                    view: false,
                    detach_key: true,
                    title,
                },
            )
            .await
        }
        Command::Attach {
            session,
            view,
            read_only,
            no_detach_key,
            title,
        } => {
            let (client, _) = session_host(&dir, &session).await;
            attach::attach(
                client,
                &session,
                attach::AttachOptions {
                    read_only,
                    view,
                    detach_key: !no_detach_key,
                    title,
                },
            )
            .await
        }
        Command::Open {
            session,
            terminal,
            title,
            cwd,
            view,
        } => {
            let (_, info) = session_host(&dir, &session).await;
            let id = info["session"].as_str().unwrap_or(&session).to_string();
            let title = title
                .or_else(|| info["name"].as_str().map(String::from))
                .unwrap_or_else(|| format!("tether {id}"));
            let cwd = cwd.or_else(|| info["cwd"].as_str().map(PathBuf::from));
            let mut attach_args = dir_args(&dir, explicit_dir);
            if view {
                attach_args.push("--view".into());
            }
            let exe = exe();
            let req = open::OpenRequest {
                exe: &exe,
                session: &id,
                title: &title,
                cwd: cwd.as_deref(),
                attach_args,
                terminal: &terminal,
            };
            match open::open(&req) {
                Ok(o) => {
                    let mut out = json!({ "session": id, "terminal": o.terminal });
                    if let Some(pid) = o.pid {
                        out["pid"] = json!(pid);
                    }
                    print(&out);
                    0
                }
                Err(e) => fail(e),
            }
        }
        Command::Spawn {
            name,
            keep,
            labels,
            cwd,
            cols,
            rows,
            argv,
        } => {
            let client = host(&dir).await;
            let cwd = cwd.or_else(|| std::env::current_dir().ok());
            let labels: serde_json::Map<String, Value> =
                labels.into_iter().map(|(k, v)| (k, json!(v))).collect();
            let mut r = client
                .request("spawn", json!({ "argv": argv, "cwd": cwd, "env": own_env(), "size": { "cols": cols, "rows": rows }, "name": name, "labels": labels, "keep": keep }))
                .await
                .unwrap_or_else(|e| fail(e));
            r["host"] = json!(client.host.host);
            print(&r);
            0
        }
        Command::Ls { json } => {
            let mut all = Vec::new();
            for h in tether_client::live_hosts(&dir) {
                let Ok(c) = Client::connect(&h, "tether").await else {
                    continue;
                };
                let Ok(r) = c.request("list", json!({})).await else {
                    continue;
                };
                for mut s in r["sessions"].as_array().cloned().unwrap_or_default() {
                    s["host"] = json!(h.host);
                    all.push(s);
                }
            }
            if json {
                print(&json!({ "sessions": all }));
            } else if all.is_empty() {
                println!("no sessions");
            } else {
                for s in all {
                    let argv = s["argv"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                        .unwrap_or_default();
                    let windows = s["clients"]
                        .as_array()
                        .map(|c| c.iter().filter(|c| c["role"] == "window").count())
                        .unwrap_or(0);
                    println!(
                        "{}  {:<8} {:>7}  {}x{}  {} window(s)  {}{}",
                        s["session"].as_str().unwrap_or(""),
                        s["status"].as_str().unwrap_or(""),
                        s["pid"].as_u64().map(|p| p.to_string()).unwrap_or_default(),
                        s["cols"],
                        s["rows"],
                        windows,
                        s["name"]
                            .as_str()
                            .map(|n| format!("[{n}] "))
                            .unwrap_or_default(),
                        argv
                    );
                }
            }
            0
        }
        Command::Info { session } => {
            let (_, info) = session_host(&dir, &session).await;
            print(&info);
            0
        }
        Command::Send {
            session,
            text,
            enter,
        } => {
            let (c, _) = session_host(&dir, &session).await;
            c.request("paste", json!({ "session": session, "text": text }))
                .await
                .unwrap_or_else(|e| fail(e));
            if enter {
                tokio::time::sleep(Duration::from_millis(300)).await;
                c.request("keys", json!({ "session": session, "keys": ["Enter"] }))
                    .await
                    .unwrap_or_else(|e| fail(e));
            }
            0
        }
        Command::Keys { session, keys } => {
            let (c, _) = session_host(&dir, &session).await;
            c.request("keys", json!({ "session": session, "keys": keys }))
                .await
                .unwrap_or_else(|e| fail(e));
            0
        }
        Command::Screen {
            session,
            format,
            scrollback,
        } => {
            let (c, _) = session_host(&dir, &session).await;
            let s = c
                .request(
                    "screen",
                    json!({ "session": session, "format": format, "scrollback": scrollback }),
                )
                .await
                .unwrap_or_else(|e| fail(e));
            match format.as_str() {
                "text" => {
                    for l in s["lines"].as_array().cloned().unwrap_or_default() {
                        println!("{}", l.as_str().unwrap_or(""));
                    }
                }
                "vt" => print!("{}", s["data"].as_str().unwrap_or("")),
                _ => print(&s),
            }
            0
        }
        Command::Kill { session } => {
            let (c, _) = session_host(&dir, &session).await;
            c.request("kill", json!({ "session": session }))
                .await
                .unwrap_or_else(|e| fail(e));
            0
        }
        Command::Host => {
            let Some(h) = tether_client::current_host(&dir) else {
                fail("no host is running")
            };
            let c = Client::connect(&h, "tether")
                .await
                .unwrap_or_else(|e| fail(e));
            print(
                &c.request("host", json!({}))
                    .await
                    .unwrap_or_else(|e| fail(e)),
            );
            0
        }
        Command::Drain { host } => {
            let hosts = tether_client::live_hosts(&dir);
            let target = match &host {
                Some(id) => hosts.into_iter().find(|h| &h.host == id),
                None => tether_client::current_host(&dir),
            };
            let Some(h) = target else {
                fail("no such host")
            };
            let c = Client::connect(&h, "tether")
                .await
                .unwrap_or_else(|e| fail(e));
            c.request("drain", json!({}))
                .await
                .unwrap_or_else(|e| fail(e));
            println!("host {} is draining", h.host);
            0
        }
        Command::Logs { lines } => {
            let logs = tether_proto::discovery::logs_dir(&dir);
            println!("{}", logs.display());
            if let Some(h) = tether_client::current_host(&dir) {
                let text = std::fs::read_to_string(logs.join(format!("{}.log", h.host)))
                    .unwrap_or_default();
                let all: Vec<&str> = text.lines().collect();
                for l in &all[all.len().saturating_sub(lines)..] {
                    println!("{l}");
                }
            }
            0
        }
        Command::Profiles { action } => {
            let exe = exe().to_string_lossy().to_string();
            let run_args = dir_args(&dir, explicit_dir);
            match action {
                ProfilesAction::Install {
                    wt,
                    iterm2,
                    app,
                    name,
                    argv,
                } => {
                    let p = profiles::Profile {
                        app,
                        name,
                        command: if argv.is_empty() {
                            profiles::default_shell()
                        } else {
                            argv
                        },
                        run_args,
                    };
                    let (wt, iterm2) = if !wt && !iterm2 {
                        (cfg!(windows), cfg!(target_os = "macos"))
                    } else {
                        (wt, iterm2)
                    };
                    if !wt && !iterm2 {
                        fail("no terminal here takes profiles; run `tether run -- <program>` from your terminal's own profile");
                    }
                    if wt {
                        let path = profiles::wt_path(&p).unwrap_or_else(|e| fail(e));
                        let (path, changed) =
                            profiles::install(path, &profiles::windows_terminal(&exe, &p))
                                .unwrap_or_else(|e| fail(e));
                        println!(
                            "{} {}",
                            if changed { "wrote" } else { "unchanged" },
                            path.display()
                        );
                    }
                    if iterm2 {
                        let path = profiles::iterm_path(&p).unwrap_or_else(|e| fail(e));
                        let (path, changed) = profiles::install(path, &profiles::iterm2(&exe, &p))
                            .unwrap_or_else(|e| fail(e));
                        println!(
                            "{} {}",
                            if changed { "wrote" } else { "unchanged" },
                            path.display()
                        );
                    }
                    0
                }
                ProfilesAction::Print {
                    iterm2,
                    app,
                    name,
                    argv,
                } => {
                    let p = profiles::Profile {
                        app,
                        name,
                        command: if argv.is_empty() {
                            profiles::default_shell()
                        } else {
                            argv
                        },
                        run_args,
                    };
                    print!(
                        "{}",
                        if iterm2 {
                            profiles::iterm2(&exe, &p)
                        } else {
                            profiles::windows_terminal(&exe, &p)
                        }
                    );
                    0
                }
            }
        }
    }
}
