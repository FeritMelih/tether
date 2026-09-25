// The frame codec, the handshake arithmetic and discovery, against the fixtures the Rust
// crates check too.

import { describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { currentHost, Decoder, encodeInput, encodeJson, encodeSeq, FrameType, liveHosts, newer, stateDir } from "../src/index.ts";
import type { Frame } from "../src/index.ts";
import { CLIENT_LABEL, hex, HOST_LABEL, proof, unhex, verify } from "../src/handshake.ts";

const spec = join(import.meta.dir, "..", "..", "..", "spec");
const fixtures = JSON.parse(readFileSync(join(spec, "fixtures", "frames.json"), "utf8")).fixtures as { name: string; hex: string; frame: Record<string, unknown> }[];
const bytes = (h: string) => new Uint8Array(Buffer.from(h, "hex"));

function comparable(f: Frame): Record<string, unknown> {
  if (f.type === "output" || f.type === "resync") return { type: f.type, stream: f.stream, seq: f.seq, data: hex(f.data) };
  if (f.type === "input") return { type: f.type, stream: f.stream, data: hex(f.data) };
  return f as unknown as Record<string, unknown>;
}

describe("frames", () => {
  test("decode every shared fixture, whole and split at every byte", () => {
    const all = fixtures.map((f) => bytes(f.hex));
    const stream = new Uint8Array(all.reduce((n, b) => n + b.length, 0));
    let at = 0;
    for (const b of all) {
      stream.set(b, at);
      at += b.length;
    }
    for (let split = 0; split <= stream.length; split++) {
      const d = new Decoder();
      const got: Record<string, unknown>[] = [];
      for (const part of [stream.subarray(0, split), stream.subarray(split)]) {
        d.push(part);
        for (let f = d.next(); f; f = d.next()) got.push(comparable(f));
      }
      expect(got).toEqual(fixtures.map((f) => f.frame));
      expect(d.pending()).toBe(0);
    }
  });

  test("encode what a client sends exactly as the fixtures spell it", () => {
    const byName = (n: string) => fixtures.find((f) => f.name === n)!;
    expect(hex(encodeJson(FrameType.Message, { id: 1, op: "ping" }))).toBe(byName("a request").hex);
    expect(hex(encodeInput(3, new Uint8Array([13])))).toBe(byName("input").hex);
    expect(hex(encodeSeq(FrameType.Output, 7, 2 ** 40 + 5, new TextEncoder().encode("\x1b[31mhi")))).toBe(byName("output past 2^32 bytes").hex);
  });

  test("refuse an oversized frame", () => {
    const d = new Decoder();
    const head = new Uint8Array(5);
    new DataView(head.buffer).setUint32(0, 1024 * 1024 + 1, true);
    head[4] = FrameType.Message;
    d.push(head);
    expect(() => d.next()).toThrow();
  });
});

describe("handshake", () => {
  test("proofs match the shared vector", () => {
    const v = JSON.parse(readFileSync(join(spec, "fixtures", "handshake.json"), "utf8"));
    const [nc, nh] = [unhex(v.clientNonce)!, unhex(v.hostNonce)!];
    expect(hex(proof(v.token, HOST_LABEL, nc, nh))).toBe(v.hostProof);
    expect(hex(proof(v.token, CLIENT_LABEL, nc, nh))).toBe(v.clientProof);
    expect(verify(v.token, HOST_LABEL, nc, nh, unhex(v.hostProof)!)).toBe(true);
    expect(verify(v.token, HOST_LABEL, nc, nh, unhex(v.clientProof)!)).toBe(false);
    expect(unhex("zz")).toBeUndefined();
  });
});

describe("discovery", () => {
  test("the state directory per platform, and its override", () => {
    expect(stateDir({ TETHER_DIR: "X" }, "linux")).toBe("X");
    expect(stateDir({ LOCALAPPDATA: "C:\\L" }, "win32")).toBe(join("C:\\L", "tether"));
    expect(stateDir({ XDG_STATE_HOME: "/s" }, "linux")).toBe(join("/s", "tether"));
    expect(stateDir({}, "darwin").endsWith(join("Library", "Application Support", "tether"))).toBe(true);
  });

  test("dead hosts are swept, and current wins among the live", () => {
    const dir = mkdtempSync(join(tmpdir(), "tether-disc-"));
    mkdirSync(join(dir, "hosts"));
    const file = (host: string, pid: number, startedAt: number, extra: Record<string, unknown> = {}) =>
      writeFileSync(join(dir, "hosts", `${host}.json`), JSON.stringify({ host, pid, version: "0.1.0", protocol: 1, endpoint: "e", token: "t", startedAt, ...extra }));
    file("dead", 1, 3);
    file("old", 2, 1);
    file("new", 3, 2);
    file("draining", 4, 4, { draining: true });
    writeFileSync(join(dir, "current"), "old");
    const alive = (pid: number) => pid !== 1;
    expect(liveHosts(dir, alive).map((h) => h.host)).toEqual(["draining", "new", "old"]);
    expect(currentHost(dir, alive)?.host).toBe("old");
    writeFileSync(join(dir, "current"), "draining");
    expect(currentHost(dir, alive)?.host).toBe("new");
  });

  test("versions compare by their numbers", () => {
    expect(newer("0.10.0", "0.9.9")).toBe(true);
    expect(newer("0.1.0", "0.1.0")).toBe(false);
    expect(newer("1.0.0-beta", "0.9.0")).toBe(true);
  });
});
