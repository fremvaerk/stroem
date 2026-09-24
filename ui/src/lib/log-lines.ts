/** A row the viewer renders as a "lines missing" divider. NUL never
 * appears in JSONL, so it cannot collide with a real line. */
export const LOG_GAP_MARKER = String.fromCharCode(0) + "stroem-gap";

/** Split a JSONL body into its non-empty lines. */
export function splitLogLines(body: string): string[] {
  if (!body) return [];
  return body.split("\n").filter((line) => line.length > 0);
}

/** Bytes of JSON around the text of a typical line
 * (`{"ts":…,"stream":…,"step":…,"line":…}`). */
const JSON_OVERHEAD = 70;

/** Estimated wrapped rows of a raw line; the virtualiser measures the
 * real height once the row renders. */
export function estimateLineRows(raw: string, charsPerRow: number): number {
  const visible = raw.startsWith("{") ? raw.length - JSON_OVERHEAD : raw.length;
  return Math.max(1, Math.ceil(Math.max(visible, 1) / Math.max(charsPerRow, 1)));
}

/** `262144` → `256.0 KiB` (IEC units, one decimal). */
export function formatBytesIEC(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KiB", "MiB", "GiB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(1)} ${units[unit]}`;
}
