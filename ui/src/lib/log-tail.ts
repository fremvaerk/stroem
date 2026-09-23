import { LOG_GAP_MARKER } from "./log-lines";

export interface AppendResult {
  lines: string[];
  gap: boolean;
}

/**
 * Stitch a freshly polled tail onto the lines the viewer shows in full mode
 * (spec § 3.6): remove from `displayed`, newest first, as many copies of
 * each line as the tail holds, then append the tail. Nothing about time —
 * a file window is not a time range. Idempotent, never duplicates under an
 * arrival-ordered tail, keeps the multiplicity observed so far. When no
 * tail line was displayed, a gap marker separates the view from the tail.
 */
export function appendTail(displayed: readonly string[], tail: readonly string[]): AppendResult {
  if (tail.length === 0) return { lines: [...displayed], gap: false };
  const counts = new Map<string, number>();
  for (const line of tail) counts.set(line, (counts.get(line) ?? 0) + 1);
  const keep = new Array<boolean>(displayed.length).fill(true);
  let remaining = tail.length;
  let removed = 0;
  for (let i = displayed.length - 1; i >= 0 && remaining > 0; i--) {
    const count = counts.get(displayed[i]);
    if (count) {
      counts.set(displayed[i], count - 1);
      keep[i] = false;
      removed += 1;
      remaining -= 1;
    }
  }
  const kept = displayed.filter((_, i) => keep[i]);
  const gap = displayed.length > 0 && removed === 0;
  return { lines: gap ? kept.concat([LOG_GAP_MARKER], tail) : kept.concat(tail), gap };
}
