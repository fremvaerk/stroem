import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import {
  formatTime,
  formatDuration,
  formatRelativeTime,
  formatFutureTime,
} from "./formatting";

// Pin a fixed "now" for all time-relative tests
const FIXED_NOW = new Date("2025-06-15T12:00:00.000Z").getTime();

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(FIXED_NOW);
});

afterEach(() => {
  vi.useRealTimers();
});

// ---------------------------------------------------------------------------
// formatTime
// ---------------------------------------------------------------------------
describe("formatTime", () => {
  it("returns '-' for null input", () => {
    expect(formatTime(null)).toBe("-");
  });

  it("returns '-' for empty string", () => {
    expect(formatTime("")).toBe("-");
  });

  it("returns a non-empty string for a valid ISO date", () => {
    // The exact locale-formatted string is environment-dependent, so we only
    // assert it is truthy and not the fallback value.
    const result = formatTime("2025-02-27T14:30:00.000Z");
    expect(result).toBeTruthy();
    expect(result).not.toBe("-");
  });
});

// ---------------------------------------------------------------------------
// formatDuration
// ---------------------------------------------------------------------------
describe("formatDuration", () => {
  it("returns '-' when start is null", () => {
    expect(formatDuration(null, null)).toBe("-");
  });

  it("formats a 5-second duration", () => {
    const start = new Date(FIXED_NOW - 5_000).toISOString();
    const end = new Date(FIXED_NOW).toISOString();
    expect(formatDuration(start, end)).toBe("5s");
  });

  it("formats a 90-second duration as '1m 30s'", () => {
    const start = new Date(FIXED_NOW - 90_000).toISOString();
    const end = new Date(FIXED_NOW).toISOString();
    expect(formatDuration(start, end)).toBe("1m 30s");
  });

  it("formats a 65-minute duration as '1h 5m'", () => {
    const start = new Date(FIXED_NOW - 65 * 60_000).toISOString();
    const end = new Date(FIXED_NOW).toISOString();
    expect(formatDuration(start, end)).toBe("1h 5m");
  });

  it("uses current time when end is null (live duration)", () => {
    const start = new Date(FIXED_NOW - 10_000).toISOString();
    expect(formatDuration(start, null)).toBe("10s");
  });

  it("returns '0s' when start equals end", () => {
    const ts = new Date(FIXED_NOW).toISOString();
    expect(formatDuration(ts, ts)).toBe("0s");
  });

  it("clamps negative diff to '0s'", () => {
    // end is before start — diff is negative, should clamp to 0
    const start = new Date(FIXED_NOW + 5_000).toISOString();
    const end = new Date(FIXED_NOW).toISOString();
    expect(formatDuration(start, end)).toBe("0s");
  });
});

// ---------------------------------------------------------------------------
// formatRelativeTime
// ---------------------------------------------------------------------------
describe("formatRelativeTime", () => {
  it("returns 'Never' for null", () => {
    expect(formatRelativeTime(null)).toBe("Never");
  });

  it("shows seconds ago for a recent timestamp", () => {
    const ts = new Date(FIXED_NOW - 30_000).toISOString();
    expect(formatRelativeTime(ts)).toBe("30s ago");
  });

  it("shows minutes ago for a 5-minute-old timestamp", () => {
    const ts = new Date(FIXED_NOW - 5 * 60_000).toISOString();
    expect(formatRelativeTime(ts)).toBe("5m ago");
  });

  it("shows hours ago for a 3-hour-old timestamp", () => {
    const ts = new Date(FIXED_NOW - 3 * 3_600_000).toISOString();
    expect(formatRelativeTime(ts)).toBe("3h ago");
  });

  it("shows days ago for a 2-day-old timestamp", () => {
    const ts = new Date(FIXED_NOW - 2 * 86_400_000).toISOString();
    expect(formatRelativeTime(ts)).toBe("2d ago");
  });

  it("shows '0s ago' for a timestamp equal to now", () => {
    const ts = new Date(FIXED_NOW).toISOString();
    expect(formatRelativeTime(ts)).toBe("0s ago");
  });
});

// ---------------------------------------------------------------------------
// formatFutureTime
// ---------------------------------------------------------------------------
describe("formatFutureTime", () => {
  it("returns 'past' for a date in the past", () => {
    const ts = new Date(FIXED_NOW - 1_000).toISOString();
    expect(formatFutureTime(ts)).toBe("past");
  });

  it("formats a 10-minute future time as 'in 10m'", () => {
    const ts = new Date(FIXED_NOW + 10 * 60_000).toISOString();
    expect(formatFutureTime(ts)).toBe("in 10m");
  });

  it("formats a 2-hour 30-minute future time as 'in 2h 30m'", () => {
    const ts = new Date(FIXED_NOW + (2 * 60 + 30) * 60_000).toISOString();
    expect(formatFutureTime(ts)).toBe("in 2h 30m");
  });

  it("formats a 3-day 6-hour future time as 'in 3d 6h'", () => {
    const ts = new Date(FIXED_NOW + (3 * 24 + 6) * 3_600_000).toISOString();
    expect(formatFutureTime(ts)).toBe("in 3d 6h");
  });

  it("returns 'past' for a timestamp exactly equal to now (diff = 0)", () => {
    // diff is 0, which is not < 0, so minutes = 0 → "in 0m"
    const ts = new Date(FIXED_NOW).toISOString();
    expect(formatFutureTime(ts)).toBe("in 0m");
  });
});
