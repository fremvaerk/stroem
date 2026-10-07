import { describe, expect, it } from "vitest";
import { formatIsoDate, parseIsoDate } from "../iso-date";

describe("parseIsoDate", () => {
  it("reads YYYY-MM-DD as local midnight", () => {
    const date = parseIsoDate("2026-10-07");
    expect(date).toEqual(new Date(2026, 9, 7));
  });

  it("accepts single-digit month and day", () => {
    expect(parseIsoDate("2026-1-5")).toEqual(new Date(2026, 0, 5));
  });

  it("ignores whitespace around a pasted value", () => {
    expect(parseIsoDate(" 2026-10-07 \n")).toEqual(new Date(2026, 9, 7));
  });

  it("reads a four-digit year literally, not as 19xx", () => {
    expect(parseIsoDate("0026-01-01")?.getFullYear()).toBe(26);
  });

  it("accepts Feb 29 only in a leap year", () => {
    expect(parseIsoDate("2024-02-29")).toEqual(new Date(2024, 1, 29));
    expect(parseIsoDate("2026-02-29")).toBeUndefined();
  });

  it.each([
    ["impossible day", "2026-02-31"],
    ["month 13", "2026-13-01"],
    ["month 0", "2026-00-10"],
    ["day 0", "2026-10-00"],
  ])("rejects an %s instead of rolling it over (%s)", (_, text) => {
    expect(parseIsoDate(text)).toBeUndefined();
  });

  it.each([
    ["empty", ""],
    ["blank", "   "],
    ["not a date", "garbage"],
    ["partial", "2026-10"],
    // date-fns `parse` alone would read this as year 26 AD.
    ["two-digit year", "26-10-07"],
    ["day-first", "07.10.2026"],
    ["slashes", "2026/10/07"],
    ["three-digit month", "2026-010-07"],
    ["trailing junk", "2026-10-07x"],
    // datetime fields carry the time in their own box.
    ["datetime", "2026-10-07T14:30"],
  ])("rejects %s input (%j)", (_, text) => {
    expect(parseIsoDate(text)).toBeUndefined();
  });
});

describe("formatIsoDate", () => {
  it("formats a Date as YYYY-MM-DD with zero padding", () => {
    expect(formatIsoDate(new Date(2026, 0, 5))).toBe("2026-01-05");
    expect(formatIsoDate(parseIsoDate(" 2026-1-5 ")!)).toBe("2026-01-05");
  });
});
