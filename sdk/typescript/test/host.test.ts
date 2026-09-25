// The SDK against a real host: starting one, the typing helpers, an upgrade, and the host
// letting go of what its starter held. Runs when TETHER_BIN and TESTTUI_BIN are set.

import { afterAll, describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { chooseDigit, connectOrStart, currentHost, liveHosts, quiet, selectRow, submit, TetherClient, waitForScreen } from "../src/index.ts";

const BIN = process.env["TETHER_BIN"];
const TESTTUI = process.env["TESTTUI_BIN"];
const live = BIN && TESTTUI ? describe : describe.skip;

const text = (lines: string[] | undefined) => (lines ?? []).join("\n");
const inputs = (lines: string[] | undefined) => (lines ?? []).filter((l) => l.startsWith("IN ")).map((l) => l.slice(3)).join("");

live("a real host", () => {
  const dir = mkdtempSync(join(tmpdir(), "tether-ts-host-"));
  const opened: TetherClient[] = [];
  const open = async (exeVersion?: string) => {
    const c = await connectOrStart({ exe: BIN!, dir, name: "sdk-test", idleExitS: 30, ...(exeVersion ? { exeVersion } : {}) });
    opened.push(c);
    return c;
  };

  afterAll(async () => {
    for (const h of liveHosts(dir)) {
      const c = await TetherClient.connect(h, { name: "teardown" }).catch(() => undefined);
      for (const s of (await c?.list().catch(() => [])) ?? []) if (s.status === "running") await c?.kill(s.session).catch(() => undefined);
      await c?.request("drain").catch(() => undefined);
      c?.close();
    }
    for (const c of opened) c.close();
  });

  test("connectOrStart starts one host and then finds it", async () => {
    const a = await open();
    const b = await open();
    expect(a.host.host).toBe(b.host.host);
    expect(a.welcome.protocol).toBe(1);
    expect(currentHost(dir)?.host).toBe(a.host.host);
  });

  test("submit types a line and presses Enter once it has landed", async () => {
    const c = await open();
    const { session } = await c.spawn({ argv: [TESTTUI!], env: { base: "empty", set: process.env as Record<string, string> } });
    const r = await submit(c, session, "lines 2", {
      ready: async () => text((await c.screen(session)).lines).includes("SIZE"),
      verify: async () => text((await c.screen(session)).lines).includes("line 2"),
    });
    expect(r).toEqual({ verified: true, enters: 1 });
    await quiet(c, session, 200);
    const seq = (await c.info(session)).seq;
    expect(seq).toBeGreaterThan(0);
    await c.kill(session);
  });

  test("selectRow presses until the screen says so, and chooseDigit types the digit then Enter", async () => {
    const c = await open();
    const { session } = await c.spawn({ argv: [TESTTUI!] });
    await waitForScreen(c, session, (s) => text(s.lines).includes("SIZE"));
    // testtui echoes each key: "selected" here is the third Down.
    await selectRow(c, session, { isSelected: (s) => (inputs(s.lines).match(/1b5b42/g) ?? []).length >= 3, max: 5 });
    expect((inputs((await c.screen(session)).lines).match(/1b5b42/g) ?? []).length).toBe(3);
    await chooseDigit(c, session, 2, { isSelected: (s) => inputs(s.lines).endsWith("32") });
    await waitForScreen(c, session, (s) => inputs(s.lines).endsWith("320d"));
    await c.kill(session);
  });

  test("a subscription streams in order from its snapshot", async () => {
    const c = await open();
    const { session } = await c.spawn({ argv: [TESTTUI!], name: "streamed" });
    await waitForScreen(c, session, (s) => text(s.lines).includes("SIZE"));
    const got: { kind: string; seq: number; len: number }[] = [];
    let all = "";
    const sub = await c.subscribe({ session: "streamed", sizing: false }, {
      onOutput: (d, seq) => {
        got.push({ kind: "output", seq, len: d.length });
        all += new TextDecoder().decode(d);
      },
      onResync: (d, seq) => {
        got.push({ kind: "resync", seq, len: d.length });
        all += new TextDecoder().decode(d);
      },
    });
    for (let i = 0; i < 100 && got.length === 0; i++) await new Promise((r) => setTimeout(r, 10));
    expect(got[0]?.kind).toBe("resync");
    expect(got[0]?.seq).toBe(sub.seq);
    await c.paste(session, "lines 30", false);
    await c.keys(session, ["Enter"]);
    await waitForScreen(c, session, (s) => text(s.lines).includes("line 30"));
    await quiet(c, session, 100);
    // Each chunk starts where the one before ended.
    let at = sub.seq;
    for (const g of got.slice(1)) {
      expect(g.seq).toBe(at);
      at += g.len;
    }
    expect(at).toBe((await c.info(session)).seq);
    expect(all).toContain("line 30");
    await sub.unsubscribe();
    await c.kill(session);
  });

  test("a newer binary gets a host of its own and the old one drains", async () => {
    const old = await open();
    const { session } = await old.spawn({ argv: [TESTTUI!] });
    const fresh = await open("99.0.0");
    expect(fresh.host.host).not.toBe(old.host.host);
    expect(currentHost(dir)?.host).toBe(fresh.host.host);
    const oldFile = JSON.parse(readFileSync(join(dir, "hosts", `${old.host.host}.json`), "utf8"));
    expect(oldFile.draining).toBe(true);
    // The old host keeps the session it has.
    expect((await old.info(session)).status).toBe("running");
    await old.kill(session);
  }, 30000);

  test("a host started from a process that holds a listener does not keep it", async () => {
    const port = 47000 + Math.floor(Math.random() * 900);
    const script = join(dir, "hold.ts");
    writeFileSync(
      script,
      `import { startHost } from ${JSON.stringify(join(import.meta.dir, "..", "src", "index.ts"))};
const server = Bun.serve({ port: ${port}, hostname: "127.0.0.1", fetch: () => new Response("held") });
await startHost({ exe: ${JSON.stringify(BIN)}, dir: ${JSON.stringify(join(dir, "held"))}, idleExitS: 30 });
server.stop(true);
`,
    );
    const r = Bun.spawnSync([process.execPath, script], { stdout: "pipe", stderr: "pipe" });
    expect(r.exitCode).toBe(0);
    expect(liveHosts(join(dir, "held")).length).toBe(1);
    const again = Bun.serve({ port, hostname: "127.0.0.1", fetch: () => new Response("again") });
    again.stop(true);
    for (const h of liveHosts(join(dir, "held"))) {
      const c = await TetherClient.connect(h);
      await c.request("drain");
      c.close();
    }
  }, 30000);
});
