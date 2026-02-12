import { useCallback, useEffect, useState } from "react";
import { useParams, Link } from "react-router";
import { ArrowLeft } from "lucide-react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { StatusBadge } from "@/components/status-badge";
import { StepTimeline } from "@/components/step-timeline";
import { LogViewer } from "@/components/log-viewer";
import { useJobLogs } from "@/hooks/use-job-logs";
import { getJob } from "@/lib/api";
import type { JobDetail } from "@/lib/types";

function formatTime(dateStr: string | null): string {
  if (!dateStr) return "-";
  return new Date(dateStr).toLocaleString();
}

function formatDuration(start: string | null, end: string | null): string {
  if (!start) return "-";
  const s = new Date(start).getTime();
  const e = end ? new Date(end).getTime() : Date.now();
  const diff = Math.max(0, e - s);
  const seconds = Math.floor(diff / 1000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const secs = seconds % 60;
  if (minutes < 60) return `${minutes}m ${secs}s`;
  const hours = Math.floor(minutes / 60);
  return `${hours}h ${minutes % 60}m`;
}

export function JobDetailPage() {
  const { id } = useParams<{ id: string }>();
  const [job, setJob] = useState<JobDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const { logs, isStreaming } = useJobLogs(id, job?.status ?? null);

  const load = useCallback(async () => {
    if (!id) return;
    try {
      const data = await getJob(id);
      setJob(data);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load job");
    } finally {
      setLoading(false);
    }
  }, [id]);

  useEffect(() => {
    load();
  }, [load]);

  // Auto-refresh while pending or running
  useEffect(() => {
    if (!job || (job.status !== "pending" && job.status !== "running")) return;
    const interval = setInterval(load, 3000);
    return () => clearInterval(interval);
  }, [job, load]);

  if (loading) {
    return (
      <div className="flex items-center justify-center py-20">
        <div className="h-8 w-8 animate-spin rounded-full border-4 border-muted border-t-primary" />
      </div>
    );
  }

  if (error || !job) {
    return (
      <div className="py-20 text-center">
        <p className="text-sm text-destructive">{error || "Job not found"}</p>
        <Button variant="link" asChild className="mt-2">
          <Link to="/jobs">Back to jobs</Link>
        </Button>
      </div>
    );
  }

  return (
    <div className="space-y-6">
      <div className="flex items-center gap-4">
        <Button variant="ghost" size="icon" asChild>
          <Link to="/jobs">
            <ArrowLeft className="h-4 w-4" />
          </Link>
        </Button>
        <div className="flex-1">
          <div className="flex items-center gap-3">
            <h1 className="text-2xl font-semibold tracking-tight">
              {job.task_name}
            </h1>
            <StatusBadge status={job.status} />
          </div>
          <p className="mt-0.5 font-mono text-xs text-muted-foreground">
            {job.job_id}
          </p>
        </div>
        <Button variant="outline" asChild>
          <Link to={`/tasks/${encodeURIComponent(job.task_name)}`}>
            Re-run
          </Link>
        </Button>
      </div>

      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
        {[
          { label: "Source", value: job.source_type },
          { label: "Created", value: formatTime(job.created_at) },
          { label: "Started", value: formatTime(job.started_at) },
          {
            label: "Duration",
            value: formatDuration(job.started_at, job.completed_at),
          },
        ].map((item) => (
          <div key={item.label} className="rounded-lg border px-4 py-3">
            <p className="text-xs text-muted-foreground">{item.label}</p>
            <p className="mt-0.5 text-sm font-medium">{item.value}</p>
          </div>
        ))}
      </div>

      <div className="grid gap-6 lg:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Steps</CardTitle>
          </CardHeader>
          <CardContent>
            {job.steps.length === 0 ? (
              <p className="text-sm text-muted-foreground">No steps</p>
            ) : (
              <StepTimeline steps={job.steps} />
            )}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="text-base">Logs</CardTitle>
          </CardHeader>
          <CardContent>
            <LogViewer logs={logs} isStreaming={isStreaming} />
          </CardContent>
        </Card>
      </div>
    </div>
  );
}
