import { describe, it, expect } from "vitest";
import { fileFieldBlocksRun, initialFileFieldState, isFileFieldState } from "../file-field";

describe("file-field", () => {
  it("recognises only file states", () => {
    expect(isFileFieldState({ kind: "file", mode: "empty" })).toBe(true);
    expect(isFileFieldState({ kind: "json", mode: "value", text: "" })).toBe(false);
    expect(isFileFieldState("file_x")).toBe(false);
    expect(isFileFieldState(null)).toBe(false);
  });

  it("starts empty without a source", () => {
    expect(initialFileFieldState()).toEqual({ kind: "file", mode: "empty" });
  });

  it("starts ready + fromSource with a source", () => {
    expect(
      initialFileFieldState({ field: "f", file_id: "file_a", name: "a.csv", size_bytes: 7, content_type: "text/csv" }),
    ).toEqual({ kind: "file", mode: "ready", fileId: "file_a", name: "a.csv", size: 7, fromSource: true });
  });

  it("blocks Run while uploading or failed only", () => {
    expect(fileFieldBlocksRun({ kind: "file", mode: "uploading", name: "a", size: 1, progress: 0.5 })).toBe(true);
    expect(fileFieldBlocksRun({ kind: "file", mode: "failed", name: "a", error: "x" })).toBe(true);
    expect(fileFieldBlocksRun({ kind: "file", mode: "empty" })).toBe(false);
    expect(
      fileFieldBlocksRun({ kind: "file", mode: "ready", fileId: "f", name: "a", size: 1, fromSource: false }),
    ).toBe(false);
    expect(fileFieldBlocksRun("")).toBe(false);
    expect(fileFieldBlocksRun(undefined)).toBe(false);
  });
});
