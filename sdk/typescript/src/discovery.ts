// Where hosts announce themselves (spec/protocol.md, Discovery): a per-user state directory
// holding one file per running host and a pointer to the newest.

import { readdirSync, readFileSync, rmSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

export interface HostFile {
  host: string;
  pid: number;
  version: string;
  protocol: number;
  endpoint: string;
  token: string;
  startedAt: number;
  draining?: boolean;
}

/** The state directory: `TETHER_DIR`, else the platform's per-user place for state. */
export function stateDir(env: Record<string, string | undefined> = process.env, platform: NodeJS.Platform = process.platform): string {
  if (env["TETHER_DIR"]) return env["TETHER_DIR"];
  if (platform === "win32") return join(env["LOCALAPPDATA"] ?? join(homedir(), "AppData", "Local"), "tether");
  if (platform === "darwin") return join(homedir(), "Library", "Application Support", "tether");
  return join(env["XDG_STATE_HOME"] || join(homedir(), ".local", "state"), "tether");
}

export function hostsDir(dir: string): string {
  return join(dir, "hosts");
}

/** Every host file that parses, newest first. */
export function readHosts(dir: string): HostFile[] {
  let names: string[];
  try {
    names = readdirSync(hostsDir(dir));
  } catch {
    return [];
  }
  const out: HostFile[] = [];
  for (const name of names) {
    if (!name.endsWith(".json")) continue;
    try {
      const f = JSON.parse(readFileSync(join(hostsDir(dir), name), "utf8")) as HostFile;
      if (typeof f.host === "string" && typeof f.endpoint === "string" && typeof f.token === "string") out.push(f);
    } catch {
      // Half-written or foreign: not a host.
    }
  }
  return out.sort((a, b) => b.startedAt - a.startedAt);
}

export function readCurrent(dir: string): string | undefined {
  try {
    const s = readFileSync(join(dir, "current"), "utf8").trim();
    return s || undefined;
  } catch {
    return undefined;
  }
}

export function processAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (e) {
    return (e as NodeJS.ErrnoException).code === "EPERM";
  }
}

/** Hosts whose process is alive, newest first; the files of dead ones are removed. */
export function liveHosts(dir: string, alive: (pid: number) => boolean = processAlive): HostFile[] {
  return readHosts(dir).filter((h) => {
    if (alive(h.pid)) return true;
    try {
      rmSync(join(hostsDir(dir), `${h.host}.json`), { force: true });
      if (readCurrent(dir) === h.host) rmSync(join(dir, "current"), { force: true });
    } catch {
      // Another client got there first.
    }
    return false;
  });
}

/** The host new sessions should go to: `current`, else the newest live host not draining. */
export function currentHost(dir: string, alive?: (pid: number) => boolean): HostFile | undefined {
  const hosts = liveHosts(dir, alive);
  const current = readCurrent(dir);
  return hosts.find((h) => h.host === current && !h.draining) ?? hosts.find((h) => !h.draining);
}
