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
      return "upstream failed";
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
      return "Skipped: every dependency was skipped.";
    case "unreachable":
      return "Skipped: an upstream step failed or was cancelled.";
    default:
      return "Skipped.";
  }
}
