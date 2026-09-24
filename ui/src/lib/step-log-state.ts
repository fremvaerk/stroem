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
 *  I2  `body` is the newest-by-seq NON-EMPTY tail; `meta` (truncated, the
 *      size bound) is from the newest-by-seq response that is non-empty or
 *      truncated. An empty, untruncated answer — a cold replica, or a truly
 *      empty log — carries no information: it advances neither and never
 *      blocks an older response. An older response never replaces a newer.
 *  I3  `full`, when present, is exactly the snapshot with seq `fullSeq`
 *      followed by every non-empty tail with a greater seq, folded with
 *      `appendTail` in ASCENDING seq order — whatever order they arrived
 *      in. Nothing with a smaller seq than the snapshot touches it.
 *  I4  A non-empty tail is left out of the view only when a snapshot with a
 *      greater seq exists; that snapshot was taken after the tail's
 *      response and so contains it.
 *  I5  A snapshot is replaced only by a snapshot with a greater seq, and at
 *      most one load is in flight: starting a load retires every older one.
 *  I6  History is bounded: `pending` holds only what some in-flight request
 *      still needs, and past the hard cap the oldest in-flight request is
 *      abandoned so pruning can advance.
 */

/** Hard cap on `pending` (I6): past either bound the oldest in-flight
 * request is abandoned. Entries are one per non-empty poll while an older
 * request is in flight; chars count the retained line text in UTF-16 code
 * units (bytes, for ASCII logs). */
export const MAX_PENDING_ENTRIES = 64;
export const MAX_PENDING_CHARS = 8 * 1024 * 1024;

export interface TailEntry {
  seq: number;
  lines: string[];
  /** Sum of `lines[i].length`, for the cap. */
  chars: number;
}

export interface TailBody {
  logs: string;
  returned_bytes: number;
}

export interface TailMeta {
  truncated: boolean;
  total_bytes: number;
}

