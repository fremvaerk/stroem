import { Link } from "react-router";
import { AlertTriangle } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { StatusBadge } from "@/components/status-badge";
import { formatTime } from "@/lib/formatting";
import { missedLabel, triggersWithMissed } from "@/lib/trigger-status";
import type { MissedRuns, TriggerInfo } from "@/lib/types";

export function LastRunCell({ trigger }: { trigger: TriggerInfo }) {
  const run = trigger.last_run;
  if (run === undefined && trigger.last_run_hidden === undefined) {
    // An older API (rolling deploy) sends no status data.
    return <span className="text-xs text-muted-foreground">—</span>;
  }
  if (!run) {
    return (
      <span className="text-xs text-muted-foreground">
        {trigger.last_run_hidden ? "no access" : "never"}
      </span>
    );
  }
  const link = (
    <Link to={`/jobs/${run.job_id}`} className="text-xs hover:underline">
      {formatTime(run.created_at)}
    </Link>
  );
  if (trigger.type === "event_source" && run.status === "running") {
    return (
      <span className="flex items-center gap-1.5">
        <StatusBadge status="running" />
        <span className="text-xs text-muted-foreground">running since</span>
        {link}
      </span>
    );
  }
  return (
    <span className="flex items-center gap-1.5">
      <StatusBadge status={run.status} />
      {link}
    </span>
  );
}

export function MissedRunsPill({ missed }: { missed: MissedRuns }) {
  return (
    <Badge
      variant="secondary"
      className="bg-amber-100 text-xs text-amber-800 dark:bg-amber-900/30 dark:text-amber-400"
    >
      {missedLabel(missed)}
    </Badge>
  );
}

export function MissedRunsWarning({ triggers }: { triggers: TriggerInfo[] }) {
  const missed = triggersWithMissed(triggers);
  if (missed.length === 0) return null;
  const n = missed.length;
  return (
    <div
      role="alert"
      className="mb-3 flex items-start gap-2 rounded-md border border-amber-300 bg-amber-50 px-3 py-2 text-sm text-amber-900 dark:border-amber-800 dark:bg-amber-950/30 dark:text-amber-200"
    >
      <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
      <p>
        {`${n} ${n === 1 ? "trigger" : "triggers"} missed scheduled runs — ${missed
          .map((t) => t.name)
          .join(", ")}. Run the tasks manually if you need the data. The mark stays until each trigger's next scheduled run.`}
      </p>
    </div>
  );
}
