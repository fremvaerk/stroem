import { describe, it, expect } from "vitest";
import {
  MAX_PENDING_CHARS,
  MAX_PENDING_ENTRIES,
  fullStateOf,
  initialState,
  reduce,
  type StepLogEvent,
  type StepLogState,
} from "../step-log-state";
import { LOG_GAP_MARKER } from "../log-lines";

const GAP = LOG_GAP_MARKER;

const body = (lines: string[], over: Partial<{ truncated: boolean; total_bytes: number }> = {}) => {
  const logs = lines.map((l) => `${l}\n`).join("");
  return { logs, truncated: false, total_bytes: logs.length, returned_bytes: logs.length, ...over };
};

const pollStarted = (seq: number): StepLogEvent => ({ type: "pollStarted", seq });
const pollDone = (seq: number, lines: string[], over?: Partial<{ truncated: boolean; total_bytes: number }>): StepLogEvent => ({
  type: "pollDone",
  seq,
  tail: body(lines, over),
});
const pollEmpty = (seq: number, over?: Partial<{ truncated: boolean; total_bytes: number }>): StepLogEvent => ({
  type: "pollDone",
  seq,
  tail: body([], over),
});
const pollFailed = (seq: number): StepLogEvent => ({ type: "pollFailed", seq });
const loadStarted = (seq: number): StepLogEvent => ({ type: "loadStarted", seq });
const loadDone = (seq: number, lines: string[]): StepLogEvent => ({
  type: "loadDone",
  seq,
  text: lines.map((l) => `${l}\n`).join(""),
});
const loadFailed = (seq: number): StepLogEvent => ({ type: "loadFailed", seq });
const abandoned = (seq: number): StepLogEvent => ({ type: "requestAbandoned", seq });
const reset: StepLogEvent = { type: "reset" };

/** A poll that starts and resolves with nothing else in between. */
const poll = (seq: number, lines: string[]): StepLogEvent[] => [pollStarted(seq), pollDone(seq, lines)];

const run = (events: StepLogEvent[], from: StepLogState = initialState()) => events.reduce(reduce, from);

/** The displayed lines, as the hook derives them. */
const view = (s: StepLogState) => s.full ?? (s.body?.logs ?? "").split("\n").filter((l) => l.length > 0);

/** Displayed [A,B,C,D] from a completed first load (seq 2) after a poll (seq 1). */
const loaded = () => run([...poll(1, ["C", "D"]), loadStarted(2), loadDone(2, ["A", "B", "C", "D"])]);

