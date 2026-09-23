import type { LogTail } from "@/lib/api";
import type { FullLogState } from "@/components/log-tail-banner";
import { appendTail } from "./log-tail";
import { splitLogLines } from "./log-lines";

/**
 * The job page's step-log data as one pure, request-ordered state model
 * (spec § 3.6). Every request — tail poll or full load — carries a sequence
 * number from ONE counter that the hook never resets; the model orders
 * responses by that number, never by when they arrive.
 *
 * Invariants — each transition below names the ones it keeps:
 *
 *  I1  Only a request this state saw start can change it. A completion
 *      whose seq is not in flight is ignored, and `reset` empties the
 *      in-flight sets, so nothing from an earlier generation ever lands.
 *  I2  `tail` is the newest-by-seq response that carried information: a
 *      body; the first answer before any body; an empty-but-truncated
 *      answer once a body was seen (C3). An older response never replaces
 *      a newer one, and a cold-replica "" carries nothing.
 *  I3  `full`, when present, is exactly the snapshot with seq `fullSeq`
 *      followed by every non-empty tail with a greater seq, folded with
 *      `appendTail` in ASCENDING seq order — whatever order they arrived
 *      in. Nothing with a smaller seq than the snapshot touches it.
 *  I4  A non-empty tail is left out of the view only when a snapshot with a
 *      greater seq exists; that snapshot was taken after the tail's
 *      response and so contains it.
 *  I5  A snapshot is replaced only by a snapshot with a greater seq.
 */

export interface TailEntry {
  seq: number;
  lines: string[];
}

export interface StepLogState {
  /** Newest informative tail response (I2), or null before the first. */
  tail: LogTail | null;
  /** Seq of the response `tail` reflects; 0 before any. */
  tailSeq: number;
  /** The full view (snapshot ⊕ later tails), or null in tail mode. */
  full: string[] | null;
  /** Seq of the snapshot `full` is built on; 0 while `full` is null. */
  fullSeq: number;
  /**
   * The fold of the snapshot and every later tail that can no longer be
   * preceded by a late arrival. `full === fold(base, pending)` always; in
   * the common case (no overlapping requests) `base === full`.
   */
  base: string[] | null;
  /**
   * Non-empty tails with seq > fullSeq, in ascending seq order, kept while
   * some request with a SMALLER seq is still in flight — an older poll
   * that could still land and has to be folded in before them, or an older
   * load that will replay them onto its snapshot. Empty when nothing is
   * in flight.
   */
  pending: readonly TailEntry[];
  inFlightPolls: ReadonlySet<number>;
  inFlightLoads: ReadonlySet<number>;
  /** Greatest seq of a failed load; 0 when none failed. */
  failedLoadSeq: number;
}

export type StepLogEvent =
  | { type: "pollStarted"; seq: number }
  | { type: "pollDone"; seq: number; tail: LogTail }
  | { type: "pollFailed"; seq: number }
  | { type: "loadStarted"; seq: number }
  | { type: "loadDone"; seq: number; text: string }
  | { type: "loadFailed"; seq: number }
  | { type: "reset" };

export function initialState(): StepLogState {
  return {
    tail: null,
    tailSeq: 0,
    full: null,
    fullSeq: 0,
    base: null,
    pending: [],
    inFlightPolls: new Set(),
    inFlightLoads: new Set(),
    failedLoadSeq: 0,
  };
}

export function reduce(state: StepLogState, event: StepLogEvent): StepLogState {
  switch (event.type) {
    case "reset":
      // I1: a new generation starts empty; the seq counter lives in the
      // hook and keeps counting, so an old seq can never collide with a
      // new one.
      return initialState();
    case "pollStarted":
      return { ...state, inFlightPolls: withSeq(state.inFlightPolls, event.seq) };
    case "loadStarted":
      return { ...state, inFlightLoads: withSeq(state.inFlightLoads, event.seq) };
    case "pollFailed": {
      if (!state.inFlightPolls.has(event.seq)) return state; // I1
      return prune({ ...state, inFlightPolls: withoutSeq(state.inFlightPolls, event.seq) });
    }
    case "pollDone": {
      if (!state.inFlightPolls.has(event.seq)) return state; // I1
      let next: StepLogState = { ...state, inFlightPolls: withoutSeq(state.inFlightPolls, event.seq) };
      next = applyTail(next, event.seq, event.tail);
      const lines = splitLogLines(event.tail.logs);
      if (lines.length > 0) next = receiveLines(next, event.seq, lines);
      return prune(next);
    }
    case "loadDone": {
      if (!state.inFlightLoads.has(event.seq)) return state; // I1
      const next: StepLogState = { ...state, inFlightLoads: withoutSeq(state.inFlightLoads, event.seq) };
      // I5: a newer snapshot already landed; this one is stale.
      if (next.full !== null && event.seq < next.fullSeq) return prune(next);
      // I3/I4: the snapshot supersedes every tail requested before it and
      // is followed by every tail requested after it, in seq order.
      const base = splitLogLines(event.text);
      const pending = next.pending.filter((entry) => entry.seq > event.seq);
      return prune({ ...next, base, full: fold(base, pending), fullSeq: event.seq, pending });
    }
    case "loadFailed": {
      if (!state.inFlightLoads.has(event.seq)) return state; // I1
      // The view is untouched: polls during the load were applied to it as
      // they came (see `receiveLines`), so nothing is lost with the load.
      return prune({
        ...state,
        inFlightLoads: withoutSeq(state.inFlightLoads, event.seq),
        failedLoadSeq: Math.max(state.failedLoadSeq, event.seq),
      });
    }
  }
}

