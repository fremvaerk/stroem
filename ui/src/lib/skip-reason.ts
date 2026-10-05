import type { SkipReason } from "./types";

/**
 * Short badge text for a skipped step. A `null` reason (rows written before
 * migration 046) falls back to the old `when`-based heuristic.
 */
export function skipBadgeLabel(reason: SkipReason | null, hasWhen: boolean): string | null {
  switch (reason) {
    case "condition":
      return "condition";
    case "empty":
      return "empty loop";
    case "cascade":
      return "upstream skipped";
    case "unreachable":
      return "not satisfied";
    default:
      return hasWhen ? "condition" : null;
  }
}

/** One-sentence explanation for the step detail panel. */
export function skipExplanation(reason: SkipReason | null): string {
  switch (reason) {
    case "condition":
      return "Skipped: the step's when condition was false.";
    case "empty":
      return "Skipped: for_each produced no items.";
    case "cascade":
      return "Skipped (pre-0.18 only): a dependency was itself skipped, and that dependency's own lack of continue_when_skipped kept this step from running. New rows no longer use this reason — see the 0.18 upgrade guide.";
    case "unreachable":
      return "Skipped: a dependency's outcome did not satisfy this step's own depends_on condition, or the row has no recorded reason (jobs from before 0.16.2).";
    default:
      return "Skipped.";
  }
}
