import type { JobStep } from "@/lib/types";

const pad2 = (n: number) => String(n).padStart(2, "0");

/** Local wall-clock `HH:MM:SS`; "" for a missing or unparseable timestamp. */
export function formatClock(ts: string): string {
  const d = new Date(ts);
  if (!ts || Number.isNaN(d.getTime())) return "";
  return `${pad2(d.getHours())}:${pad2(d.getMinutes())}:${pad2(d.getSeconds())}`;
}

/**
 * `+MM:SS`, or `+H:MM:SS` from one hour on. Truncates toward zero, so
 * sub-second clock skew reads as `+00:00` rather than `-00:01`.
 */
export function formatElapsed(ms: number): string {
  const secs = Math.trunc(ms / 1000);
  const sign = secs < 0 ? "-" : "+";
  const abs = Math.abs(secs);
  const h = Math.floor(abs / 3600);
  const m = Math.floor((abs % 3600) / 60);
  const s = abs % 60;
  return h > 0 ? `${sign}${h}:${pad2(m)}:${pad2(s)}` : `${sign}${pad2(m)}:${pad2(s)}`;
}

/**
 * Start of every attempt of a step, ascending, in epoch ms. A retry resets
 * `started_at`, but the log keeps every attempt's lines, so the earlier
 * starts come from `retry_history`.
 *
 * Empty for an agent step: resuming it (after `ask_user` or a task tool)
 * re-claims the step, which moves `started_at` without a `retry_history`
 * entry, so its earlier lines have no anchor.
 */
export function attemptStarts(
  step: Pick<JobStep, "action_type" | "started_at" | "retry_history">,
): number[] {
  if (step.action_type === "agent") return [];
  const raw = [...(step.retry_history ?? []).map((a) => a.started_at), step.started_at];
  return raw
    .map((ts) => (ts ? Date.parse(ts) : NaN))
    .filter((t) => !Number.isNaN(t))
    .sort((a, b) => a - b);
}

/**
 * Elapsed time of a log line since the start of the attempt it belongs to:
 * the latest start at or before the line, else the first one (a worker clock
 * behind the server's stamps lines before `started_at`).
 */
export function elapsedLabel(ts: string, starts: readonly number[]): string {
  const t = ts ? Date.parse(ts) : NaN;
  if (starts.length === 0 || Number.isNaN(t)) return "";
  let anchor = starts[0];
  for (const s of starts) {
    if (s <= t) anchor = s;
    else break;
  }
  return formatElapsed(t - anchor);
}
