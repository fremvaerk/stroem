import { format, isValid, parse } from "date-fns";

/** The wire format of a `date` input, and the only format the UI accepts. */
export const ISO_DATE_FORMAT = "yyyy-MM-dd";

// Year first, dashes only; month and day may drop the leading zero.
const ISO_DATE_SHAPE = /^\d{4}-\d{1,2}-\d{1,2}$/;

/**
 * Reads a typed or pasted date. Returns the local-midnight `Date`, or
 * `undefined` when `text` is not a real calendar date.
 */
export function parseIsoDate(text: string): Date | undefined {
  const trimmed = text.trim();
  // The shape check matters: `parse` alone accepts a 1–3 digit year.
  if (!ISO_DATE_SHAPE.test(trimmed)) return undefined;
  // `parse` validates the day against the month: 2026-02-31 is Invalid Date,
  // never rolled over into March.
  const date = parse(trimmed, "yyyy-M-d", new Date());
  return isValid(date) ? date : undefined;
}

export function formatIsoDate(date: Date): string {
  return format(date, ISO_DATE_FORMAT);
}
