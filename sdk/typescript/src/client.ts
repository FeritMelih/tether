// A client for a tether host over its local endpoint: the handshake (each side proves the
// token without sending it), then requests answered by id, events, and subscriptions whose
// output arrives in order. A subscription's handlers are given with the request, because its
// first frame follows the response directly and must not find nobody listening.

import { connect } from "node:net";
import type { Socket } from "node:net";
import type { HostFile } from "./discovery.ts";
import { Decoder, encodeInput, encodeJson, FrameType } from "./frame.ts";
import type { Frame } from "./frame.ts";
import { CLIENT_LABEL, hex, HOST_LABEL, nonce, proof, unhex, verify } from "./handshake.ts";

export const PROTOCOL = 1;

export class TetherError extends Error {
  readonly code: string;
  constructor(code: string, message: string) {
    super(message);
    this.code = code;
    this.name = "TetherError";
  }
}

export interface Welcome {
  host: string;
  version: string;
  protocol: number;
  pid: number;
  caps: string[];
}

export interface ClientInfo {
  client: string;
  stream: number;
  pid?: number;
  name?: string;
  role: "window" | "viewer" | "control";
  input: boolean;
  sizing: boolean;
  cols?: number;
  rows?: number;
  attachedAt: number;
  lastInput?: number;
}

export interface SessionInfo {
  session: string;
  name?: string;
  pid?: number;
  argv: string[];
  cwd: string;
  cols: number;
  rows: number;
  title?: string;
  cwdReported?: string;
  labels: Record<string, string>;
  /** The program runs on when its last window closes; absent from a host before 0.2.0, where every program did. */
  keep?: boolean;
  status: "running" | "exited";
  exit?: { code: number; signal?: string };
  startedAt: number;
  exitedAt?: number;
  seq: number;
  clients: ClientInfo[];
}

export interface Run {
  t: string;
  fg?: number | string;
  bg?: number | string;
  bold?: true;
  dim?: true;
  italic?: true;
  underline?: true;
  inverse?: true;
}

export interface Screen {
  format: "text" | "vt" | "cells";
  cols: number;
  rows: number;
  cursor: { row: number; col: number; visible: boolean };
  title?: string;
  seq: number;
  altScreen: boolean;
  bracketedPaste: boolean;
  appCursor: boolean;
  lines?: string[];
  data?: string;
  cells?: Run[][];
}

export interface TetherEvent {
  ev: string;
  session?: string;
  [key: string]: unknown;
}

export interface StreamHandlers {
  /** Output as the program wrote it; `seq` is the position of its first byte. */
  onOutput(data: Uint8Array, seq: number): void;
  /** A repaint: write it to a fresh terminal of the session's size. Output resumes at `seq`. */
  onResync(data: Uint8Array, seq: number): void;
}

export interface SubscribeParams {
  session: string;
  from?: "snapshot" | "now" | number;
  input?: boolean;
  sizing?: boolean;
  size?: { cols: number; rows: number };
  role?: "window" | "viewer" | "control";
  scrollback?: boolean;
  buffer?: number;
}

export interface Subscription {
  readonly stream: number;
  /** Where the session's output stood when the subscription began. */
  readonly seq: number;
  readonly cols: number;
  readonly rows: number;
  /** Types into the session; only a subscription opened with `input` may. */
  input(data: Uint8Array | string): boolean;
  unsubscribe(): Promise<void>;
}

interface Pending {
  resolve(v: unknown): void;
  reject(e: Error): void;
  handlers?: StreamHandlers;
}

export interface EnvSpec {
  base?: "empty" | "host";
  set?: Record<string, string>;
  unset?: string[];
}

export interface SpawnParams {
  argv: string[];
  cwd?: string;
  env?: EnvSpec;
  size?: { cols: number; rows: number };
  name?: string;
  labels?: Record<string, string>;
  scrollback?: number;
  /** Keep the program running when its last window closes; otherwise that ends it. */
  keep?: boolean;
}

const utf8 = new TextEncoder();

export interface ConnectOptions {
  /** Who this client is, in the host's client list. */
  name?: string;
  version?: string;
  timeoutMs?: number;
}