describe("step-log state model", () => {
  describe("round 1 C2 — a tail that arrives during the first load is stitched onto it", () => {
    it("keeps the poll's newer line and ignores an older poll that lands afterwards", () => {
      let s = run([...poll(1, ["C", "D"]), loadStarted(2), ...poll(3, ["D", "E"])]);
      expect(s.full).toBeNull();
      expect(view(s)).toEqual(["D", "E"]);
      s = run([loadDone(2, ["A", "B", "C", "D"])], s);
      expect(view(s)).toEqual(["A", "B", "C", "D", "E"]);
      // An older request (started before the load) resolving now is a no-op
      // for the view: the snapshot is newer than it.
      const before = s;
      s = run([pollDone(1, ["C", "D"])], run([pollStarted(1)], s));
      expect(view(s)).toEqual(view(before));
    });

    it("a tail already covered by the snapshot's end changes nothing", () => {
      const s = run([...poll(1, ["A", "B"]), loadStarted(2), ...poll(3, ["C", "D"]), loadDone(2, ["A", "B", "C", "D"])]);
      expect(view(s)).toEqual(["A", "B", "C", "D"]);
    });
  });

  describe("round 2 #1 — every tail fetched during a load is kept, in request order", () => {
    it("during the first load", () => {
      const s = run([
        ...poll(1, ["C", "D"]),
        loadStarted(2),
        ...poll(3, ["D", "E"]),
        ...poll(4, ["F", "G"]),
        loadDone(2, ["A", "B", "C", "D"]),
      ]);
      expect(view(s)).toEqual(["A", "B", "C", "D", "E", GAP, "F", "G"]);
      expect(fullStateOf(s)).toBe("loaded");
    });

    it("during a reload, where the polls also show up on the current view as they land", () => {
      let s = run([loadStarted(3), ...poll(4, ["D", "E"])], loaded());
      expect(view(s)).toEqual(["A", "B", "C", "D", "E"]);
      s = run([...poll(5, ["F", "G"])], s);
      expect(view(s)).toEqual(["A", "B", "C", "D", "E", GAP, "F", "G"]);
      s = run([loadDone(3, ["A", "B", "C", "D"])], s);
      expect(view(s)).toEqual(["A", "B", "C", "D", "E", GAP, "F", "G"]);
    });
  });

  describe("round 2 #2 — a poll started before the load, resolving during it, does not reorder the snapshot", () => {
    it("yields exactly the snapshot", () => {
      const s = run([pollStarted(1), loadStarted(2), pollDone(1, ["B", "C"]), loadDone(2, ["A", "B", "C", "D"])]);
      expect(view(s)).toEqual(["A", "B", "C", "D"]);
      expect(s.pending).toEqual([]);
    });
  });

  describe("round 3 #1 — a failed reload keeps what polls added during it", () => {
    it("shows the poll's line immediately and still after the failure", () => {
      let s = run([loadStarted(3), ...poll(4, ["D", "E"])], loaded());
      expect(view(s)).toEqual(["A", "B", "C", "D", "E"]);
      s = run([loadFailed(3)], s);
      expect(view(s)).toEqual(["A", "B", "C", "D", "E"]);
      expect(fullStateOf(s)).toBe("error");
      // Polling goes on stitching afterwards.
      s = run([...poll(5, ["E", "F"])], s);
      expect(view(s)).toEqual(["A", "B", "C", "D", "E", "F"]);
      expect(s.pending).toEqual([]);
    });

    it("a failed first load leaves tail mode alone", () => {
      const s = run([...poll(1, ["A"]), loadStarted(2), loadFailed(2)]);
      expect(view(s)).toEqual(["A"]);
      expect(s.full).toBeNull();
      expect(fullStateOf(s)).toBe("error");
    });
  });

  describe("round 3 #2 — an event from before `reset` never touches the new generation", () => {
    it("ignores an old load's completion and failure, and an old poll's completion", () => {
      const s = run([...poll(1, ["a-1"]), loadStarted(2), reset, ...poll(3, ["b-1"]), loadStarted(4), ...poll(5, ["b-1", "b-2"])]);
      const untouched = run([loadDone(2, ["a-0", "a-1"]), loadFailed(2), pollDone(1, ["a-9"]), pollFailed(1), abandoned(2)], s);
      expect(untouched).toBe(s);
      // The new generation's own load then finishes normally, with its
      // pending tails intact.
      const done = run([loadDone(4, ["b-0", "b-1"])], untouched);
      expect(view(done)).toEqual(["b-0", "b-1", "b-2"]);
      expect(fullStateOf(done)).toBe("loaded");
    });

    it("reset empties the state and its in-flight sets", () => {
      const s = run([...poll(1, ["A"]), loadStarted(2), reset]);
      expect(s).toEqual(initialState());
      expect(fullStateOf(s)).toBe("idle");
    });
  });

  describe("round 3 #3 — a poll started before the load that resolves after it is ignored for the view", () => {
    it("leaves the snapshot unchanged", () => {
      const s = run([pollStarted(1), loadStarted(2), loadDone(2, ["A", "B", "C", "D"]), pollDone(1, ["B", "C"])]);
      expect(view(s)).toEqual(["A", "B", "C", "D"]);
      // The tail body still reflects it: it is the newest body seen.
      expect(s.body?.logs).toBe("B\nC\n");
    });
  });

  describe("round 3 #4 / R4-1 — a newer load retires the older one; the newest request's snapshot is what stays", () => {
    it("starting a load retires an older in-flight load, whose later completion is ignored", () => {
      let s = run([...poll(1, ["C", "D"]), loadStarted(2), loadStarted(3), ...poll(4, ["E", "F"])]);
      expect([...s.inFlightLoads]).toEqual([3]);
      expect(fullStateOf(s)).toBe("loading");
      const retired = s;
      s = run([loadDone(2, ["A", "B", "C", "D"])], s);
      expect(s).toBe(retired);
      s = run([loadDone(3, ["A", "B", "C", "D", "E"])], s);
      expect(view(s)).toEqual(["A", "B", "C", "D", "E", "F"]);
      expect(s.fullSeq).toBe(3);
      expect(fullStateOf(s)).toBe("loaded");
      expect(s.pending).toEqual([]);
    });

    it("leaves loading once the newest snapshot completes even though the retired load never settled", () => {
      const s = run([loadStarted(2), loadStarted(3), loadDone(3, ["A"])]);
      expect(fullStateOf(s)).toBe("loaded");
      expect(view(s)).toEqual(["A"]);
      expect(s.inFlightLoads.size).toBe(0);
    });

    it("a tail between the two loads is superseded by the newer snapshot", () => {
      // poll 3 sits between L1 (2) and L2 (4): L2's snapshot contains it.
      let s = run([...poll(1, ["C", "D"]), loadStarted(2), ...poll(3, ["D", "E"]), loadStarted(4)]);
      expect(s.pending.map((e) => e.seq)).toEqual([]);
      s = run([loadDone(4, ["A", "B", "C", "D", "E", "F"])], s);
      expect(view(s)).toEqual(["A", "B", "C", "D", "E", "F"]);
    });

    it("a retired load's failure does not mark a newer, successful snapshot as an error", () => {
      const s = run([loadStarted(2), loadStarted(3), loadDone(3, ["A"]), loadFailed(2)]);
      expect(fullStateOf(s)).toBe("loaded");
      expect(view(s)).toEqual(["A"]);
    });

    it("a newer load's failure after an older success reports the error over the older view", () => {
      const s = run([loadStarted(2), loadDone(2, ["A"]), loadStarted(3), loadFailed(3)]);
      expect(fullStateOf(s)).toBe("error");
      expect(view(s)).toEqual(["A"]);
    });

    it("a stale snapshot never replaces a newer one (I5 guard)", () => {
      // Not reachable through the events any more (a load retires older
      // ones), so drive the guard directly.
      const s = run([loadStarted(3), loadDone(3, ["A", "B"])]);
      const withOld: StepLogState = { ...s, inFlightLoads: new Set([2]) };
      const after = reduce(withOld, loadDone(2, ["X"]));
      expect(after.full).toEqual(["A", "B"]);
      expect(after.fullSeq).toBe(3);
      expect(after.inFlightLoads.size).toBe(0);
    });
  });

  describe("R4-1 — abandonment and the hard cap", () => {
    it("abandoning the oldest in-flight request lets pending prune", () => {
      let s = run([loadStarted(3), ...poll(4, ["D", "E"]), ...poll(5, ["E", "F"])], loaded());
      expect(s.pending.map((e) => e.seq)).toEqual([4, 5]);
      expect(fullStateOf(s)).toBe("loading");
      s = run([abandoned(3)], s);
      expect(s.pending).toEqual([]);
      expect(s.base).toBe(s.full);
      // Not an error: the load was given up on, not failed.
      expect(fullStateOf(s)).toBe("loaded");
      expect(view(s)).toEqual(["A", "B", "C", "D", "E", "F"]);
    });

    it("abandoning a poll frees its slot the same way", () => {
      let s = run([pollStarted(3), pollStarted(4), pollDone(4, ["D", "E"])], loaded());
      expect(s.pending.map((e) => e.seq)).toEqual([4]);
      s = run([abandoned(3)], s);
      expect(s.pending).toEqual([]);
      expect(s.inFlightPolls.size).toBe(0);
    });

    it("abandoning a request that is not in flight is a no-op", () => {
      const s = loaded();
      expect(reduce(s, abandoned(9))).toBe(s);
    });

    it("past the entry cap the oldest in-flight request is abandoned; a load counts as failed", () => {
      // One load in flight, then distinct bodies on every poll.
      const events: StepLogEvent[] = [loadStarted(3)];
      for (let i = 0; i < MAX_PENDING_ENTRIES; i++) events.push(...poll(4 + i, [`L${i}`]));
      let s = run(events, loaded());
      expect(s.pending.length).toBe(MAX_PENDING_ENTRIES);
      expect(fullStateOf(s)).toBe("loading");
      // The 65th entry tips it over.
      s = run(poll(4 + MAX_PENDING_ENTRIES, ["L64"]), s);
      expect(s.inFlightLoads.size).toBe(0);
      expect(s.pending).toEqual([]);
      expect(fullStateOf(s)).toBe("error");
      // Nothing shown was lost: every poll is on the view, in order.
      expect(view(s).slice(4)).toEqual([GAP, "L0", ...Array.from({ length: MAX_PENDING_ENTRIES }, (_, i) => [GAP, `L${i + 1}`]).flat()]);
    });

    it("past the byte cap the oldest in-flight request is abandoned, oldest first", () => {
      const big = "x".repeat(MAX_PENDING_CHARS / 4 + 1);
      // A hung poll (2) and a load (3) are in flight; three distinct big
      // bodies land.
      let s = run([pollStarted(2), loadStarted(3), ...poll(4, [`${big}a`]), ...poll(5, [`${big}b`]), ...poll(6, [`${big}c`])], loaded());
      expect(s.pending.length).toBe(3);
      expect(s.inFlightPolls.has(2)).toBe(true);
      // The fourth exceeds 8 MiB: the hung poll goes first, and since the
      // load (3) still pins everything, it goes too.
      s = run(poll(7, [`${big}d`]), s);
      expect(s.inFlightPolls.size).toBe(0);
      expect(s.inFlightLoads.size).toBe(0);
      expect(s.pending).toEqual([]);
      expect(fullStateOf(s)).toBe("error");
    });

    it("the cap does not fire while pending is within both bounds", () => {
      const events: StepLogEvent[] = [loadStarted(3)];
      for (let i = 0; i < MAX_PENDING_ENTRIES; i++) events.push(...poll(4 + i, [`L${i}`]));
      const s = run(events, loaded());
      expect(s.inFlightLoads.has(3)).toBe(true);
      expect(fullStateOf(s)).toBe("loading");
    });
  });

  describe("polls resolving out of request order (model-only; the hook never overlaps its own polls)", () => {
    it("folds a late older poll before the newer one instead of appending it after", () => {
      // Two polls in flight; the newer resolves first, then the older one
      // with an INTERIOR tail, which `appendTail` would otherwise move to
      // the end.
      let s = run([pollStarted(3), pollStarted(4), pollDone(4, ["D", "E"])], loaded());
      expect(view(s)).toEqual(["A", "B", "C", "D", "E"]);
      s = run([pollDone(3, ["C", "D"])], s);
      expect(view(s)).toEqual(["A", "B", "C", "D", "E"]);
      expect(s.pending).toEqual([]);
      expect(s.base).toBe(s.full);
    });

    it("gives the same view as in-order arrival when the late poll has unique lines", () => {
      const inOrder = run([...poll(3, ["D", "E"]), ...poll(4, ["X", "Y"])], loaded());
      const late = run([pollStarted(3), pollStarted(4), pollDone(4, ["X", "Y"]), pollDone(3, ["D", "E"])], loaded());
      expect(view(inOrder)).toEqual(["A", "B", "C", "D", "E", GAP, "X", "Y"]);
      expect(view(late)).toEqual(view(inOrder));
    });

    it("an older tail body never replaces a newer one in tail mode", () => {
      const s = run([pollStarted(1), pollStarted(2), pollDone(2, ["B", "C"]), pollDone(1, ["A", "B"])]);
      expect(view(s)).toEqual(["B", "C"]);
      expect(s.bodySeq).toBe(2);
    });
  });

  describe("R4-2 — tail body and metadata", () => {
    it("Codex: an empty final poll from a cold replica does not suppress an older body landing later", () => {
      // Poll 1 is pending; the step goes terminal; the final poll (2) hits
      // a cold replica; poll 1 then returns the known log.
      const s = run([pollStarted(1), pollStarted(2), pollEmpty(2), pollDone(1, ["known log"])]);
      expect(view(s)).toEqual(["known log"]);
      expect(s.bodySeq).toBe(1);
      expect(s.meta).toEqual({ truncated: false, total_bytes: "known log\n".length });
    });

    it("an empty, untruncated response advances nothing", () => {
      const s = run([pollStarted(1), pollEmpty(1)]);
      expect(s.body).toBeNull();
      expect(s.meta).toBeNull();
      expect(s.bodySeq).toBe(0);
      expect(s.metaSeq).toBe(0);
      expect(view(s)).toEqual([]);
      expect(fullStateOf(s)).toBe("idle");
    });

    it("keeps the last non-empty body when a cold replica answers empty", () => {
      const s = run([...poll(1, ["A"]), pollStarted(2), pollEmpty(2)]);
      expect(view(s)).toEqual(["A"]);
      expect(s.meta?.truncated).toBe(false);
      // The empty answer carried nothing, so it does not shadow an older
      // body that is still in flight.
      const late = run([pollStarted(3), pollStarted(4), pollEmpty(4), pollDone(3, ["A", "B"])], s);
      expect(view(late)).toEqual(["A", "B"]);
    });

    it("an older empty response after a newer body changes nothing but its own in-flight slot", () => {
      const s = run([pollStarted(1), pollStarted(2), pollDone(2, ["B", "C"], { truncated: true, total_bytes: 50 })]);
      // Poll 2's body sat in `pending` only because poll 1 was still in
      // flight; its settling releases it.
      expect(s.pending.map((e) => e.seq)).toEqual([2]);
      for (const late of [pollEmpty(1), pollEmpty(1, { truncated: true, total_bytes: 999 })]) {
        const after = reduce(s, late);
        expect(after.inFlightPolls.has(1)).toBe(false);
        expect(after.pending).toEqual([]);
        expect(view(after)).toEqual(["B", "C"]);
        expect(after.body).toBe(s.body);
        expect(after.bodySeq).toBe(2);
        expect(after.meta).toEqual({ truncated: true, total_bytes: 50 });
        expect(after.metaSeq).toBe(2);
      }
    });

    it("C3: adopts truncated and the larger bound from an empty poll once a body was seen", () => {
      const s = run([...poll(1, ["A"]), pollStarted(2), pollEmpty(2, { truncated: true, total_bytes: 999 })]);
      expect(view(s)).toEqual(["A"]);
      expect(s.meta).toEqual({ truncated: true, total_bytes: 999 });
      expect(s.metaSeq).toBe(2);
      // The body's own seq did not move: an older body would still be
      // accepted, and the newer truncation stays on top of it.
      expect(s.bodySeq).toBe(1);
      const late = run([pollStarted(3), pollStarted(4), pollEmpty(4, { truncated: true, total_bytes: 1000 }), pollDone(3, ["A", "B"])], s);
      expect(view(late)).toEqual(["A", "B"]);
      expect(late.meta).toEqual({ truncated: true, total_bytes: 1000 });
    });

    it("keeps a larger bound already known over a smaller one from the empty poll", () => {
      const s = run([...poll(1, ["A"]), pollStarted(2), pollEmpty(2, { truncated: true, total_bytes: 1 })]);
      expect(s.meta?.total_bytes).toBe(2);
    });

    it("records an empty-but-truncated first answer's metadata, with no body", () => {
      const s = run([pollStarted(1), pollEmpty(1, { truncated: true, total_bytes: 5 })]);
      expect(view(s)).toEqual([]);
      expect(s.body).toBeNull();
      expect(s.meta).toEqual({ truncated: true, total_bytes: 5 });
    });

    it("a newer non-empty response replaces both body and metadata", () => {
      const s = run([pollStarted(1), pollDone(1, ["A"], { truncated: true, total_bytes: 99 }), ...poll(2, ["A", "B"])]);
      expect(view(s)).toEqual(["A", "B"]);
      expect(s.meta).toEqual({ truncated: false, total_bytes: 4 });
      expect(s.body?.returned_bytes).toBe(4);
    });

    it("a failed poll frees its slot and lets pending tails settle", () => {
      let s = run([pollStarted(3), pollStarted(4), pollDone(4, ["D", "E"])], loaded());
      expect(s.pending.map((e) => e.seq)).toEqual([4]);
      s = run([pollFailed(3)], s);
      expect(s.pending).toEqual([]);
      expect(view(s)).toEqual(["A", "B", "C", "D", "E"]);
    });
  });

  describe("bounds and bookkeeping", () => {
    it("holds a tail only while an older request is in flight", () => {
      let s = run([loadStarted(2), ...poll(3, ["A"])]);
      expect(s.pending.map((e) => e.seq)).toEqual([3]);
      s = run([...poll(4, ["A", "B"])], s);
      expect(s.pending.map((e) => e.seq)).toEqual([3, 4]);
      s = run([loadFailed(2)], s);
      expect(s.pending).toEqual([]);
    });

    it("keeps one entry per run for identical bodies while a load is in flight, with the same result", () => {
      let s = run([loadStarted(3), ...poll(4, ["D", "E"]), ...poll(5, ["D", "E"]), ...poll(6, ["D", "E"])], loaded());
      expect(s.pending.map((e) => e.seq)).toEqual([4]);
      expect(view(s)).toEqual(["A", "B", "C", "D", "E"]);
      s = run([loadDone(3, ["A", "B", "C", "D"])], s);
      expect(view(s)).toEqual(["A", "B", "C", "D", "E"]);
      // A different body is a new entry again.
      s = run([loadStarted(7), ...poll(8, ["D", "E"]), ...poll(9, ["E", "F"])], s);
      expect(s.pending.map((e) => e.seq)).toEqual([8, 9]);
    });

    it("keeps identical bodies apart when an in-flight request separates them", () => {
      // A hung poll (5) sits between the two identical polls: it could
      // still land between them, so both stay.
      let s = run([loadStarted(3), ...poll(4, ["D", "E"]), pollStarted(5), ...poll(6, ["D", "E"])], loaded());
      expect(s.pending.map((e) => e.seq)).toEqual([4, 6]);
      s = run([loadDone(3, ["A", "B"])], s);
      expect(view(s)).toEqual(["A", "B", GAP, "D", "E"]);
      s = run([pollFailed(5)], s);
      expect(s.pending).toEqual([]);
      expect(view(s)).toEqual(["A", "B", GAP, "D", "E"]);
    });

    it("a poll that starts and resolves alone is never buffered, and base tracks full", () => {
      const s = run([...poll(3, ["D", "E"]), ...poll(4, ["E", "F"])], loaded());
      expect(s.pending).toEqual([]);
      expect(s.base).toBe(s.full);
      expect(view(s)).toEqual(["A", "B", "C", "D", "E", "F"]);
    });

    it("a completion for a seq that never started is ignored", () => {
      const s = loaded();
      expect(reduce(s, pollDone(9, ["Z"]))).toBe(s);
      expect(reduce(s, loadDone(9, ["Z"]))).toBe(s);
      expect(reduce(s, loadFailed(9))).toBe(s);
      expect(reduce(s, pollFailed(9))).toBe(s);
    });

    it("does not mutate the previous state", () => {
      const s = loaded();
      const snapshot = structuredClone({ ...s, inFlightPolls: [...s.inFlightPolls], inFlightLoads: [...s.inFlightLoads] });
      run([pollStarted(3), loadStarted(4), pollDone(3, ["D", "E"]), loadDone(4, ["A"])], s);
      expect({ ...s, inFlightPolls: [...s.inFlightPolls], inFlightLoads: [...s.inFlightLoads] }).toEqual(snapshot);
    });
  });

  describe("fullStateOf", () => {
    it("walks idle → loading → loaded, and loading → error", () => {
      expect(fullStateOf(initialState())).toBe("idle");
      const loading = run([loadStarted(1)]);
      expect(fullStateOf(loading)).toBe("loading");
      expect(fullStateOf(run([loadDone(1, ["A"])], loading))).toBe("loaded");
      expect(fullStateOf(run([loadFailed(1)], loading))).toBe("error");
    });

    it("a reload shows loading over a loaded view and error after it fails, then loaded after a later success", () => {
      let s = run([loadStarted(3)], loaded());
      expect(fullStateOf(s)).toBe("loading");
      s = run([loadFailed(3)], s);
      expect(fullStateOf(s)).toBe("error");
      expect(view(s)).toEqual(["A", "B", "C", "D"]);
      s = run([loadStarted(4), loadDone(4, ["X"])], s);
      expect(fullStateOf(s)).toBe("loaded");
      expect(view(s)).toEqual(["X"]);
    });
  });
});
