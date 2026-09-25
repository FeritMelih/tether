// Typing into a program the way a person would, for applications that drive terminal UIs.
// Nothing here knows any program: what counts as ready, typed or selected is the caller's to
// say, from the screen. The rules are what projects that type into agent TUIs learned:
// submit a message as a bracketed paste, wait, then press Enter on its own and check it
// landed (Enter is swallowed while a UI is still starting); answer a dialog with keys, never
// a paste, because the escape that opens a paste cancels a choice.

import type { Screen, TetherClient } from "./client.ts";
import { TetherError } from "./client.ts";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

export interface WaitOptions {
  timeoutMs?: number;
  intervalMs?: number;
}

/**
 * Polls the screen until `test` returns something other than `undefined` or `false`, and
 * returns that; throws `timeout` with the last screen's text when it never does.
 */
export async function waitForScreen<T>(c: TetherClient, session: string, test: (s: Screen) => T | undefined | false, opts: WaitOptions = {}): Promise<T> {
  const deadline = Date.now() + (opts.timeoutMs ?? 10000);
  let last: Screen | undefined;
  for (;;) {
    last = await c.screen(session);
    const r = test(last);
    if (r !== undefined && r !== false) return r;
    if (Date.now() >= deadline) throw new TetherError("timeout", `the screen never matched; it reads:\n${(last.lines ?? []).join("\n")}`);
    await sleep(opts.intervalMs ?? 100);
  }
}

/** Waits until the program has written nothing for `ms`. */
export async function quiet(c: TetherClient, session: string, ms: number, opts: WaitOptions = {}): Promise<void> {
  const deadline = Date.now() + (opts.timeoutMs ?? 10000);
  let seq = (await c.info(session)).seq;
  let since = Date.now();
  for (;;) {
    await sleep(Math.min(opts.intervalMs ?? 50, ms));
    const now = (await c.info(session)).seq;
    if (now !== seq) {
      seq = now;
      since = Date.now();
    } else if (Date.now() - since >= ms) {
      return;
    }
    if (Date.now() >= deadline) throw new TetherError("timeout", `the program never went quiet for ${ms} ms`);
  }
}

export interface SubmitOptions {
  /** Between the paste and Enter. */
  delayMs?: number;
  /** Whether the program is ready to be typed into; waited for before anything is typed. */
  ready?: () => boolean | Promise<boolean>;
  /** Whether the text landed; waited for after Enter. */
  verify?: () => boolean | Promise<boolean>;
  /** Whether the text still sits unsent where it was typed: then Enter is pressed once more. */
  pending?: () => boolean | Promise<boolean>;
  timeoutMs?: number;
}

async function until(test: () => boolean | Promise<boolean>, deadline: number, intervalMs = 100): Promise<boolean> {
  for (;;) {
    if (await test()) return true;
    if (Date.now() >= deadline) return false;
    await sleep(intervalMs);
  }
}

/**
 * Types a message and sends it: a bracketed paste (when the program asked for bracketed
 * pastes), a pause, then Enter. With `verify`, waits for it to land, pressing Enter once more
 * if `pending` says the text is still waiting to be sent. Returns whether it was verified
 * (true when there was nothing to verify) and how many times Enter was pressed.
 */
export async function submit(c: TetherClient, session: string, text: string, opts: SubmitOptions = {}): Promise<{ verified: boolean; enters: number }> {
  const deadline = Date.now() + (opts.timeoutMs ?? 30000);
  if (opts.ready && !(await until(opts.ready, deadline))) throw new TetherError("timeout", "the program was never ready to be typed into");
  await c.paste(session, text);
  await sleep(opts.delayMs ?? 300);
  await c.keys(session, ["Enter"]);
  if (!opts.verify) return { verified: true, enters: 1 };
  const half = Date.now() + Math.max(1000, (deadline - Date.now()) / 2);
  if (await until(opts.verify, Math.min(half, deadline))) return { verified: true, enters: 1 };
  if (opts.pending && (await opts.pending())) {
    await c.keys(session, ["Enter"]);
    return { verified: await until(opts.verify, deadline), enters: 2 };
  }
  return { verified: await until(opts.verify, deadline), enters: 1 };
}

export interface SelectOptions extends WaitOptions {
  /** Whether the wanted row is selected, from the screen. */
  isSelected: (s: Screen) => boolean;
  /** The key that moves the selection; `Down` by default. */
  key?: string;
  /** How many presses before giving up. */
  max?: number;
  /** Press Enter once it is selected. */
  confirm?: boolean;
}

/** Moves a selection with a key until the screen shows the wanted row selected. */
export async function selectRow(c: TetherClient, session: string, opts: SelectOptions): Promise<void> {
  const max = opts.max ?? 20;
  for (let i = 0; i <= max; i++) {
    const found = await waitForScreen(c, session, (s) => opts.isSelected(s) || undefined, { timeoutMs: i === 0 ? 0 : (opts.timeoutMs ?? 1500), intervalMs: opts.intervalMs ?? 50 }).catch(() => false);
    if (found) {
      if (opts.confirm) await c.keys(session, ["Enter"]);
      return;
    }
    if (i < max) await c.keys(session, [opts.key ?? "Down"]);
  }
  throw new TetherError("timeout", `the row was never selected after ${max} presses`);
}

/**
 * Chooses a numbered row by its digit, then, once `isSelected` agrees (when given), confirms
 * with Enter.
 */
export async function chooseDigit(c: TetherClient, session: string, digit: number, opts: { isSelected?: (s: Screen) => boolean; confirm?: boolean } & WaitOptions = {}): Promise<void> {
  if (!Number.isInteger(digit) || digit < 0 || digit > 9) throw new TetherError("bad_request", `${digit} is not a digit`);
  await c.keys(session, [String(digit)]);
  if (opts.isSelected) await waitForScreen(c, session, (s) => opts.isSelected!(s) || undefined, { timeoutMs: opts.timeoutMs ?? 3000, intervalMs: opts.intervalMs ?? 50 });
  if (opts.confirm ?? true) await c.keys(session, ["Enter"]);
}
