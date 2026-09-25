//! What a session starts as: its program found the way a shell would find it, its working
//! directory, and its environment built from what the caller sent, not from whatever
//! environment the host itself happened to start with.

use serde_json::Value;
use std::path::{Path, PathBuf};

pub const DEFAULT_COLS: u16 = 120;
pub const DEFAULT_ROWS: u16 = 32;
pub const DEFAULT_SCROLLBACK: usize = 3000;

#[derive(Clone, Debug, Default)]
pub struct EnvSpec {
    /// Start from nothing rather than from the host's own environment.
    pub empty: bool,
    pub set: Vec<(String, String)>,
    pub unset: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct SpawnSpec {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: EnvSpec,
    pub cols: u16,
    pub rows: u16,
    pub name: Option<String>,
    pub labels: Vec<(String, String)>,
    pub scrollback: usize,
    /// Keep the program running when its last window closes; otherwise that ends it.
    pub keep: bool,
}

fn size_of(v: &Value, key: &str, default: u16) -> Result<u16, String> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(n) => n
            .as_u64()
            .filter(|n| (1..=u64::from(u16::MAX)).contains(n))
            .map(|n| n as u16)
            .ok_or_else(|| format!("{key} must be a positive integer")),
    }
}

/// `{cols, rows}`, each defaulted.
pub fn parse_size(v: Option<&Value>, default: (u16, u16)) -> Result<(u16, u16), String> {
    match v {
        None | Some(Value::Null) => Ok(default),
        Some(v) => Ok((
            size_of(v, "cols", default.0)?,
            size_of(v, "rows", default.1)?,
        )),
    }
}

fn string_map(v: Option<&Value>, what: &str) -> Result<Vec<(String, String)>, String> {
    match v {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Object(m)) => m
            .iter()
            .map(|(k, v)| {
                v.as_str()
                    .map(|s| (k.clone(), s.to_string()))
                    .ok_or_else(|| format!("{what}.{k} must be a string"))
            })
            .collect(),
        Some(_) => Err(format!("{what} must be an object of strings")),
    }
}

/// A `spawn` request's fields.
pub fn parse_spawn(req: &Value) -> Result<SpawnSpec, String> {
    let argv: Vec<String> = req["argv"]
        .as_array()
        .ok_or("argv must be an array of strings")?
        .iter()
        .map(|a| {
            a.as_str()
                .map(String::from)
                .ok_or("argv must be an array of strings")
        })
        .collect::<Result<_, _>>()?;
    if argv.is_empty() || argv[0].is_empty() {
        return Err("argv names no program".into());
    }
    let cwd = match &req["cwd"] {
        Value::Null => None,
        Value::String(s) => Some(PathBuf::from(s)),
        _ => return Err("cwd must be a string".into()),
    };
    if let Some(dir) = &cwd {
        if !dir.is_dir() {
            return Err(format!("cwd {} is not a directory", dir.display()));
        }
    }
    let env = &req["env"];
    let env = EnvSpec {
        empty: match env.get("base").and_then(Value::as_str) {
            None | Some("host") => false,
            Some("empty") => true,
            Some(other) => {
                return Err(format!(
                    "env.base {other:?} is neither \"empty\" nor \"host\""
                ))
            }
        },
        set: string_map(env.get("set"), "env.set")?,
        unset: env
            .get("unset")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
    };
    let (cols, rows) = parse_size(req.get("size"), (DEFAULT_COLS, DEFAULT_ROWS))?;
    let name = req["name"]
        .as_str()
        .map(String::from)
        .filter(|n| !n.is_empty());
    let labels = string_map(req.get("labels"), "labels")?;
    let scrollback = match &req["scrollback"] {
        Value::Null => DEFAULT_SCROLLBACK,
        v => v
            .as_u64()
            .ok_or("scrollback must be a number")?
            .min(100_000) as usize,
    };
    let keep = match &req["keep"] {
        Value::Null => false,
        v => v.as_bool().ok_or("keep must be true or false")?,
    };
    Ok(SpawnSpec {
        argv,
        cwd,
        env,
        cols,
        rows,
        name,
        labels,
        scrollback,
        keep,
    })
}

fn same_name(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// The session's environment: the base, then what was set, then what was unset, then the
/// host's own two additions.
pub fn build_env(spec: &EnvSpec, session: &str) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = if spec.empty {
        Vec::new()
    } else {
        std::env::vars().collect()
    };
    let put = |k: &str, v: &str, env: &mut Vec<(String, String)>| {
        env.retain(|(name, _)| !same_name(name, k));
        env.push((k.to_string(), v.to_string()));
    };
    for (k, v) in &spec.set {
        put(k, v, &mut env);
    }
    for k in &spec.unset {
        env.retain(|(name, _)| !same_name(name, k));
    }
    put("TETHER_SESSION", session, &mut env);
    if cfg!(unix) && !env.iter().any(|(k, _)| k == "TERM") {
        env.push(("TERM".into(), "xterm-256color".into()));
    }
    env
}

