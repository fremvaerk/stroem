import { useEffect, useRef, useState } from "react";
import { Link } from "react-router";
import { RotateCcw } from "lucide-react";
import {
  Tabs,
  TabsContent,
  TabsList,
  TabsTrigger,
} from "@/components/ui/tabs";
import { Button } from "@/components/ui/button";
import { LogViewer } from "@/components/log-viewer";
import { JsonViewer } from "@/components/json-viewer";
import { ApprovalCard } from "@/components/approval-card";
import { getStepLogs } from "@/lib/api";
import { formatTime } from "@/lib/formatting";
import { isTerminalJobStatus } from "@/lib/job-status";
import type { JobStep } from "@/lib/types";

interface StepDetailProps {
  jobId: string;
  step: JobStep;
  onRefresh?: () => void;
  /** Status of the owning job — restart is only offered once it is terminal. */
  jobStatus?: string;
  /** Task-level `can_execute`; false hides the restart button. */
  canRestart?: boolean;
  /** Source job of a restart, used to link a carried-over step to its logs. */
  sourceJobId?: string | null;
  /** Opens the shared restart dialog for this step. Absent = no button. */
  onRestart?: (stepName: string) => void;
  /** True while the restart plan/creation for this step is in flight. */
  restartPending?: boolean;
}

export function StepDetail({
  jobId,
  step,
  onRefresh,
  jobStatus,
  canRestart = false,
  sourceJobId,
  onRestart,
  restartPending = false,
}: StepDetailProps) {
  const [logs, setLogs] = useState("");
  const [loadingLogs, setLoadingLogs] = useState(true);
  const hasLogsRef = useRef(false);
  const isCarriedOver = step.carried_over;

  useEffect(() => {
    // Carried-over steps were never executed by this job — their logs and
    // artifacts belong to the source job, so there is nothing to fetch here.
    if (isCarriedOver) {
      setLoadingLogs(false);
      return;
    }

    // Reset state for the current step (guards against same component instance
    // being reused for a different step when parent re-renders).
    let cancelled = false;
    hasLogsRef.current = false;
    setLoadingLogs(true);

    async function fetchLogs() {
      try {
        const data = await getStepLogs(jobId, step.step_name);
        if (cancelled) return;
        // With multi-replica servers, a poll routed to a replica that hasn't
        // received this job's chunks returns "". Don't clear what we already
        // have — keep the last non-empty body until a real update arrives.
        if (data.logs) {
          hasLogsRef.current = true;
          setLogs(data.logs);
        } else if (!hasLogsRef.current) {
          setLogs("");
        }
      } catch {
        // Silently handle — logs may not exist yet
      } finally {
        if (!cancelled) setLoadingLogs(false);
      }
    }

    fetchLogs();

    // Poll while step is running
    const isActive = step.status === "running" || step.status === "ready";
    if (isActive) {
      const interval = setInterval(fetchLogs, 2000);
      return () => {
        cancelled = true;
        clearInterval(interval);
      };
    }

    return () => {
      cancelled = true;
    };
  }, [jobId, step.step_name, step.status, isCarriedOver]);

  const isStreaming = step.status === "running";
  const isSuspendedApproval =
    step.status === "suspended" &&
    (step.action_type === "approval" || step.action_type === "agent");

  // Loop instances are restarted through their placeholder, never individually.
  const showRestart =
    onRestart != null &&
    canRestart &&
    isTerminalJobStatus(jobStatus) &&
    step.loop_source == null;

  return (
    <div className="space-y-3">
      {isSuspendedApproval && (
        <ApprovalCard
          jobId={jobId}
          step={step}
          onAction={onRefresh ?? (() => {})}
        />
      )}
      {showRestart && (
        <div className="flex justify-end">
          <Button
            variant="outline"
            size="sm"
            disabled={restartPending}
            onClick={() => onRestart(step.step_name)}
          >
            <RotateCcw className="mr-1.5 h-3.5 w-3.5" aria-hidden="true" />
            Restart from here
          </Button>
        </div>
      )}
      {step.retry_history && step.retry_history.length > 0 && (
        <div className="rounded-md border px-3 py-2 space-y-1.5">
          <p className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
            Retry History
          </p>
          {step.retry_history.map((attempt) => (
            <div
              key={`retry-${attempt.attempt}`}
              className="flex items-start gap-2 text-xs"
            >
              <span className="shrink-0 font-medium text-muted-foreground">
                #{attempt.attempt + 1}
              </span>
              <span className="shrink-0 text-muted-foreground">
                {formatTime(attempt.started_at)}
                {attempt.failed_at && (
                  <> &ndash; {formatTime(attempt.failed_at)}</>
                )}
              </span>
              {attempt.error && (
                <span className="text-red-600 dark:text-red-400 break-all">
                  {attempt.error}
                </span>
              )}
            </div>
          ))}
        </div>
      )}
      <Tabs defaultValue="logs" className="w-full">
        <TabsList>
          <TabsTrigger value="logs">Logs</TabsTrigger>
          <TabsTrigger value="input">Input</TabsTrigger>
          <TabsTrigger value="output">Output</TabsTrigger>
        </TabsList>
        <TabsContent value="logs">
          {isCarriedOver ? (
            <p
              data-testid="carried-over-notice"
              className="rounded-md border bg-muted/40 px-3 py-2 text-sm text-muted-foreground"
            >
              {sourceJobId ? (
                <>
                  Carried over from job{" "}
                  <Link
                    to={`/jobs/${sourceJobId}`}
                    className="font-mono text-primary hover:underline"
                  >
                    {sourceJobId.substring(0, 8)}
                  </Link>{" "}
                  &mdash; logs and artifacts live there.
                </>
              ) : (
                <>Carried over from an earlier job.</>
              )}
            </p>
          ) : loadingLogs ? (
            <div className="flex items-center justify-center py-8">
              <div className="h-5 w-5 animate-spin rounded-full border-2 border-muted border-t-primary" />
            </div>
          ) : (
            <LogViewer logs={logs} isStreaming={isStreaming} />
          )}
        </TabsContent>
        <TabsContent value="input">
          <JsonViewer data={step.input} />
        </TabsContent>
        <TabsContent value="output">
          <JsonViewer data={step.output} />
        </TabsContent>
      </Tabs>
    </div>
  );
}
