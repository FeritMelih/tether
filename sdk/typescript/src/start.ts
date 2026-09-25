// Finding the host new sessions go to, and starting one when there is none: `tether serve
// --daemonize` does everything platform-specific (a detached process that inherits no
// handle) and prints the new host's file once it is listening. A host older than the binary
// given is replaced for new sessions and told to drain, keeping its own until they end.

import { execFile } from "node:child_process";
import { closeSync, mkdirSync, openSync, rmSync, statSync } from "node:fs";
import { join } from "node:path";
import { TetherClient, TetherError } from "./client.ts";
import type { ConnectOptions } from "./client.ts";
import { currentHost, liveHosts, stateDir } from "./discovery.ts";
import type { HostFile } from "./discovery.ts";

export interface StartOptions extends ConnectOptions {
  /** The `tether` binary to start a host with. */
  exe: string;
  /** The state directory; `stateDir()` by default. */
  dir?: string;
  /** Passed to `serve` as `--idle-exit`, in seconds. */
  idleExitS?: number;
  /** The binary's version, when already known; read from `exe --version` otherwise. */
  exeVersion?: string;
}

function run(exe: string, args: string[], env: Record<string, string | undefined>, timeoutMs: number): Promise<{ code: number; out: string; err: string }> {
  return new Promise((resolve) => {
    execFile(exe, args, { env: env as NodeJS.ProcessEnv, timeout: timeoutMs, windowsHide: true }, (error, stdout, stderr) => {
      const code = error ? (typeof (error as { code?: unknown }).code === "number" ? ((error as { code: number }).code) : 1) : 0;
      resolve({ code, out: String(stdout), err: String(stderr) });
    });
  });
}

/** Semantic versions by their numbers. */
export function newer(a: string, b: string): boolean {
  const parse = (v: string) => v.split(/[.+-]/).slice(0, 3).map((p) => Number.parseInt(p, 10) || 0);
  const [x, y] = [parse(a), parse(b)];
  for (let i = 0; i < 3; i++) if ((x[i] ?? 0) !== (y[i] ?? 0)) return (x[i] ?? 0) > (y[i] ?? 0);
  return false;
}

export async function exeVersion(exe: string): Promise<string | undefined> {
  const r = await run(exe, ["--version"], process.env, 10000);
  return r.code === 0 ? r.out.trim().split(/\s+/).pop() : undefined;
}

/** Runs `exe serve --daemonize` and returns the host file it prints. */
export async function startHost(opts: StartOptions): Promise<HostFile> {
  const dir = opts.dir ?? stateDir();
  const args = ["serve", "--daemonize"];
  if (opts.idleExitS !== undefined) args.push("--idle-exit", String(opts.idleExitS));
  const r = await run(opts.exe, args, { ...process.env, TETHER_DIR: dir }, 30000);
  if (r.code !== 0) throw new TetherError("unavailable", `the host did not start: ${r.err.trim() || `exit ${r.code}`}`);
  try {
    return JSON.parse(r.out) as HostFile;
  } catch {
    throw new TetherError("unavailable", `the host's announcement did not parse: ${r.out.slice(0, 200)}`);
  }
}

/** The right to start a host, held while `start.lock` exists; a lock older than 10 s was left by a starter that died. */
async function takeStartLock(dir: string): Promise<() => void> {
  mkdirSync(dir, { recursive: true });
  const path = join(dir, "start.lock");
  const deadline = Date.now() + 15000;
  for (;;) {
    try {
      closeSync(openSync(path, "wx"));
      return () => rmSync(path, { force: true });
    } catch (e) {
      if ((e as NodeJS.ErrnoException).code !== "EEXIST") throw e;
      let stale = true;
      try {
        stale = Date.now() - statSync(path).mtimeMs > 10000;
      } catch {
        // Gone already.
      }
      if (stale) {
        rmSync(path, { force: true });
        continue;
      }
      if (Date.now() > deadline) throw new TetherError("unavailable", `${path} is held`);
      await new Promise((r) => setTimeout(r, 50));
    }
  }
}

/** The host new sessions go to, started if need be; an older one is told to drain. */
export async function connectOrStart(opts: StartOptions): Promise<TetherClient> {
  const dir = opts.dir ?? stateDir();
  const mine = opts.exeVersion ?? (await exeVersion(opts.exe));
  const usable = (h: HostFile | undefined): h is HostFile => h !== undefined && !(mine && newer(mine, h.version));
  const tryCurrent = async (): Promise<TetherClient | undefined> => {
    const h = currentHost(dir);
    if (!usable(h)) return undefined;
    return TetherClient.connect(h, opts).catch(() => undefined);
  };
  const found = await tryCurrent();
  if (found) return found;
  const release = await takeStartLock(dir);
  try {
    const raced = await tryCurrent();
    if (raced) return raced;
    const older = liveHosts(dir).filter((h) => !h.draining && mine !== undefined && newer(mine, h.version));
    const started = await startHost({ ...opts, dir });
    const client = await TetherClient.connect(started, opts);
    for (const h of older) {
      const old = await TetherClient.connect(h, opts).catch(() => undefined);
      if (!old) continue;
      await old.request("drain").catch(() => undefined);
      old.close();
    }
    return client;
  } finally {
    release();
  }
}

/** A client for every live host, newest first; hosts that cannot be reached are skipped. */
export async function connectAll(opts: ConnectOptions & { dir?: string }): Promise<TetherClient[]> {
  const dir = opts.dir ?? stateDir();
  const out: TetherClient[] = [];
  for (const h of liveHosts(dir)) {
    const c = await TetherClient.connect(h, opts).catch(() => undefined);
    if (c) out.push(c);
  }
  return out;
}
