import type { JobStep } from "./types";

/** `@ release/2.3 · 3f2a9c0` — null when the job or step is not pinned. */
export function formatPin(
  ref: string | null | undefined,
  commit: string | null | undefined,
): string | null {
  if (!ref) return null;
  return commit ? `@ ${ref} · ${commit.substring(0, 7)}` : `@ ${ref}`;
}

/** `ref: {{ … }}` — a step whose action waits for its templated ref to be resolved. */
export function deferredRefLabel(
  step: Pick<JobStep, "action_type" | "ref_template">,
): string | null {
  if (step.action_type !== "deferred" || !step.ref_template) return null;
  return `ref: ${step.ref_template}`;
}

/** The pin a step row carries: its action's ref, else its `type: task` task's ref. */
export function stepPin(
  step: Pick<JobStep, "action_ref" | "action_revision" | "task_ref" | "task_revision">,
): { ref: string; commit: string | null } | null {
  if (step.action_ref) return { ref: step.action_ref, commit: step.action_revision };
  if (step.task_ref) return { ref: step.task_ref, commit: step.task_revision };
  return null;
}
