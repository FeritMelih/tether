# tether

tether holds programs in pseudo-terminals so that any application can type into them, read
their screens and watch them, while ordinary terminal windows attach and detach. A program in
tether needs no window to run, and a window is only a view onto it: detaching one leaves the
program running, while closing the last one ends it, as closing a terminal would, unless the
program was started with `--keep`.

It exists for applications that drive interactive terminal programs as a person would: an
orchestrator typing into a coding agent's TUI, a dashboard showing a live terminal, a test
harness answering a dialog. What such an application types arrives as keys typed into the
program's terminal, indistinguishable from the user's own, and what the program draws reaches
every window as the program wrote it.

```
                     ┌──────────────────── tether host ────────────────────┐
 application ─ req ─▶│  session                                             │
            ◀─ evt ──│   ┌───────────┐   ┌──────┐                           │
            ◀─ out ──│   │ screen    │◀──┤ ring │◀── output ──┐             │
                     │   │ model     │   └──────┘             │             │
 window ◀── repaint ─│   └───────────┘                    [ pseudo-terminal ]◀──▶ program
 (tether attach)     │                                         ▲             │
       ◀── output ───│ ────────────────────────────────────────┘             │
       ─── keys ────▶│ ── input ───────────────────────────────┘             │
                     └──────────────────────────────────────────────────────┘
```

tether is not a multiplexer. It draws nothing of its own: no panes, tabs, status bars or key
tables. The window shows the program's own screen, byte for byte (on Windows as ConPTY
renders it).

## Using it

```
tether run -- claude                  # start a program in tether and attach this terminal
tether run --keep -- claude           # the same, running on when the window closes
tether spawn --name build -- make     # start one with no window
tether ls                             # sessions on every host
tether attach build                   # a window on it (Ctrl-] detaches; twice sends it)
tether open build                     # the same in a new terminal window
tether send build "hello" --enter     # paste text, then Enter
tether keys build Down Enter          # press keys by name
tether screen build --scrollback      # the screen as text
tether kill build
tether profiles install               # a Windows Terminal or iTerm2 profile that starts in tether
```

The first command that needs a host starts one (`tether serve --daemonize`), and a host with no
sessions and no clients exits after ten minutes.

**Windows.** `open` prefers Windows Terminal, which draws the symbols TUIs use from fallback
fonts; the classic console window shows some of them as `?`. `--terminal console` asks Windows
for a new console (your default terminal application) and `--terminal conhost` names the
classic window.

**macOS and Linux.** `open` uses iTerm2 when it is installed and Terminal.app otherwise on
macOS, and the first terminal program found on Linux (`$TERMINAL` first).

`open` prints what it opened as JSON: `{session, terminal, pid?}`, with `pid` the window's
process when `open` started it itself (a console window), so a caller can close exactly that
window; a terminal that hands its windows to a process of its own (Windows Terminal) has none.

## Clients

An application talks to its host over one local connection:

- **The protocol** is in [spec/protocol.md](spec/protocol.md): length-prefixed frames, JSON
  requests, responses and events, and binary output. Both sides prove a shared token without
  sending it.
- **The Rust client** is `crates/client`; the `tether` binary uses it.
- **The TypeScript SDK** is `sdk/typescript`, with no runtime dependencies. It runs in Node
  and Bun, and also has helpers for typing into TUIs: `submit` (a bracketed paste, a pause,
  Enter, then a check that it landed), `waitForScreen`, `selectRow`, `chooseDigit` and
  `quiet`.

```ts
import { connectOrStart, submit } from "@tether-pty/client";

const c = await connectOrStart({ exe: "/path/to/tether", name: "my-app" });
const { session } = await c.spawn({ argv: ["claude"], cwd: "/work", env: { base: "empty", set: process.env as Record<string, string> } });
// `term` is any terminal emulator, xterm.js say.
await c.subscribe({ session, role: "viewer" }, {
  onResync: (d) => { term.reset(); term.write(d); },
  onOutput: (d) => term.write(d),
});
await submit(c, session, "Summarise the README", { verify: () => sawItLand() });
```

Nothing in tether knows which program runs in a session. What counts as a prompt being ready,
a message having landed, or a row being selected is the application's to say, from the screen
or from the program's own signals.

## How it works

