# The tether protocol, version 1

This document is normative. A client in any language that follows it can drive a tether
host; the Rust client (`crates/client`) and the TypeScript SDK (`sdk/typescript`) are two
such clients, and `spec/conformance/` holds scenarios both run against the real host.

The key words MUST, SHOULD and MAY are used as in RFC 2119.

## Model

- **Host.** One process per user per machine (more than one only while an upgrade drains
  the old one). It owns sessions and listens on one local endpoint. It never listens on a
  network.
- **Session.** A program started in a pseudo-terminal (ConPTY on Windows, a pty elsewhere)
  that the host holds. It has an id, an optional name, labels, a size, a screen model fed
  from its output, and a ring of its most recent raw output. It keeps running whether or not
  anything is connected, until the last window on it closes (see [Lifetime](#lifetime)).
- **Connection.** A client's link to the host. Over one connection a client sends requests,
  receives responses and events, and holds any number of subscriptions.
- **Subscription (stream).** A connection's live view of one session's output, with rights
  to type into it (`input`) and to size it (`sizing`).
- **`seq`.** A session's output position: the count of output bytes the host has passed on
  since the session started (the bytes of the terminal queries it answers itself are not
  counted). Every snapshot names the `seq` it was taken at, and every output chunk the `seq`
  of its first byte, so a snapshot joined to the live stream has no gap and no repeat.

## Transport

- **Windows:** a named pipe, `\\.\pipe\tether-<32 hex>`. The name is random per host and
  appears only in the discovery file. Its DACL grants access to the user's SID alone and
  denies network logons; remote clients are rejected. The host also checks the SID of each
  connecting process.
- **macOS and Linux:** a Unix socket in a directory of mode 0700 owned by the user
  (`$XDG_RUNTIME_DIR/tether`, else `$TMPDIR/tether-<uid>` or `/tmp/tether-<uid>`), socket
  mode 0600. The host checks the peer's uid.

## Discovery

The state directory is `%LOCALAPPDATA%\tether` on Windows, `~/Library/Application
Support/tether` on macOS, `$XDG_STATE_HOME/tether` (else `~/.local/state/tether`) on Linux.
`TETHER_DIR` overrides it on every platform.

| Path | Contents |
|---|---|
| `hosts/<host>.json` | `{host, pid, version, protocol, endpoint, token, startedAt, draining?}`, written atomically by the host when it is ready, removed when it exits |
| `current` | The id of the newest host, which new sessions should go to |
| `start.lock` | Held (created exclusively) by whoever is starting a host; stale after 10 s |
| `logs/<host>.log` | The host's log |

A host file whose host cannot be connected to, or does not prove it holds the token, is
stale. A client MAY delete a stale file whose `pid` is not alive.

The token is 32 random bytes as 64 lowercase hex characters. It never goes on the wire.

## Frames

Every byte on a connection belongs to a frame:

```
u32 LE  length of the payload
u8      type
...     payload (length bytes)
```

A payload is at most 16 MiB; a JSON payload at most 1 MiB. A receiver MUST close a
connection that sends a larger frame. A receiver MUST ignore a frame of a type it does not
know.

| Type | Name | Payload | Direction |
|---|---|---|---|
| `0x00` | Handshake | UTF-8 JSON | both, before `welcome` only |
| `0x01` | Message | UTF-8 JSON: a request, response or event | both |
| `0x02` | Output | `u32 LE stream`, `u64 LE seq`, bytes | host to client |
| `0x03` | Input | `u32 LE stream`, bytes | client to host |
| `0x04` | Resync | `u32 LE stream`, `u64 LE seq`, bytes | host to client |

- **Output** carries raw pseudo-terminal output as the program wrote it (terminal queries the
  host answers are removed; see [Terminal queries](#terminal-queries)). `seq` is the position
  of its first byte.
- **Resync** carries a repaint: bytes that, written to a fresh terminal of the session's size,
  reproduce its screen, its scrollback when asked for, its cursor and its input modes. `seq`
  is the position the repaint stands at; the next Output on the stream starts there.
- **Input** carries bytes to type into the session, as if a terminal sent them. Only a stream
  opened with `input: true` may send it; anything else is dropped.

## Handshake

The client speaks first, and nothing but Handshake frames pass until the host sends
`welcome`.

1. Client: `{"hello": "tether", "protocol": [min, max], "client": {"name", "version", "pid"}, "caps": [], "nonce": "<64 hex>"}`.
2. Host: `{"protocol": n, "nonce": "<64 hex>", "proof": "<64 hex>"}`, where `n` is the
   highest version both support. With none in common, the host sends `{"error": {"code":
   "unsupported_protocol", "message"}}` and closes.
3. Client: checks the host's proof, closes if it is wrong, then sends `{"proof": "<64 hex>"}`.
4. Host: checks the client's proof, then sends `{"welcome": {"host", "version", "protocol",
   "pid", "caps": []}}`, or `{"error": {"code": "auth", "message"}}` and closes.

Proofs are HMAC-SHA256, keyed with the token's 64 hex characters as ASCII, over a label
followed by the two nonces as raw bytes, the client's first:

```
host proof   = HMAC(token, "tether host"   || client_nonce || host_nonce)
client proof = HMAC(token, "tether client" || client_nonce || host_nonce)
```

Both sides prove the token, so a client never hands it to a process squatting on a name it
read from a stale file.

Host capabilities in version 1: `paste`, `keys`, `screen.cells`, `resync`, `drain`, `labels`,
`keep` (the host ends a program with its last window, and takes `keep` on `spawn`; a host
without it keeps every program running).

## Messages

```
request   {"id": <u64>, "op": "<name>", ...fields}
response  {"re": <id>, "ok": {...}}
          {"re": <id>, "error": {"code": "<code>", "message": "<text>"}}
event     {"ev": "<name>", "session"?: "<id>", ...fields}
```

A client picks request ids; responses MAY arrive in any order. A request's fields never use
the names `id` or `op`.

Error codes: `bad_request`, `not_found`, `exited` (the session has exited), `denied`,
`draining` (the host is draining and starts nothing new), `spawn_failed`, `too_large`,
`unsupported`, `internal`.

### Requests

| Op | Fields | Result |
|---|---|---|
| `host` | | `HostInfo` |
| `ping` | | `{now}` (epoch ms) |
| `spawn` | `argv`, `cwd?`, `env?`, `size?`, `name?`, `labels?`, `scrollback?` | `{session, pid}` |
| `list` | `labels?` (every one must match) | `{sessions: [SessionInfo]}` |
| `info` | `session` | `SessionInfo` |
| `write` | `session`, `data?` (UTF-8 text) or `b64?` (bytes) | `{}` |
| `paste` | `session`, `text`, `bracketed?`: `"auto"` (default), `true`, `false` | `{bracketed}` |
| `keys` | `session`, `keys: [KeyName]` | `{}` |
| `resize` | `session`, `cols`, `rows` | `{}` |
| `screen` | `session`, `format?`: `"text"` (default), `"vt"`, `"cells"`; `scrollback?` | `Screen` |
| `subscribe` | `session`, `from?`, `input?`, `sizing?`, `size?`, `role?`, `scrollback?` | `{stream, seq, cols, rows}` |
| `unsubscribe` | `stream` | `{}` |
| `kill` | `session`, `graceMs?` | `{}` |
| `signal` | `session`, `signal`: `INT`, `TERM`, `HUP`, `KILL` | `{}` |
| `set-labels` | `session`, `labels` (a `null` value removes the label) | `{labels}` |
| `remove` | `session` | `{}` |
| `watch` | `sessions?` (default: every session) | `{}` |
| `drain` | | `{}` |

A `session` field takes a session's id or its name.

**`spawn`.**
- `argv` is the program and its arguments, the program first. A program named without a
  directory is looked up in the session's own `PATH`; on Windows with `PATHEXT` too, and a
  batch file runs under `cmd.exe /d /c`.
- `cwd` defaults to the host's.
- `env` is `{base: "empty" | "host", set?: {name: value}, unset?: [name]}`. `base` defaults
  to `"host"`. A client SHOULD send `base: "empty"` with its own environment in `set`, so a
  session never inherits whatever environment started the host. The host adds only
  `TETHER_SESSION=<id>`, and `TERM=xterm-256color` on macOS and Linux when it is missing.
- `size` is `{cols, rows}`, 120×32 by default.
- `scrollback` is the number of lines the screen model keeps above the screen, 3,000 by
  default.
- `keep: true` keeps the program running when its last window closes; by default that ends
  it (see [Lifetime](#lifetime)).
- A draining host answers `draining`.

**`paste`.** `"auto"` wraps the text in `ESC[200~` … `ESC[201~` when the program has turned
bracketed paste on (`DECSET 2004`). Line breaks become `CR`, as a terminal pastes them, and
any `ESC[201~` inside the text is removed so it cannot end the paste early.

**`keys`.** Each name is one key, written as the session expects it: arrows and Home/End
follow the program's cursor-key mode (`DECCKM`). Names, case-insensitive:

| Name | Bytes |
|---|---|
| `Enter` | `CR` |
| `Tab`, `S-Tab` | `TAB`, `ESC[Z` |
| `Esc` | `ESC` |
| `Backspace` | `DEL` |
| `Space` | ` ` |
| `Up`, `Down`, `Right`, `Left` | `ESC[A`…`ESC[D`, or `ESCOA`… in cursor-key mode |
| `Home`, `End` | `ESC[H`, `ESC[F`, or `ESCOH`, `ESCOF` |
| `PageUp`, `PageDown`, `Insert`, `Delete` | `ESC[5~`, `ESC[6~`, `ESC[2~`, `ESC[3~` |
| `F1`…`F12` | `ESCOP`…`ESCOS`, `ESC[15~`…`ESC[24~` |
| `C-a`…`C-z`, `C-[`, `C-\`, `C-]`, `C-^`, `C-_`, `C-@`, `C-Space` | the control byte |
| `M-<key>` | `ESC` then the key |
| `S-`, `C-`, `M-` on an arrow, Home or End | `ESC[1;<m><final>`, `m` = 1 + shift + 2·alt + 4·ctrl |
| any other single character | its UTF-8 bytes |

**`screen`.**
```
Screen {format, cols, rows, cursor: {row, col, visible}, title?, seq,
        altScreen, bracketedPaste, appCursor,
        lines?: [string]      // "text": each row, trailing blanks trimmed
        data?: string         // "vt": the repaint, as Resync carries it
        cells?: [[Run]]}      // "cells": each row as runs of like-styled text
Run {t, fg?, bg?, bold?, dim?, italic?, underline?, inverse?}
```
Colours are an index 0–255 or `"#rrggbb"`. With `scrollback: true`, `lines` and `cells`
begin with the scrollback, oldest first, and `data` repaints it.

**`subscribe`.**
- `from`: `"snapshot"` (default) sends a Resync first; `"now"` sends only what comes next;
  a number replays Output from that `seq` when the ring still holds it, and a Resync
  otherwise.
- `input: true` lets the stream send Input frames. `sizing: true` makes it a sizing viewer
  (see [Sizing](#sizing)); `size` is its own `{cols, rows}`.
- `role` is `"window"` for a terminal window, `"viewer"` for anything else that shows the
  session, `"control"` otherwise. It is reported in `SessionInfo.clients`, and only
  `"window"` is acted on: a session's windows decide its lifetime (see [Lifetime](#lifetime)).
- `scrollback: true` makes every Resync on the stream carry the scrollback.
- The response comes first, then the stream's frames. Subscribing to an exited session gives
  its last screen and an `exited` event.

**`kill`.** Ends the program: on macOS and Linux `SIGHUP` to its process group, then
`SIGKILL` after `graceMs` (default 2000), and the same to the terminal's foreground process
group when a shell runs a program there; on Windows the process and every process it started
are terminated at once. The host runs each program in a job object, so this reaches what the
program started outside its terminal too, except a process that asked to leave the job.

**`signal`.** On Windows `INT` types `Ctrl+C` and anything else ends the program as `kill`
does.

**`remove`.** Forgets an exited session. A running session answers `bad_request`.

**`drain`.** The host starts nothing new, marks its discovery file `draining`, emits
`host.draining` to every connection, and exits once its last session has ended.

### Shapes

```
HostInfo {host, pid, version, protocol, startedAt, draining, sessions, clients}

SessionInfo {session, name?, pid?, argv, cwd, cols, rows, title?, cwdReported?,
             labels: {string: string}, keep, status: "running" | "exited",
             exit?: {code, signal?}, startedAt, exitedAt?, seq,
             clients: [ClientInfo]}

ClientInfo {client, stream, pid?, name?, role, input, sizing, cols?, rows?,
            attachedAt, lastInput?}
```

Times are epoch milliseconds. `ClientInfo.pid` is the connecting process as the operating
system reports it. An exited session is kept, with its last screen, for 10 minutes or until
`remove`.

### Events

| Event | Fields | Sent to |
|---|---|---|
| `created` | `session`, `info` | watchers |
| `exited` | `session`, `code`, `signal?` | watchers, the session's streams |
| `title` | `session`, `title` | watchers, the session's streams |
| `bell` | `session` | watchers, the session's streams |
| `cwd` | `session`, `cwd` (OSC 7) | watchers, the session's streams |
| `resized` | `session`, `cols`, `rows`, `by?` (a client id) | watchers, the session's streams |
| `attached` | `session`, `client: ClientInfo` | watchers |
| `detached` | `session`, `client` | watchers |
| `labels` | `session`, `labels` | watchers |
| `removed` | `session` | watchers |
| `host.draining` | `host` | every connection |

A connection receives an event once even when it both watches and subscribes.

## Sizing

- The pseudo-terminal has one size.
- A subscription with `sizing: true` carries its own size. The latest sizing subscription to
  subscribe, type or resize sets the pseudo-terminal to its size. When it leaves, the
  sizing subscription used most recently sets it, if any is left.
- A `resize` request from a connection with no sizing subscription on the session sets the
  size directly. From a sizing subscription it updates that subscription's size and, under
  the rule above, the terminal's.
- Everything else follows: a viewer that does not size learns of changes from `resized`.

## Flow control

The host never waits on a client. Each subscription has an output budget (1 MiB). A
subscription whose queued output passes it is marked lagging, and the host stops queueing
output for it. Once the connection has written everything already queued, the host sends a
Resync, and output resumes from its `seq`. A viewer that falls behind gets a fresh screen,
not the backlog.

## Terminal queries

A program asks its terminal questions, and with no window attached nobody would answer; with
two windows attached, both would. So the host answers these itself, from its screen model,
and removes them from the output it passes on:

| Query | Answer |
|---|---|
| `ESC[6n` (cursor position) | `ESC[<row>;<col>R` |
| `ESC[5n` (status) | `ESC[0n` |
| `ESC[c`, `ESC[0c` (primary attributes) | `ESC[?62;22c` |
| `ESC[>c`, `ESC[>0c` (secondary attributes) | `ESC[>0;10;1c` |

Every other query passes through to the attached windows.

## Lifetime

- `tether serve --daemonize` starts a host fully detached (on Windows with no inherited
  handles, outside the caller's job; on macOS and Linux in a new session, forked twice) and
  returns once it is ready, printing its host file.
- A host exits after 10 minutes with no sessions and no connections (`--idle-exit`).
- A newer host of the same protocol starts beside an older one and becomes `current`; the
  older one is told to `drain`.
- Sessions die with their host. A host crash ends them.
- A session ends with its windows. When a connection closes holding the last `"window"`
  stream on a running session, the host ends the program as `kill` does. A window that
  detaches sends `unsubscribe` first, and the program runs on. Only windows count: viewers
  and control streams never keep a program running, a session no window has attached to runs
  until it exits or is killed, and one spawned with `keep: true` runs on whatever its windows
  do.
