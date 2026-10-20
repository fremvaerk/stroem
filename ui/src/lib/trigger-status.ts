import { formatTime } from "@/lib/formatting";
import type { MissedRuns, TriggerInfo } from "@/lib/types";

export function missedLabel(m: MissedRuns): string {
  return `${m.capped ? "1000+" : m.count} missed · ${formatTime(m.first)}`;
}

export function triggersWithMissed<T extends TriggerInfo>(ts: T[]): T[] {
  return ts.filter((t) => t.missed);
}
