import { useCallback, useEffect, useRef, useState } from "react";
import {
  Tabs,
  TabsContent,
  TabsList,
  TabsTrigger,
} from "@/components/ui/tabs";
import { LogViewer } from "@/components/log-viewer";
import { JsonViewer } from "@/components/json-viewer";
import { ApprovalCard } from "@/components/approval-card";
import { getStepLogs } from "@/lib/api";
import { formatTime } from "@/lib/formatting";
import type { JobStep } from "@/lib/types";

interface StepDetailProps {
  jobId: string;
  step: JobStep;
  onRefresh?: () => void;
}

export function StepDetail({ jobId, step, onRefresh }: StepDetailProps) {
  const [logs, setLogs] = useState("");
  const [loadingLogs, setLoadingLogs] = useState(true);
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const fetchLogs = useCallback(async () => {
    try {
      const data = await getStepLogs(jobId, step.step_name);
      setLogs(data.logs);
    } catch {
      // Silently handle — logs may not exist yet
    } finally {
      setLoadingLogs(false);
    }
  }, [jobId, step.step_name]);

  useEffect(() => {
    fetchLogs();

    // Poll while step is running
    const isActive = step.status === "running" || step.status === "ready";
    if (isActive) {
      intervalRef.current = setInterval(fetchLogs, 2000);
    }

    return () => {
      if (intervalRef.current) {
        clearInterval(intervalRef.current);
      }
    };
  }, [fetchLogs, step.status]);

  const isStreaming = step.status === "running";
  const isSuspendedApproval =
    step.status === "suspended" &&
    (step.action_type === "approval" || step.action_type === "agent");

  return (
    <div className="space-y-3">
      {isSuspendedApproval && (
        <ApprovalCard
          jobId={jobId}
          step={step}
          onAction={onRefresh ?? (() => {})}
        />
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
          {loadingLogs ? (
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
