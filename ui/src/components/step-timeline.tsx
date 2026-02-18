import {
  AlertCircle,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  Circle,
  Loader2,
  SkipForward,
  XCircle,
  Clock,
} from "lucide-react";
import { StepDetail } from "@/components/step-detail";
import type { JobStep } from "@/lib/types";

const statusIcon: Record<string, React.ReactNode> = {
  pending: <Clock className="h-4 w-4 text-muted-foreground" />,
  ready: <Circle className="h-4 w-4 text-blue-500" />,
  running: <Loader2 className="h-4 w-4 animate-spin text-blue-500" />,
  completed: <CheckCircle2 className="h-4 w-4 text-green-500" />,
  failed: <XCircle className="h-4 w-4 text-red-500" />,
  skipped: <SkipForward className="h-4 w-4 text-muted-foreground" />,
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

interface StepTimelineProps {
  jobId: string;
  steps: JobStep[];
  selectedStep: string | null;
  onSelectStep: (stepName: string | null) => void;
  workerNames?: Map<string, string>;
}

export function StepTimeline({
  jobId,
  steps,
  selectedStep,
  onSelectStep,
  workerNames,
}: StepTimelineProps) {
  return (
    <div className="space-y-0">
      {steps.map((step, index) => {
        const isExpanded = selectedStep === step.step_name;
        return (
          <div key={step.step_name}>
            <button
              type="button"
              className="flex w-full gap-3 text-left hover:bg-muted/50 rounded-md px-1 -mx-1 transition-colors"
              onClick={() =>
                onSelectStep(isExpanded ? null : step.step_name)
              }
            >
              <div className="flex flex-col items-center">
                <div className="flex h-6 w-6 items-center justify-center">
                  {statusIcon[step.status] ?? (
                    <Circle className="h-4 w-4 text-muted-foreground" />
                  )}
                </div>
                {(index < steps.length - 1 || isExpanded) && (
                  <div className="w-px flex-1 bg-border" />
                )}
              </div>
              <div className="flex-1 pb-4">
                <div className="flex items-center gap-2">
                  {isExpanded ? (
                    <ChevronDown className="h-3.5 w-3.5 text-muted-foreground" />
                  ) : (
                    <ChevronRight className="h-3.5 w-3.5 text-muted-foreground" />
                  )}
                  <span className="font-mono text-sm font-medium">
                    {step.step_name}
                  </span>
                  <span className="text-xs text-muted-foreground">
                    {step.action_name}
                  </span>
                  {step.worker_id && workerNames && (
                    <span className="text-xs text-muted-foreground/60">
                      {workerNames.get(step.worker_id) ?? step.worker_id.substring(0, 8)}
                    </span>
                  )}
                  {(step.started_at || step.completed_at) && (
                    <span className="ml-auto font-mono text-xs text-muted-foreground">
                      {formatDuration(step.started_at, step.completed_at)}
                    </span>
                  )}
                </div>
                {step.error_message && (
                  <div className="mt-1 ml-5.5 flex items-start gap-1.5 rounded-md bg-red-50 px-2 py-1.5 dark:bg-red-900/20">
                    <AlertCircle className="mt-0.5 h-3.5 w-3.5 shrink-0 text-red-500" />
                    <pre className="whitespace-pre-wrap break-all text-xs text-red-700 dark:text-red-400">
                      {step.error_message}
                    </pre>
                  </div>
                )}
              </div>
            </button>
            {isExpanded && (
              <div className="ml-9 mb-4">
                <StepDetail jobId={jobId} step={step} />
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}