export interface StepLogState {
  /** Newest non-empty tail body (I2), or null before the first. */
  body: TailBody | null;
  /** Seq of the response `body` reflects; 0 before any. */
  bodySeq: number;
  /** Truncation and size bound from the newest informative response (I2). */
  meta: TailMeta | null;
  /** Seq of the response `meta` reflects; 0 before any. */
  metaSeq: number;
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
  /** The request was given up on without a result (retired, aborted on
   * unmount, evicted by the cap); not an error. */
  | { type: "requestAbandoned"; seq: number }
  | { type: "reset" };

export function initialState(): StepLogState {
  return {
    body: null,
    bodySeq: 0,
    meta: null,
    metaSeq: 0,
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
    case "loadStarted": {
      // I5: the new load supersedes every older one still in flight — the
      // newer snapshot would win anyway, so nothing they could deliver is
      // needed. Retirement is not a failure.
      const next = retireLoadsBefore(state, event.seq);
      return prune({ ...next, inFlightLoads: withSeq(next.inFlightLoads, event.seq) });
    }
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
      return enforceCap(prune(next));
    }
    case "loadDone": {
      if (!state.inFlightLoads.has(event.seq)) return state; // I1
      let next: StepLogState = { ...state, inFlightLoads: withoutSeq(state.inFlightLoads, event.seq) };
      // I5: a newer snapshot already landed; this one is stale.
      if (next.full !== null && event.seq < next.fullSeq) return prune(next);
      next = retireLoadsBefore(next, event.seq);
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
    case "requestAbandoned": {
      if (state.inFlightPolls.has(event.seq)) {
        return prune({ ...state, inFlightPolls: withoutSeq(state.inFlightPolls, event.seq) });
      }
      if (state.inFlightLoads.has(event.seq)) {
        return prune({ ...state, inFlightLoads: withoutSeq(state.inFlightLoads, event.seq) });
      }
      return state; // I1
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

/** I2. Body and metadata are tracked separately by seq. A non-empty
 * response carries both. An empty-but-truncated one (C3: a scan cap or an
 * over-window newest line) raises `truncated` and the bound but leaves the
 * body — and its seq — alone. An empty, untruncated one is ignored
 * entirely, so it can never shadow an older body still in flight. */
function applyTail(state: StepLogState, seq: number, data: LogTail): StepLogState {
  const nonEmpty = data.logs !== "";
  if (!nonEmpty && !data.truncated) return state;
  let next = state;
  if (nonEmpty && seq > state.bodySeq) {
    next = { ...next, body: { logs: data.logs, returned_bytes: data.returned_bytes }, bodySeq: seq };
  }
  if (seq > state.metaSeq) {
    const meta: TailMeta = nonEmpty
      ? { truncated: data.truncated, total_bytes: data.total_bytes }
      : { truncated: true, total_bytes: Math.max(state.meta?.total_bytes ?? 0, data.total_bytes) };
    next = { ...next, meta, metaSeq: seq };
  }
  return next;
}

/** I3/I4. A tail older than the snapshot is left out (the snapshot contains
 * it). Otherwise it joins `pending` at its seq position; when it is the
 * newest the view grows by one `appendTail`, and a LATE arrival (an older
 * request that resolved after a newer one) re-folds the view from `base`
 * so the tails stay in request order. */
function receiveLines(state: StepLogState, seq: number, lines: string[]): StepLogState {
  if (seq < state.fullSeq) return state;
  const last = state.pending[state.pending.length - 1];
  if (!last || last.seq < seq) {
    // A body identical to the previous entry of the same run (no in-flight
    // seq between them) changes no fold: `appendTail` is idempotent and the
    // two are always replayed or folded together. Skipping it keeps
    // `pending` at one entry per run for a quiet step while a load is in
    // flight, instead of one per poll.
    if (last && sameLines(last.lines, lines) && !inFlightBetween(state, last.seq, seq)) return state;
    const pending = [...state.pending, entryOf(seq, lines)];
    const full = state.full === null ? null : appendTail(state.full, lines).lines;
    return { ...state, pending, full };
  }
  const at = state.pending.findIndex((e) => e.seq > seq);
  const pending = [...state.pending.slice(0, at), entryOf(seq, lines), ...state.pending.slice(at)];
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

/** I6. While `pending` is over the cap, abandon the oldest in-flight
 * request — the one pinning the history — and prune again. An abandoned
 * poll is simply gone; an abandoned LOAD is what the user asked for and
 * will now never arrive, so it counts as a failed load. */
function enforceCap(state: StepLogState): StepLogState {
  let next = state;
  while (overCap(next.pending)) {
    const oldest = oldestInFlight(next);
    if (oldest === null) return next;
    next =
      oldest.kind === "poll"
        ? { ...next, inFlightPolls: withoutSeq(next.inFlightPolls, oldest.seq) }
        : {
            ...next,
            inFlightLoads: withoutSeq(next.inFlightLoads, oldest.seq),
            failedLoadSeq: Math.max(next.failedLoadSeq, oldest.seq),
          };
    next = prune(next);
  }
  return next;
}

function overCap(pending: readonly TailEntry[]): boolean {
  if (pending.length > MAX_PENDING_ENTRIES) return true;
  let chars = 0;
  for (const entry of pending) chars += entry.chars;
  return chars > MAX_PENDING_CHARS;
}

function retireLoadsBefore(state: StepLogState, seq: number): StepLogState {
  let loads: ReadonlySet<number> | null = null;
  for (const s of state.inFlightLoads) {
    if (s < seq) loads = withoutSeq(loads ?? state.inFlightLoads, s);
  }
  return loads === null ? state : { ...state, inFlightLoads: loads };
}

function entryOf(seq: number, lines: string[]): TailEntry {
  let chars = 0;
  for (const line of lines) chars += line.length;
  return { seq, lines, chars };
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

function oldestInFlight(state: StepLogState): { seq: number; kind: "poll" | "load" } | null {
  let oldest: { seq: number; kind: "poll" | "load" } | null = null;
  for (const seq of state.inFlightPolls) if (!oldest || seq < oldest.seq) oldest = { seq, kind: "poll" };
  for (const seq of state.inFlightLoads) if (!oldest || seq < oldest.seq) oldest = { seq, kind: "load" };
  return oldest;
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
