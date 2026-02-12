import {
  CheckCircle2,
  Circle,
  Loader2,
  XCircle,
  Clock,
} from "lucide-react";
import type { JobStep } from "@/lib/types";

const statusIcon: Record<string, React.ReactNode> = {
  pending: <Clock className="h-4 w-4 text-muted-foreground" />,
  ready: <Circle className="h-4 w-4 text-blue-500" />,
  running: <Loader2 className="h-4 w-4 animate-spin text-blue-500" />,
  completed: <CheckCircle2 className="h-4 w-4 text-green-500" />,
  failed: <XCircle className="h-4 w-4 text-red-500" />,
};

function formatDuration(start: string | null, end: string | null): string {
  if (!start) return "";
  const s = new Date(start).getTime();
  const e = end ? new Date(end).getTime() : Date.now();
  const diff = Math.max(0, e - s);
  const seconds = Math.floor(diff / 1000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const secs = seconds % 60;
  return `${minutes}m ${secs}s`;
}

export function StepTimeline({ steps }: { steps: JobStep[] }) {
  return (
    <div className="space-y-0">
      {steps.map((step, index) => (
        <div key={step.step_name} className="flex gap-3">
          <div className="flex flex-col items-center">
            <div className="flex h-6 w-6 items-center justify-center">
              {statusIcon[step.status] ?? (
                <Circle className="h-4 w-4 text-muted-foreground" />
              )}
            </div>
            {index < steps.length - 1 && (
              <div className="w-px flex-1 bg-border" />
            )}
          </div>
          <div className="flex-1 pb-4">
            <div className="flex items-center gap-2">
              <span className="font-mono text-sm font-medium">
                {step.step_name}
              </span>
              <span className="text-xs text-muted-foreground">
                {step.action_name}
              </span>
              {(step.started_at || step.completed_at) && (
                <span className="ml-auto font-mono text-xs text-muted-foreground">
                  {formatDuration(step.started_at, step.completed_at)}
                </span>
              )}
            </div>
            {step.error_message && (
              <p className="mt-1 rounded-md bg-red-50 px-2 py-1 text-xs text-red-700 dark:bg-red-900/20 dark:text-red-400">
                {step.error_message}
              </p>
            )}
          </div>
        </div>
      ))}
    </div>
  );
}
