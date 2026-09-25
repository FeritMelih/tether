// The handshake's arithmetic: nonces and the proofs each side computes to show it holds the
// token without sending it (spec/protocol.md, Handshake).

import { createHmac, randomBytes, timingSafeEqual } from "node:crypto";

export const HOST_LABEL = "tether host";
export const CLIENT_LABEL = "tether client";

export function nonce(): Uint8Array {
  return new Uint8Array(randomBytes(32));
}

export function hex(bytes: Uint8Array): string {
  return Buffer.from(bytes).toString("hex");
}

export function unhex(s: string): Uint8Array | undefined {
  if (s.length % 2 !== 0 || !/^[0-9a-fA-F]*$/.test(s)) return undefined;
  return new Uint8Array(Buffer.from(s, "hex"));
}

/** `HMAC-SHA256(token, label || client_nonce || host_nonce)`, keyed with the token's hex text. */
export function proof(token: string, label: string, clientNonce: Uint8Array, hostNonce: Uint8Array): Uint8Array {
  const mac = createHmac("sha256", Buffer.from(token, "ascii"));
  mac.update(Buffer.from(label, "ascii"));
  mac.update(clientNonce);
  mac.update(hostNonce);
  return new Uint8Array(mac.digest());
}

export function verify(token: string, label: string, clientNonce: Uint8Array, hostNonce: Uint8Array, given: Uint8Array): boolean {
  const want = proof(token, label, clientNonce, hostNonce);
  return given.length === want.length && timingSafeEqual(given, want);
}
