import { describe, it, expect } from "vitest";
import { estimateLineRows, formatBytesIEC, splitLogLines } from "../log-lines";

describe("log line helpers", () => {
  it("splits a JSONL body into non-empty lines", () => {
    expect(splitLogLines("")).toEqual([]);
    expect(splitLogLines("a\n\nb\n")).toEqual(["a", "b"]);
    expect(splitLogLines("a\nb")).toEqual(["a", "b"]);
  });

  it("estimates wrapped rows, discounting the JSON envelope", () => {
    expect(estimateLineRows("short", 120)).toBe(1);
    expect(estimateLineRows("x".repeat(250), 100)).toBe(3);
    expect(estimateLineRows(`{"ts":"t","stream":"stdout","step":"s","line":"${"y".repeat(150)}"}`, 100)).toBe(2);
    expect(estimateLineRows("", 100)).toBe(1);
  });

  it("formats IEC sizes with one decimal", () => {
    expect(formatBytesIEC(512)).toBe("512 B");
    expect(formatBytesIEC(262_144)).toBe("256.0 KiB");
    expect(formatBytesIEC(87_325_871)).toBe("83.3 MiB");
  });
});