/** Banner state: an in-flight load shows as loading; a failure counts only
 * until a NEWER load succeeds (a stale load's failure says nothing about
 * the view). */
export function fullStateOf(state: StepLogState): FullLogState {
  if (state.inFlightLoads.size > 0) return "loading";
  if (state.failedLoadSeq > state.fullSeq) return "error";
  return state.full !== null ? "loaded" : "idle";
}

/** I2. Which responses carry information mirrors the pre-model hook: a body
 * always; an empty answer only before any body, or — once a body was seen —
 * when it says `truncated` (C3: adopt the flag and the larger bound, keep
 * the body). A cold-replica "" is ignored and does not advance `tailSeq`,
 * so an older in-flight body is not shadowed by it. */
function applyTail(state: StepLogState, seq: number, data: LogTail): StepLogState {
  if (seq <= state.tailSeq) return state;
  const hasLogs = state.tail !== null && state.tail.logs !== "";
  if (data.logs || !hasLogs) return { ...state, tail: data, tailSeq: seq };
  if (data.truncated && state.tail) {
    return {
      ...state,
      tail: { ...state.tail, truncated: true, total_bytes: Math.max(state.tail.total_bytes, data.total_bytes) },
      tailSeq: seq,
    };
  }
  return state;
}

/** I3/I4. A tail older than the snapshot is left out (the snapshot contains
 * it). Otherwise it joins `pending` at its seq position; when it is the
 * newest the view grows by one `appendTail`, and a LATE arrival (an older
 * request that resolved after a newer one) re-folds the view from `base`
 * so the tails stay in request order. */
function receiveLines(state: StepLogState, seq: number, lines: string[]): StepLogState {
  if (seq < state.fullSeq) return state;
  const entry: TailEntry = { seq, lines };
  const last = state.pending[state.pending.length - 1];
  if (!last || last.seq < seq) {
    // A body identical to the previous entry of the same run (no in-flight
    // seq between them) changes no fold: `appendTail` is idempotent and the
    // two are always replayed or folded together. Skipping it keeps
    // `pending` at one entry per run for a quiet step while a load is in
    // flight, instead of one per poll.
    if (last && sameLines(last.lines, lines) && !inFlightBetween(state, last.seq, seq)) return state;
    const pending = [...state.pending, entry];
    const full = state.full === null ? null : appendTail(state.full, lines).lines;
    return { ...state, pending, full };
  }
  const at = state.pending.findIndex((e) => e.seq > seq);
  const pending = [...state.pending.slice(0, at), entry, ...state.pending.slice(at)];
  const full = state.full === null ? null : fold(state.base ?? [], pending);
  return { ...state, pending, full };
}

/** Fold the leading `pending` entries that no in-flight request can precede
 * into `base` (they can neither be reordered by a late poll nor replayed by
 * an older load any more). `full` does not change: it already is
 * `fold(base, pending)`. */
function prune(state: StepLogState): StepLogState {
  const horizon = minInFlight(state);
  let safe = 0;
  while (safe < state.pending.length && state.pending[safe].seq < horizon) safe += 1;
  if (safe === 0) return state;
  const rest = state.pending.slice(safe);
  if (state.full === null) return { ...state, pending: rest };
  const base = rest.length === 0 ? state.full : fold(state.base ?? [], state.pending.slice(0, safe));
  return { ...state, base, pending: rest };
}

function fold(base: string[], entries: readonly TailEntry[]): string[] {
  return entries.reduce((acc, entry) => appendTail(acc, entry.lines).lines, base);
}

function minInFlight(state: StepLogState): number {
  let min = Infinity;
  for (const seq of state.inFlightPolls) if (seq < min) min = seq;
  for (const seq of state.inFlightLoads) if (seq < min) min = seq;
  return min;
}

function inFlightBetween(state: StepLogState, lo: number, hi: number): boolean {
  for (const seq of state.inFlightPolls) if (seq > lo && seq < hi) return true;
  for (const seq of state.inFlightLoads) if (seq > lo && seq < hi) return true;
  return false;
}

function sameLines(a: readonly string[], b: readonly string[]): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

function withSeq(set: ReadonlySet<number>, seq: number): ReadonlySet<number> {
  const next = new Set(set);
  next.add(seq);
  return next;
}

function withoutSeq(set: ReadonlySet<number>, seq: number): ReadonlySet<number> {
  const next = new Set(set);
  next.delete(seq);
  return next;
}
