import { useCallback, useEffect, useState } from "react";
import { useParams, Link } from "react-router";
import { ArrowLeft, LayoutList, Network, TriangleAlert } from "lucide-react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { StatusBadge } from "@/components/status-badge";
import { StepTimeline } from "@/components/step-timeline";
import { StepDetail } from "@/components/step-detail";
import { WorkflowDag } from "@/components/workflow-dag";
import { ServerEvents } from "@/components/server-events";
import { JsonViewer } from "@/components/json-viewer";
import { getJob } from "@/lib/api";
import { useTitle } from "@/hooks/use-title";
import { useWorkerNames } from "@/hooks/use-worker-names";
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
  useTitle(id ? `Job: ${id.substring(0, 8)}` : "Job");
  const workerNames = useWorkerNames();
  const [job, setJob] = useState<JobDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [selectedStep, setSelectedStep] = useState<string | null>(null);
  const [viewMode, setViewMode] = useState<"timeline" | "dag">("timeline");

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
          <Link to={`/workspaces/${encodeURIComponent(job.workspace)}/tasks/${encodeURIComponent(job.task_name)}/run`}>
            Re-run
          </Link>
        </Button>
      </div>

      {job.status === "completed" &&
        job.steps.filter((s) => s.status === "failed").length > 0 && (
          <div className="flex items-center gap-2 rounded-md border border-yellow-300 bg-yellow-50 px-4 py-3 dark:border-yellow-700 dark:bg-yellow-950">
            <TriangleAlert className="h-4 w-4 shrink-0 text-yellow-600 dark:text-yellow-400" />
            <p className="text-sm text-yellow-800 dark:text-yellow-200">
              {job.steps.filter((s) => s.status === "failed").length} step(s)
              failed with continue_on_failure:{" "}
              {job.steps
                .filter((s) => s.status === "failed")
                .map((s) => s.step_name)
                .join(", ")}
            </p>
          </div>
        )}

      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-6">
        {[
          { label: "Workspace", value: job.workspace },
          { label: "Worker", value: job.worker_id ? (workerNames.get(job.worker_id) ?? job.worker_id.substring(0, 8)) : "-", linkTo: job.worker_id ? `/workers/${job.worker_id}` : undefined },
          { label: "Source", value: job.source_id ? `${job.source_type} (${job.source_id})` : job.source_type },
          { label: "Created", value: formatTime(job.created_at) },
          { label: "Started", value: formatTime(job.started_at) },
          {
            label: "Duration",
            value: formatDuration(job.started_at, job.completed_at),
          },
        ].map((item) => (
          <div key={item.label} className="rounded-lg border px-4 py-3">
            <p className="text-xs text-muted-foreground">{item.label}</p>
            {"linkTo" in item && item.linkTo ? (
              <Link to={item.linkTo} className="mt-0.5 block text-sm font-medium hover:underline">
                {item.value}
              </Link>
            ) : (
              <p className="mt-0.5 text-sm font-medium">{item.value}</p>
            )}
          </div>
        ))}
      </div>

      <ServerEvents jobId={job.job_id} jobStatus={job.status} />

      {(job.input || job.output) && (
        <div className="grid gap-4 lg:grid-cols-2">
          {job.input && (
            <Card>
              <CardHeader>
                <CardTitle className="text-base">Job Input</CardTitle>
              </CardHeader>
              <CardContent>
                <JsonViewer data={job.input} />
              </CardContent>
            </Card>
          )}
          {job.output && (
            <Card>
              <CardHeader>
                <CardTitle className="text-base">Job Output</CardTitle>
              </CardHeader>
              <CardContent>
                <JsonViewer data={job.output} />
              </CardContent>
            </Card>
          )}
        </div>
      )}

      <Card>
        <CardHeader>
          <div className="flex items-center justify-between">
            <CardTitle className="text-base">Steps</CardTitle>
            <div className="flex gap-1 rounded-md border p-1">
              <Button
                variant={viewMode === "timeline" ? "secondary" : "ghost"}
                size="sm"
                onClick={() => setViewMode("timeline")}
              >
                <LayoutList className="mr-1.5 h-3.5 w-3.5" />
                Timeline
              </Button>
              <Button
                variant={viewMode === "dag" ? "secondary" : "ghost"}
                size="sm"
                onClick={() => setViewMode("dag")}
              >
                <Network className="mr-1.5 h-3.5 w-3.5" />
                Graph
              </Button>
            </div>
          </div>
        </CardHeader>
        <CardContent>
          {job.steps.length === 0 ? (
            <p className="text-sm text-muted-foreground">No steps</p>
          ) : viewMode === "timeline" ? (
            <StepTimeline
              jobId={job.job_id}
              steps={job.steps}
              selectedStep={selectedStep}
              onSelectStep={setSelectedStep}
              workerNames={workerNames}
            />
          ) : (
            <>
              <WorkflowDag
                steps={job.steps}
                selectedStep={selectedStep}
                onSelectStep={setSelectedStep}
              />
              {selectedStep &&
                job.steps.find((s) => s.step_name === selectedStep) && (
                  <div className="mt-4 border-t pt-4">
                    <StepDetail
                      jobId={job.job_id}
                      step={job.steps.find((s) => s.step_name === selectedStep)!}
                    />
                  </div>
                )}
            </>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