fn lookup<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
    env.iter()
        .rev()
        .find(|(k, _)| same_name(k, key))
        .map(|(_, v)| v.as_str())
}

/// The program to run, as the session's own `PATH` finds it, and whatever must run it. On
/// Windows `PATHEXT` is tried before the bare name, because an npm global folder holds a
/// shell script called `claude` beside `claude.cmd` and CreateProcess cannot run the script;
/// a batch file runs under `cmd.exe`. Elsewhere the bare name is looked up in `PATH`.
pub fn resolve(
    argv: &[String],
    env: &[(String, String)],
    cwd: Option<&Path>,
) -> Result<Vec<String>, String> {
    let program = &argv[0];
    let path = Path::new(program);
    let explicit =
        path.is_absolute() || program.contains('/') || (cfg!(windows) && program.contains('\\'));
    let dirs: Vec<PathBuf> = if explicit {
        let base = if path.is_absolute() {
            PathBuf::new()
        } else {
            cwd.map(Path::to_path_buf).unwrap_or_default()
        };
        vec![base]
    } else {
        lookup(env, "PATH")
            .map(|p| std::env::split_paths(p).collect())
            .unwrap_or_default()
    };
    let found = if cfg!(windows) {
        let exts: Vec<String> = lookup(env, "PATHEXT")
            .unwrap_or(".COM;.EXE;.BAT;.CMD")
            .split(';')
            .filter(|e| !e.is_empty())
            .map(|e| e.to_ascii_lowercase())
            .collect();
        let has_ext = path.extension().is_some_and(|e| {
            exts.contains(&format!(".{}", e.to_string_lossy().to_ascii_lowercase()))
        });
        dirs.iter().find_map(|d| {
            let base = d.join(program);
            if has_ext {
                return base.is_file().then_some(base);
            }
            exts.iter()
                .map(|e| PathBuf::from(format!("{}{e}", base.display())))
                .find(|p| p.is_file())
        })
    } else {
        dirs.iter()
            .map(|d| d.join(program))
            .find(|p| is_executable(p))
    };
    let Some(found) = found else {
        return Err(format!("{program} was not found"));
    };
    let found_s = found.to_string_lossy().to_string();
    let ext = found
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());
    let mut out = Vec::with_capacity(argv.len() + 3);
    if cfg!(windows) && matches!(ext.as_deref(), Some("cmd" | "bat")) {
        let comspec = lookup(env, "ComSpec").unwrap_or("cmd.exe").to_string();
        out.extend([comspec, "/d".into(), "/c".into()]);
    }
    out.push(found_s);
    out.extend(argv[1..].iter().cloned());
    Ok(out)
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_and_defaults() {
        let s = parse_spawn(&json!({"argv": ["x", "-y"], "labels": {"a": "b"}, "env": {"base": "empty", "set": {"K": "V"}}})).unwrap();
        assert_eq!(
            (s.cols, s.rows, s.scrollback),
            (DEFAULT_COLS, DEFAULT_ROWS, DEFAULT_SCROLLBACK)
        );
        assert!(s.env.empty);
        assert_eq!(s.labels, vec![("a".to_string(), "b".to_string())]);
        assert!(parse_spawn(&json!({"argv": []})).is_err());
        assert!(parse_spawn(&json!({"argv": ["x"], "size": {"cols": 0}})).is_err());
        assert!(parse_spawn(&json!({"argv": ["x"], "env": {"base": "nope"}})).is_err());
    }

    #[test]
    fn builds_an_environment_from_nothing() {
        let env = build_env(
            &EnvSpec {
                empty: true,
                set: vec![("A".into(), "1".into()), ("B".into(), "2".into())],
                unset: vec!["B".into()],
            },
            "s1",
        );
        assert_eq!(lookup(&env, "A"), Some("1"));
        assert_eq!(lookup(&env, "B"), None);
        assert_eq!(lookup(&env, "TETHER_SESSION"), Some("s1"));
    }

    #[test]
    fn finds_a_program_in_path() {
        let dir = std::env::temp_dir().join(format!("tether-resolve-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (name, file) = if cfg!(windows) {
            ("tool", "tool.cmd")
        } else {
            ("tool", "tool")
        };
        // The extensionless decoy an npm folder holds beside the real shim.
        std::fs::write(dir.join("tool"), "#!/bin/sh\n").unwrap();
        std::fs::write(dir.join(file), "x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.join(file), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let env = vec![("PATH".to_string(), dir.to_string_lossy().to_string())];
        let argv = resolve(&[name.to_string(), "arg".into()], &env, None).unwrap();
        if cfg!(windows) {
            assert_eq!(argv[1..3], ["/d".to_string(), "/c".into()]);
            assert!(argv[3].ends_with("tool.cmd"));
        } else {
            assert!(argv[0].ends_with("tool"));
        }
        assert_eq!(argv.last().unwrap(), "arg");
        assert!(resolve(&["missing-program".into()], &env, None).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
