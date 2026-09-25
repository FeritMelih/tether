# Conformance scenarios

Each file is one scenario: a list of steps a client runs against a real host whose program is
`testtui` (`$TESTTUI`). The Rust client (`crates/testtui/tests/conformance.rs`) and the
TypeScript SDK (`sdk/typescript/test/conformance.test.ts`) both run every scenario, so the two
clients and the host agree on the protocol as `../protocol.md` states it.

| Step | Does |
|---|---|
| `{"request": op, "args": {...}}` | Sends a request. `"save": {"var": "path"}` keeps fields of the result; `"expect": {"path": value}` checks them; `"error": "code"` expects the request to fail with that code. |
| `{"type": "$s", "text": "…"}` | Types a line as testtui's command: pasted unbracketed, then Enter. |
| `{"wait": "$s", "contains": "…"}` | Polls the screen's text until it contains the string (10 s). |
| `{"subscribe": {...}, "until": "…"}` | Subscribes and reads the stream until its raw text contains the string; `"resync": true` expects a Resync first; `"after": step` runs a step once subscribed. The subscription stays open to the end. |
| `{"event": name, "expect": {...}}` | Waits for an event (the scenario must `watch` first). |

Paths are dotted (`exit.code`, `clients.0.role`). `$name` in any string is a saved value.
