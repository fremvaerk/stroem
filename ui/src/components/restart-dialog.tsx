import { TriangleAlert } from "lucide-react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import type { RestartPlanResponse } from "@/lib/api";

interface RestartDialogProps {
  open: boolean;
  /** Dry-run plan from the server. `null` while it is still being fetched. */
  plan: RestartPlanResponse | null;
  taskName: string;
  stepName: string;
  /** True while the real restart request is in flight. */
  busy?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}

/** The server returns `restart_steps` from a set — sort for a stable listing. */
function sorted(names: string[]): string[] {
  return [...names].sort();
}

export function RestartDialog({
  open,
  plan,
  taskName,
  stepName,
  busy = false,
  onConfirm,
  onCancel,
}: RestartDialogProps) {
  if (!plan) return null;

  const restartSteps = sorted(plan.restart_steps);
  const carriedFailed = sorted(plan.carried_failed);

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next && !busy) onCancel();
      }}
    >
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>{`Restart ${taskName} from ${stepName}?`}</DialogTitle>
          <DialogDescription>
            {`Reruns ${restartSteps.length} step(s): ${restartSteps.join(", ")}. ` +
              `${plan.carried_over.length} step(s) are carried over unchanged.`}
          </DialogDescription>
        </DialogHeader>

        {carriedFailed.length > 0 && (
          <div
            data-testid="restart-failure-warning"
            className="flex items-start gap-2 rounded-md border border-yellow-300 bg-yellow-50 px-3 py-2 dark:border-yellow-700 dark:bg-yellow-950"
          >
            <TriangleAlert
              aria-hidden="true"
              className="mt-0.5 h-4 w-4 shrink-0 text-yellow-600 dark:text-yellow-400"
            />
            <p className="text-sm text-yellow-800 dark:text-yellow-200">
              {`${carriedFailed.length} carried-over step(s) ended failed and are not ` +
                `tolerated by the current flow (${carriedFailed.join(", ")}) — the new ` +
                `job will end failed. Restart from an earlier step to rerun them.`}
            </p>
          </div>
        )}

        <DialogFooter>
          <Button variant="outline" disabled={busy} onClick={onCancel}>
            Cancel
          </Button>
          <Button disabled={busy} onClick={onConfirm}>
            {busy ? "Restarting..." : "Restart"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
