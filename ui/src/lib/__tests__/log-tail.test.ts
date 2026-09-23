import { describe, it, expect } from "vitest";
import { appendTail } from "../log-tail";
import { LOG_GAP_MARKER } from "../log-lines";

const plain = (lines: string[]) => lines.filter((l) => l !== LOG_GAP_MARKER);

describe("appendTail", () => {
  it("appends only what the view lacks on a plain overlap", () => {
    expect(appendTail(["a", "b", "c"], ["b", "c", "d"])).toEqual({ lines: ["a", "b", "c", "d"], gap: false });
  });

  it("is idempotent", () => {
    const once = appendTail(["a", "b"], ["b", "c"]).lines;
    expect(appendTail(once, ["b", "c"]).lines).toEqual(once);
  });

  it("does not duplicate under an arrival-ordered tail", () => {
    expect(appendTail(["B", "A", "C"], ["B", "A", "C"]).lines).toEqual(["B", "A", "C"]);
  });

  it("shows a line another replica recovered in the middle", () => {
    expect(appendTail(["A", "C"], ["A", "B", "C"]).lines).toEqual(["A", "B", "C"]);
  });

  it("appends a stale straggler once", () => {
    const once = appendTail(["a", "b", "c"], ["c", "s"]).lines;
    expect(once).toEqual(["a", "b", "c", "s"]);
    expect(appendTail(once, ["c", "s"]).lines).toEqual(once);
  });

  it("marks a gap only when no tail line was displayed", () => {
    expect(appendTail(["a", "b"], ["x", "y"])).toEqual({ lines: ["a", "b", LOG_GAP_MARKER, "x", "y"], gap: true });
    expect(appendTail([], ["x"])).toEqual({ lines: ["x"], gap: false });
  });

  it("keeps observed multiplicity", () => {
    expect(appendTail(["X", "X"], ["X"]).lines).toEqual(["X", "X"]);
    expect(plain(appendTail(["A"], ["B", "B"]).lines)).toEqual(["A", "B", "B"]);
    expect(plain(appendTail(["A", "A"], ["B"]).lines)).toEqual(["A", "A", "B"]);
  });

  it("keeps kept lines in their order and keeps gap markers", () => {
    expect(appendTail(["a", "b", "c", "d"], ["b", "d"]).lines).toEqual(["a", "c", "b", "d"]);
    expect(appendTail(["a", LOG_GAP_MARKER, "b"], ["b", "c"]).lines).toEqual(["a", LOG_GAP_MARKER, "b", "c"]);
  });

  it("returns the view unchanged for an empty tail", () => {
    expect(appendTail(["a"], [])).toEqual({ lines: ["a"], gap: false });
  });
});