export class TetherClient {
  readonly host: HostFile;
  readonly welcome: Welcome;
  private sock: Socket;
  private decoder: Decoder;
  private nextId = 1;
  private pending = new Map<number, Pending>();
  private streams = new Map<number, StreamHandlers>();
  private eventHandlers = new Set<(e: TetherEvent) => void>();
  private closeHandlers = new Set<(why: string) => void>();
  private isClosed = false;

  private constructor(host: HostFile, welcome: Welcome, sock: Socket, decoder: Decoder) {
    this.host = host;
    this.welcome = welcome;
    this.sock = sock;
    this.decoder = decoder;
    sock.on("data", (b: Buffer) => this.onData(b));
    sock.on("close", () => this.shut("the host closed the connection"));
    sock.on("error", (e) => this.shut(e.message));
    // Bytes that came with the welcome are already in the decoder.
    queueMicrotask(() => this.drain());
  }

  /** Connects and proves the token; the host proves it back, or the connection is dropped. */
  static connect(host: HostFile, opts: ConnectOptions = {}): Promise<TetherClient> {
    return new Promise((resolve, reject) => {
      const sock = connect(host.endpoint);
      const decoder = new Decoder();
      const clientNonce = nonce();
      let hostNonce: Uint8Array | undefined;
      let settled = false;
      const fail = (e: Error) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        sock.destroy();
        reject(e);
      };
      const timer = setTimeout(() => fail(new TetherError("unavailable", `${host.endpoint}: the handshake timed out`)), opts.timeoutMs ?? 10000);
      const onData = (b: Buffer) => {
        decoder.push(new Uint8Array(b.buffer, b.byteOffset, b.byteLength));
        let f: Frame | undefined;
        try {
          f = decoder.next();
        } catch (e) {
          fail(e as Error);
          return;
        }
        while (f && !settled) {
          if (f.type !== "handshake") return fail(new TetherError("unavailable", "expected a handshake frame"));
          const j = f.json as Record<string, unknown>;
          if (j["error"]) {
            const e = j["error"] as { code: string; message: string };
            return fail(new TetherError(e.code, e.message));
          }
          if (!hostNonce) {
            hostNonce = unhex(String(j["nonce"] ?? ""));
            const given = unhex(String(j["proof"] ?? "")) ?? new Uint8Array();
            if (!hostNonce || !verify(host.token, HOST_LABEL, clientNonce, hostNonce, given)) return fail(new TetherError("auth", `${host.endpoint} did not prove it holds the token`));
            sock.write(encodeJson(FrameType.Handshake, { proof: hex(proof(host.token, CLIENT_LABEL, clientNonce, hostNonce)) }));
          } else {
            settled = true;
            clearTimeout(timer);
            sock.off("data", onData);
            sock.off("error", onError);
            sock.off("close", onClose);
            resolve(new TetherClient(host, j["welcome"] as Welcome, sock, decoder));
            return;
          }
          f = decoder.next();
        }
      };
      const onError = (e: Error) => fail(new TetherError("unavailable", `${host.endpoint}: ${e.message}`));
      const onClose = () => fail(new TetherError("unavailable", `${host.endpoint} closed the connection`));
      sock.on("data", onData);
      sock.on("error", onError);
      sock.on("close", onClose);
      sock.on("connect", () => {
        sock.write(
          encodeJson(FrameType.Handshake, {
            hello: "tether",
            protocol: [PROTOCOL, PROTOCOL],
            client: { name: opts.name ?? "tether-ts", version: opts.version ?? "0.2.0", pid: process.pid },
            caps: [],
            nonce: hex(clientNonce),
          }),
        );
      });
    });
  }

  get closed(): boolean {
    return this.isClosed;
  }

  private shut(why: string): void {
    if (this.isClosed) return;
    this.isClosed = true;
    for (const p of this.pending.values()) p.reject(new TetherError("unavailable", why));
    this.pending.clear();
    this.streams.clear();
    for (const h of this.closeHandlers) h(why);
  }

  close(): void {
    this.sock.destroy();
    this.shut("closed");
  }

  onEvent(cb: (e: TetherEvent) => void): () => void {
    this.eventHandlers.add(cb);
    return () => this.eventHandlers.delete(cb);
  }

  onClose(cb: (why: string) => void): () => void {
    this.closeHandlers.add(cb);
    return () => this.closeHandlers.delete(cb);
  }

  private onData(b: Buffer): void {
    this.decoder.push(new Uint8Array(b.buffer, b.byteOffset, b.byteLength));
    this.drain();
  }

  private drain(): void {
    for (;;) {
      let f: Frame | undefined;
      try {
        f = this.decoder.next();
      } catch (e) {
        this.sock.destroy();
        this.shut((e as Error).message);
        return;
      }
      if (!f) return;
      this.handle(f);
    }
  }

  private handle(f: Frame): void {
    switch (f.type) {
      case "message": {
        const m = f.json as Record<string, unknown>;
        if (typeof m["re"] === "number") {
          const p = this.pending.get(m["re"]);
          if (!p) return;
          this.pending.delete(m["re"]);
          if (m["error"]) {
            const e = m["error"] as { code: string; message: string };
            p.reject(new TetherError(e.code, e.message));
            return;
          }
          const ok = m["ok"] as Record<string, unknown>;
          // The stream is listened to before the frame after this one is read.
          if (p.handlers) this.streams.set(Number(ok["stream"]), p.handlers);
          p.resolve(ok);
        } else if (typeof m["ev"] === "string") {
          for (const h of this.eventHandlers) h(m as TetherEvent);
        }
        return;
      }
      case "output":
        this.streams.get(f.stream)?.onOutput(f.data, f.seq);
        return;
      case "resync":
        this.streams.get(f.stream)?.onResync(f.data, f.seq);
        return;
      default:
        return;
    }
  }

  private call<T>(op: string, fields: Record<string, unknown>, handlers?: StreamHandlers): Promise<T> {
    if (this.isClosed) return Promise.reject(new TetherError("unavailable", "the connection is closed"));
    const id = this.nextId++;
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, { resolve: resolve as (v: unknown) => void, reject, ...(handlers ? { handlers } : {}) });
      this.sock.write(encodeJson(FrameType.Message, { ...fields, id, op }));
    });
  }

  request<T = Record<string, unknown>>(op: string, fields: Record<string, unknown> = {}): Promise<T> {
    return this.call<T>(op, fields);
  }

  async subscribe(params: SubscribeParams, handlers: StreamHandlers): Promise<Subscription> {
    const r = await this.call<{ stream: number; seq: number; cols: number; rows: number }>("subscribe", { ...params }, handlers);
    const stream = r.stream;
    return {
      stream,
      seq: r.seq,
      cols: r.cols,
      rows: r.rows,
      input: (data) => {
        if (this.isClosed) return false;
        this.sock.write(encodeInput(stream, typeof data === "string" ? utf8.encode(data) : data));
        return true;
      },
      unsubscribe: async () => {
        this.streams.delete(stream);
        if (!this.isClosed) await this.call("unsubscribe", { stream }).catch(() => undefined);
      },
    };
  }

  // --- conveniences, one per request ---------------------------------------------------

  spawn(p: SpawnParams): Promise<{ session: string; pid?: number }> {
    return this.request("spawn", { ...p });
  }

  async list(labels?: Record<string, string>): Promise<SessionInfo[]> {
    const r = await this.request<{ sessions: SessionInfo[] }>("list", labels ? { labels } : {});
    return r.sessions;
  }

  info(session: string): Promise<SessionInfo> {
    return this.request("info", { session });
  }

  write(session: string, data: string | Uint8Array): Promise<void> {
    return this.request("write", typeof data === "string" ? { session, data } : { session, b64: Buffer.from(data).toString("base64") }).then(() => undefined);
  }

  paste(session: string, text: string, bracketed: "auto" | boolean = "auto"): Promise<{ bracketed: boolean }> {
    return this.request("paste", { session, text, bracketed });
  }

  keys(session: string, keys: string[]): Promise<void> {
    return this.request("keys", { session, keys }).then(() => undefined);
  }

  resize(session: string, cols: number, rows: number): Promise<void> {
    return this.request("resize", { session, cols, rows }).then(() => undefined);
  }

  screen(session: string, opts: { format?: "text" | "vt" | "cells"; scrollback?: boolean } = {}): Promise<Screen> {
    return this.request("screen", { session, ...opts });
  }

  kill(session: string, graceMs?: number): Promise<void> {
    return this.request("kill", { session, ...(graceMs !== undefined ? { graceMs } : {}) }).then(() => undefined);
  }

  watch(sessions?: string[]): Promise<void> {
    return this.request("watch", sessions ? { sessions } : {}).then(() => undefined);
  }
}
