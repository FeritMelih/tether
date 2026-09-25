// A client for tether hosts: discovery, the handshake, requests, events, streams, starting a
// host, and helpers for typing into terminal UIs. No runtime dependencies beyond Node's (or
// Bun's) own modules.

export { PROTOCOL, TetherClient, TetherError } from "./client.ts";
export type { ClientInfo, ConnectOptions, EnvSpec, Run, Screen, SessionInfo, SpawnParams, StreamHandlers, SubscribeParams, Subscription, TetherEvent, Welcome } from "./client.ts";
export { currentHost, liveHosts, processAlive, readCurrent, readHosts, stateDir } from "./discovery.ts";
export type { HostFile } from "./discovery.ts";
export { Decoder, encodeInput, encodeJson, encodeSeq, FrameError, FrameType } from "./frame.ts";
export type { Frame } from "./frame.ts";
export { CLIENT_LABEL, hex, HOST_LABEL, nonce, proof, unhex, verify } from "./handshake.ts";
export { chooseDigit, quiet, selectRow, submit, waitForScreen } from "./helpers.ts";
export type { SelectOptions, SubmitOptions, WaitOptions } from "./helpers.ts";
export { connectAll, connectOrStart, exeVersion, newer, startHost } from "./start.ts";
export type { StartOptions } from "./start.ts";
