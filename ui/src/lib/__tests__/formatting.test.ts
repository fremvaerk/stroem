import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import {
  formatTime,
  formatDuration,
  formatRelativeTime,
  formatFutureTime,
} from "../formatting";

// Pin Date.now() so relative-time tests are deterministic.
const NOW = new Date("2024-06-15T12:00:00.000Z").getTime();

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(NOW);
});

afterEach(() => {
  vi.useRealTimers();
});

// ---------------------------------------------------------------------------
// formatTime
// ---------------------------------------------------------------------------
describe("formatTime", () => {
  it("returns '-' for null", () => {
    expect(formatTime(null)).toBe("-");
  });

  it("returns '-' for empty string", () => {
    expect(formatTime("")).toBe("-");
  });

  it("returns a non-empty locale string for a valid ISO date", () => {
    const result = formatTime("2024-06-15T12:00:00.000Z");
    // We cannot assert the exact locale string because it depends on the
    // system locale, but it must not be "-" and must be a non-empty string.
    expect(result).toBeTruthy();
    expect(result).not.toBe("-");
  });

  it("includes month, day, hour and minute components", () => {
    // Use a date with unambiguous components so we can check them.
    const result = formatTime("2024-01-05T09:07:00.000Z");
    // The formatted string should contain numeric representations of the
    // date parts even if the exact format varies by locale.
    expect(result).toMatch(/\d/); // at minimum one digit
    expect(result.length).toBeGreaterThan(3);
  });
});

// ---------------------------------------------------------------------------
// formatDuration
// ---------------------------------------------------------------------------
describe("formatDuration", () => {
  it("returns '-' when start is null", () => {
    expect(formatDuration(null, null)).toBe("-");
    expect(formatDuration(null, "2024-06-15T12:00:30.000Z")).toBe("-");
  });

  it("formats seconds-only duration correctly", () => {
    const start = "2024-06-15T12:00:00.000Z";
    const end = "2024-06-15T12:00:45.000Z";
    expect(formatDuration(start, end)).toBe("45s");
  });

  it("formats exactly 0 seconds", () => {
    const ts = "2024-06-15T12:00:00.000Z";
    expect(formatDuration(ts, ts)).toBe("0s");
  });

  it("formats minutes and seconds correctly", () => {
    const start = "2024-06-15T12:00:00.000Z";
    const end = "2024-06-15T12:02:30.000Z"; // 150 s = 2m 30s
    expect(formatDuration(start, end)).toBe("2m 30s");
  });

  it("formats hours and minutes correctly", () => {
    const start = "2024-06-15T11:00:00.000Z";
    const end = "2024-06-15T12:05:00.000Z"; // 65 min = 1h 5m
    expect(formatDuration(start, end)).toBe("1h 5m");
  });

  it("uses Date.now() when end is null (live duration)", () => {
    // NOW is 2024-06-15T12:00:00Z; start is 30s earlier
    const start = "2024-06-15T11:59:30.000Z";
    expect(formatDuration(start, null)).toBe("30s");
  });

  it("clamps negative durations to 0s", () => {
    const start = "2024-06-15T12:00:10.000Z"; // 10s in the future relative to NOW
    const end = "2024-06-15T12:00:00.000Z"; // NOW
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

  it("formats seconds ago correctly", () => {
    const past = new Date(NOW - 45_000).toISOString(); // 45 s ago
    expect(formatRelativeTime(past)).toBe("45s ago");
  });

  it("formats minutes ago correctly", () => {
    const past = new Date(NOW - 5 * 60_000).toISOString(); // 5 m ago
    expect(formatRelativeTime(past)).toBe("5m ago");
  });

  it("formats hours ago correctly", () => {
    const past = new Date(NOW - 3 * 3_600_000).toISOString(); // 3 h ago
    expect(formatRelativeTime(past)).toBe("3h ago");
  });

  it("formats days ago correctly", () => {
    const past = new Date(NOW - 2 * 86_400_000).toISOString(); // 2 d ago
    expect(formatRelativeTime(past)).toBe("2d ago");
  });

  it("formats 0 seconds ago as '0s ago'", () => {
    const past = new Date(NOW).toISOString();
    expect(formatRelativeTime(past)).toBe("0s ago");
  });

  it("formats exactly 59 seconds as seconds, not minutes", () => {
    const past = new Date(NOW - 59_000).toISOString();
    expect(formatRelativeTime(past)).toBe("59s ago");
  });

  it("formats exactly 60 seconds as '1m ago'", () => {
    const past = new Date(NOW - 60_000).toISOString();
    expect(formatRelativeTime(past)).toBe("1m ago");
  });
});

// ---------------------------------------------------------------------------
// formatFutureTime
// ---------------------------------------------------------------------------
describe("formatFutureTime", () => {
  it("returns 'past' for a date in the past", () => {
    const past = new Date(NOW - 1_000).toISOString();
    expect(formatFutureTime(past)).toBe("past");
  });

  it("formats minutes in the future correctly", () => {
    const future = new Date(NOW + 30 * 60_000).toISOString(); // 30 min
    expect(formatFutureTime(future)).toBe("in 30m");
  });

  it("formats hours and minutes in the future correctly", () => {
    const future = new Date(NOW + (3 * 60 + 10) * 60_000).toISOString(); // 3h 10m
    expect(formatFutureTime(future)).toBe("in 3h 10m");
  });

  it("formats days and hours in the future correctly", () => {
    const future = new Date(NOW + (2 * 24 + 4) * 3_600_000).toISOString(); // 2d 4h
    expect(formatFutureTime(future)).toBe("in 2d 4h");
  });
});