- **A host** is one process per user per machine. It owns sessions and listens on a named
  pipe (Windows) or a Unix socket (macOS, Linux) that only the user can reach. It never
  listens on a network.
- **A session** is a program in a pseudo-terminal and one task that owns everything about it:
  - a `vt100` screen model with scrollback (3,000 lines by default);
  - a 1 MiB ring of raw output;
  - its subscribers and who sizes it.

  Threads sit on the blocking handles, reading output, writing input and waiting for the exit.
- **`seq`** counts a session's output bytes. A snapshot names the `seq` it was taken at and
  every output chunk the `seq` of its first byte, so a snapshot joined to the stream has no
  gap and no repeat.
- **Windows attach** with a repaint:
  - it clears the window, writes the scrollback into the window's own scrollback, then draws
    the screen, the cursor and the input modes;
  - then the live stream follows.
- **Sizing.** The last window to attach, type or resize sets the terminal's size. Viewers
  that do not size follow it.
- **Flow control.** The host never waits on a client. A viewer that falls behind stops being
  sent output, and once it has caught up it gets a fresh screen instead of the backlog.
- **Terminal queries.** The host answers the cursor-position, status and device-attribute
  queries itself, from its screen model, and takes them out of the output. A program with no
  window still gets its answer, and two windows never both answer.
- **Lifetime.**
  - A host started by `serve --daemonize` inherits no handle from the process that asked for
    it: on Windows it is created detached, with no handle inherited; elsewhere it is forked
    twice through a new session.
  - A newer host starts beside an older one, which drains: it starts nothing new and exits
    after its last session.
  - Sessions end with their host.
  - A program ends with its last window. A window that goes without detaching (Ctrl-]) was
    closed, and when no other window is on the session, the host ends the program as `kill`
    does. Only terminal windows count: an application viewing a session keeps nothing
    running, a session no window has attached to runs until it exits, and one started with
    `--keep` (`keep: true` on `spawn`) runs on whatever its windows do.
  - Ending a program ends what it started. On Windows each program runs in a job object, so
    `kill` reaches processes it started outside its terminal too (only one that asked to
    leave the job escapes); elsewhere its process group and the terminal's foreground job
    are hung up, then killed.

## Security

The boundary is the operating-system user, as with tmux. On Windows the pipe's DACL grants
the user's SID alone and refuses network logons, remote clients are rejected, and each
client's SID is checked. Elsewhere the socket lives in a 0700 directory, is itself 0600, and
each peer's uid is checked. Every connection then proves the host's token, which lives in the
user's state directory and never goes on the wire, and the host proves it back. Anything that
passes can type into every session as the user; see [SECURITY.md](SECURITY.md).

## Building

```
cargo build --release          # target/release/tether
cargo test -j 8                # unit tests, and integration tests over a real pseudo-terminal
cd sdk/typescript && bun test  # the SDK; with TETHER_BIN and TESTTUI_BIN set, against the real host
```

`crates/testtui` is a deterministic program the tests run inside sessions.
`spec/conformance/` holds scenarios both clients run against the real host.

| Crate | What it is |
|---|---|
| `crates/proto` | Frames, messages, the handshake, discovery files. No OS dependencies |
| `crates/core` | Sessions: the pseudo-terminal, the screen model, keys, queries, subscribers |
| `crates/server` | The host: its endpoint, connections, lifetime, `--daemonize` |
| `crates/client` | The Rust client |
| `crates/cli` | The `tether` binary (package `tether-pty`) |
| `crates/testtui` | The test program and the integration tests |

## Status and limits

- **Windows 10 before build 22523** strips the bracketed-paste markers written into ConPTY.
  The console-input fallback that other hosts use there is not built yet.
- **`portable-pty` is pinned at 0.9.0.** Its Windows half is to be forked:
  - to load `conpty.dll` only from the application's folder;
  - to make ConPTY's flags configurable;
  - so that closing a pseudo-console can never block.
- **The screen model is `vt100`.** Characters it measures at a different width from the
  terminal shift its cursor.
- **Crate and package names** are placeholders until the first release: `tether` is taken on
  crates.io and npm.

## Licence

Apache License 2.0; see [LICENSE](LICENSE) and [NOTICE](NOTICE). Contributions are signed off
under the Developer Certificate of Origin; see [CONTRIBUTING.md](CONTRIBUTING.md).
