import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { attemptStarts, elapsedLabel, formatClock, formatElapsed } from "../log-time";

describe("formatClock", () => {
  // A non-UTC zone, so a UTC implementation fails here too (CI runs in UTC).
  beforeEach(() => vi.stubEnv("TZ", "Europe/Oslo"));
  afterEach(() => vi.unstubAllEnvs());

  it("shows local wall-clock time without milliseconds, in summer time", () => {
    expect(formatClock("2026-07-15T12:23:34.194Z")).toBe("14:23:34");
  });

  it("shows local wall-clock time in winter time", () => {
    expect(formatClock("2026-01-15T12:23:34.194Z")).toBe("13:23:34");
  });

  it("zero-pads every field", () => {
    expect(formatClock("2026-01-15T08:05:07Z")).toBe("09:05:07");
  });

  it("returns an empty string for a missing or unparseable timestamp", () => {
    expect(formatClock("")).toBe("");
    expect(formatClock("not a date")).toBe("");
  });
});

describe("formatElapsed", () => {
  it("shows minutes and seconds under an hour", () => {
    expect(formatElapsed(0)).toBe("+00:00");
    expect(formatElapsed(4_999)).toBe("+00:04");
    expect(formatElapsed((12 * 60 + 34) * 1000)).toBe("+12:34");
    expect(formatElapsed((59 * 60 + 59) * 1000)).toBe("+59:59");
  });

  it("adds hours from one hour on", () => {
    expect(formatElapsed(3600 * 1000)).toBe("+1:00:00");
    expect(formatElapsed((3600 + 2 * 60 + 3) * 1000)).toBe("+1:02:03");
    expect(formatElapsed(26 * 3600 * 1000)).toBe("+26:00:00");
  });

  it("keeps the sign of a line stamped before its start (clock skew)", () => {
    expect(formatElapsed(-1_500)).toBe("-00:01");
    expect(formatElapsed(-(3600 + 5) * 1000)).toBe("-1:00:05");
  });

  it("reads sub-second skew as zero", () => {
    expect(formatElapsed(-300)).toBe("+00:00");
  });
});

describe("attemptStarts", () => {
  it("is empty for a step that never started", () => {
    expect(attemptStarts({ action_type: "script", started_at: null, retry_history: [] })).toEqual([]);
  });

  it("holds the current attempt's start", () => {
    const start = "2026-09-25T12:00:00Z";
    expect(attemptStarts({ action_type: "script", started_at: start, retry_history: [] })).toEqual([Date.parse(start)]);
  });

  it("adds earlier attempts, ascending, skipping missing and invalid starts", () => {
    const first = "2026-09-25T12:00:00Z";
    const second = "2026-09-25T12:05:00Z";
    const third = "2026-09-25T12:10:00Z";
    const history = [
      { attempt: 1, error: "e", started_at: second, failed_at: null },
      { attempt: 0, error: "e", started_at: first, failed_at: null },
      { attempt: 2, error: "e", started_at: null, failed_at: null },
      { attempt: 3, error: "e", started_at: "garbage", failed_at: null },
    ];
    expect(attemptStarts({ action_type: "script", started_at: third, retry_history: history })).toEqual([
      Date.parse(first),
      Date.parse(second),
      Date.parse(third),
    ]);
  });

  it("keeps earlier attempts while the step waits for its next retry", () => {
    const first = "2026-09-25T12:00:00Z";
    const history = [{ attempt: 0, error: "e", started_at: first, failed_at: null }];
    expect(attemptStarts({ action_type: "script", started_at: null, retry_history: history })).toEqual([Date.parse(first)]);
  });

  it("is empty for an agent step, whose started_at moves on every resume", () => {
    // Resuming after ask_user or a task tool re-claims the step, which
    // overwrites started_at without a retry_history entry.
    const history = [{ attempt: 0, error: "e", started_at: "2026-09-25T12:00:00Z", failed_at: null }];
    expect(
      attemptStarts({ action_type: "agent", started_at: "2026-09-25T12:05:00Z", retry_history: history }),
    ).toEqual([]);
  });
});

describe("elapsedLabel", () => {
  const first = Date.parse("2026-09-25T12:00:00Z");
  const second = Date.parse("2026-09-25T12:10:00Z");

  it("measures a line from the start of the attempt it belongs to", () => {
    expect(elapsedLabel("2026-09-25T12:03:07.900Z", [first, second])).toBe("+03:07");
    expect(elapsedLabel("2026-09-25T12:10:04Z", [first, second])).toBe("+00:04");
  });

  it("counts a line stamped exactly at an attempt start from that attempt", () => {
    expect(elapsedLabel("2026-09-25T12:10:00Z", [first, second])).toBe("+00:00");
  });

  it("measures a line stamped before every start from the first one", () => {
    expect(elapsedLabel("2026-09-25T11:59:58Z", [first, second])).toBe("-00:02");
  });

  it("returns an empty string without starts or a parseable timestamp", () => {
    expect(elapsedLabel("2026-09-25T12:00:01Z", [])).toBe("");
    expect(elapsedLabel("", [first])).toBe("");
    expect(elapsedLabel("not a date", [first])).toBe("");
  });
});
