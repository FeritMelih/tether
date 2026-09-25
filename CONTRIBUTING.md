# Contributing

tether is small on purpose: a host that owns pseudo-terminals, the protocol that reaches it,
two clients and a window. A change that adds a feature of a multiplexer (panes, layouts, a
status bar) or teaches the host about one particular program belongs in an application
built on top, not here.

## Before you start

- Read [spec/protocol.md](spec/protocol.md). A change to what crosses the wire changes the
  spec in the same pull request, adds or updates a fixture in `spec/fixtures` and a scenario
  in `spec/conformance`, and keeps both clients passing.
- Open an issue for anything beyond a fix, so the shape is agreed before the work.

## Working on it

```
cargo test -j 8
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo deny check
cd sdk/typescript && bun test && bunx tsc -p .
```

With `TETHER_BIN` and `TESTTUI_BIN` pointing at `target/debug`, the SDK's suite and the
conformance scenarios run against the real host too.

## Sign-off

Contributions are accepted under the [Developer Certificate of Origin](https://developercertificate.org/):
sign off every commit (`git commit -s`), certifying you wrote it or otherwise have the right to
submit it under the Apache License 2.0.

## Style

Code reads like the code around it: a comment block at the top of each file saying what it is
and why, sentences in the docs, no history in either. Tests sit on the seams: the frame codec
and the handshake against fixtures, a session against an in-memory terminal, the host against
a real one.
