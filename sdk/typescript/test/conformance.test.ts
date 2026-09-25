// The conformance scenarios in spec/conformance, through this SDK against a real host started
// with `serve --daemonize`. Runs when TETHER_BIN names the binary and TESTTUI_BIN the test
// program (`cargo build` puts both in target/debug).

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, readdirSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { startHost, TetherClient, TetherError } from "../src/index.ts";
import type { HostFile, Subscription, TetherEvent } from "../src/index.ts";

const BIN = process.env["TETHER_BIN"];
const TESTTUI = process.env["TESTTUI_BIN"];
const live = BIN && TESTTUI ? describe : describe.skip;
const dir = join(import.meta.dir, "..", "..", "..", "spec", "conformance");

type Json = Record<string, unknown>;

function substitute(v: unknown, vars: Record<string, string>): unknown {
  if (typeof v === "string" && v.startsWith("$")) return vars[v.slice(1)] ?? v;
  if (Array.isArray(v)) return v.map((x) => substitute(x, vars));
  if (v && typeof v === "object") return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, substitute(x, vars)]));
  return v;
}

function at(v: unknown, path: string): unknown {
  return path.split(".").reduce<unknown>((o, k) => (o as Record<string, unknown> | undefined)?.[k], v);
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

class Run {
  readonly c: TetherClient;
  readonly name: string;
  vars: Record<string, string>;
  events: TetherEvent[] = [];
  subs: Subscription[] = [];
  constructor(c: TetherClient, name: string) {
    this.c = c;
    this.name = name;
    this.vars = { TESTTUI: TESTTUI! };
    c.onEvent((e) => this.events.push(e));
  }

  async screen(session: string): Promise<string> {
    return ((await this.c.screen(session)).lines ?? []).join("\n");
  }

  async step(raw: Json): Promise<void> {
    const step = substitute(raw, this.vars) as Json;
    const name = this.name;
    if (typeof step["request"] === "string") {
      const op = step["request"];
      let result: unknown;
      let error: TetherError | undefined;
      try {
        result = await this.c.request(op, (step["args"] as Json) ?? {});
      } catch (e) {
        error = e as TetherError;
      }
      if (step["error"]) {
        expect(error?.code, `${name}: ${op}`).toBe(step["error"] as string);
        return;
      }
      if (error) throw new Error(`${name}: ${op} failed: ${error.message}`);
      for (const [path, want] of Object.entries((step["expect"] as Json) ?? {})) expect(at(result, path), `${name}: ${op} ${path}`).toEqual(want);
      for (const [v, path] of Object.entries((step["save"] as Record<string, string>) ?? {})) this.vars[v] = String(at(result, path));
    } else if (typeof step["type"] === "string") {
      await this.c.paste(step["type"], step["text"] as string, false);
      await this.c.keys(step["type"], ["Enter"]);
    } else if (typeof step["wait"] === "string") {
      const deadline = Date.now() + 10000;
      for (;;) {
        const text = await this.screen(step["wait"]);
        if (text.includes(step["contains"] as string)) break;
        if (Date.now() > deadline) throw new Error(`${name}: no ${JSON.stringify(step["contains"])} on the screen:\n${text}`);
        await sleep(50);
      }
    } else if (step["subscribe"]) {
      let text = "";
      let first: "output" | "resync" | undefined;
      const decoder = new TextDecoder();
      const sub = await this.c.subscribe(step["subscribe"] as Json as never, {
        onOutput: (d) => {
          first ??= "output";
          text += decoder.decode(d, { stream: true });
        },
        onResync: (d) => {
          first ??= "resync";
          text += decoder.decode(d, { stream: true });
        },
      });
      this.subs.push(sub);
      if (raw["after"]) await this.step(raw["after"] as Json);
      const deadline = Date.now() + 10000;
      while (!text.includes(step["until"] as string)) {
        if (Date.now() > deadline) throw new Error(`${name}: the stream never showed ${JSON.stringify(step["until"])}: ${JSON.stringify(text)}`);
        await sleep(20);
      }
      if (step["resync"] === true) expect(first, `${name}: the stream starts with a resync`).toBe("resync");
    } else if (typeof step["event"] === "string") {
      const want = (step["expect"] as Json) ?? {};
      const deadline = Date.now() + 10000;
      for (;;) {
        const i = this.events.findIndex((e) => e.ev === step["event"] && (want["session"] === undefined || e.session === want["session"]));
        if (i >= 0) {
          const e = this.events.splice(i, 1)[0]!;
          for (const [path, v] of Object.entries(want)) expect(at(e, path), `${name}: ${e.ev} ${path}`).toEqual(v);
          break;
        }
        if (Date.now() > deadline) throw new Error(`${name}: no ${step["event"]} event`);
        await sleep(20);
      }
    } else {
      throw new Error(`${name}: a step of no known kind: ${JSON.stringify(step)}`);
    }
  }
}

live("conformance", () => {
  let host: HostFile;
  const state = mkdtempSync(join(tmpdir(), "tether-ts-conf-"));

  beforeAll(async () => {
    host = await startHost({ exe: BIN!, dir: state, idleExitS: 30 });
  });

  afterAll(async () => {
    const c = await TetherClient.connect(host, { name: "teardown" }).catch(() => undefined);
    await c?.request("drain").catch(() => undefined);
    c?.close();
  });

  const files = readdirSync(dir).filter((f) => f.endsWith(".json")).sort();

  test("the scenarios are there", () => {
    expect(files.length).toBeGreaterThanOrEqual(5);
  });

  for (const file of files) {
    const scenario = JSON.parse(readFileSync(join(dir, file), "utf8")) as { name: string; steps: Json[] };
    test(`${scenario.name} (${file})`, async () => {
      const c = await TetherClient.connect(host, { name: "conformance" });
      const run = new Run(c, scenario.name);
      try {
        for (const step of scenario.steps) await run.step(step);
      } finally {
        c.close();
      }
    }, 60000);
  }
});
