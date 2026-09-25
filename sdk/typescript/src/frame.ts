// Frames, as spec/protocol.md defines them: `u32 LE length | u8 type | payload`. The decoder
// takes bytes as they arrive and yields frames as they complete.

export const MAX_PAYLOAD = 16 * 1024 * 1024;
export const MAX_JSON = 1024 * 1024;
const HEADER = 5;

export const FrameType = {
  Handshake: 0x00,
  Message: 0x01,
  Output: 0x02,
  Input: 0x03,
  Resync: 0x04,
} as const;

export type Frame =
  | { type: "handshake"; json: unknown }
  | { type: "message"; json: unknown }
  | { type: "output"; stream: number; seq: number; data: Uint8Array }
  | { type: "input"; stream: number; data: Uint8Array }
  | { type: "resync"; stream: number; seq: number; data: Uint8Array }
  | { type: "unknown"; code: number };

const utf8 = new TextEncoder();
const text = new TextDecoder();

function header(len: number, type: number, extra: number): Uint8Array {
  const out = new Uint8Array(HEADER + extra + len);
  const view = new DataView(out.buffer);
  view.setUint32(0, extra + len, true);
  out[4] = type;
  return out;
}

export function encodeJson(type: number, value: unknown): Uint8Array {
  const body = utf8.encode(JSON.stringify(value));
  const out = header(body.length, type, 0);
  out.set(body, HEADER);
  return out;
}

export function encodeInput(stream: number, data: Uint8Array): Uint8Array {
  const out = header(data.length, FrameType.Input, 4);
  new DataView(out.buffer).setUint32(HEADER, stream, true);
  out.set(data, HEADER + 4);
  return out;
}

/** Output and Resync: a host sends these, but tests and fakes build them too. */
export function encodeSeq(type: number, stream: number, seq: number, data: Uint8Array): Uint8Array {
  const out = header(data.length, type, 12);
  const view = new DataView(out.buffer);
  view.setUint32(HEADER, stream, true);
  view.setBigUint64(HEADER + 4, BigInt(seq), true);
  out.set(data, HEADER + 12);
  return out;
}

export class FrameError extends Error {}

export class Decoder {
  private buf = new Uint8Array(0);

  push(bytes: Uint8Array): void {
    if (this.buf.length === 0) {
      this.buf = bytes.slice();
      return;
    }
    const next = new Uint8Array(this.buf.length + bytes.length);
    next.set(this.buf);
    next.set(bytes, this.buf.length);
    this.buf = next;
  }

  /** The next complete frame; `undefined` when more bytes are needed. Throws for a frame the connection must be closed over. */
  next(): Frame | undefined {
    if (this.buf.length < HEADER) return undefined;
    const view = new DataView(this.buf.buffer, this.buf.byteOffset, this.buf.byteLength);
    const len = view.getUint32(0, true);
    const type = this.buf[4]!;
    const json = type === FrameType.Handshake || type === FrameType.Message;
    if (len > MAX_PAYLOAD || (json && len > MAX_JSON)) throw new FrameError(`frame of ${len} bytes is over the limit`);
    if (this.buf.length < HEADER + len) return undefined;
    const p = this.buf.subarray(HEADER, HEADER + len);
    const frame = decode(type, p);
    this.buf = this.buf.subarray(HEADER + len);
    return frame;
  }

  pending(): number {
    return this.buf.length;
  }
}

function decode(type: number, p: Uint8Array): Frame {
  const view = new DataView(p.buffer, p.byteOffset, p.byteLength);
  switch (type) {
    case FrameType.Handshake:
      return { type: "handshake", json: JSON.parse(text.decode(p)) };
    case FrameType.Message:
      return { type: "message", json: JSON.parse(text.decode(p)) };
    case FrameType.Output:
    case FrameType.Resync: {
      if (p.length < 12) throw new FrameError("a stream frame is shorter than its header");
      const stream = view.getUint32(0, true);
      const seq = Number(view.getBigUint64(4, true));
      const data = p.slice(12);
      return type === FrameType.Output ? { type: "output", stream, seq, data } : { type: "resync", stream, seq, data };
    }
    case FrameType.Input:
      if (p.length < 4) throw new FrameError("an input frame is shorter than its header");
      return { type: "input", stream: view.getUint32(0, true), data: p.slice(4) };
    default:
      return { type: "unknown", code: type };
  }
}
