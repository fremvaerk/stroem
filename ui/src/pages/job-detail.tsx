import { useCallback, useEffect, useMemo, useState } from "react";
import { useParams, Link } from "react-router";
import { ArrowLeft, ChevronUp, ChevronDown, Network, TriangleAlert, XCircle } from "lucide-react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { StatusBadge } from "@/components/status-badge";
import { InfoGrid } from "@/components/info-grid";
import { StepTimeline } from "@/components/step-timeline";
import { WorkflowDag } from "@/components/workflow-dag";
import { ErrorBoundary } from "@/components/error-boundary";
import { ServerEvents } from "@/components/server-events";
import { ArtifactList } from "@/components/artifact-list";
import { JsonViewer } from "@/components/json-viewer";
import { LoadingSpinner } from "@/components/loading-spinner";
import { getJob, getTaskStats, cancelJob, listJobArtifacts } from "@/lib/api";
import { useTitle } from "@/hooks/use-title";
import { useWorkerNames } from "@/hooks/use-worker-names";
import { formatTime, formatDuration, formatDurationMs } from "@/lib/formatting";
import { computeEta } from "@/lib/eta";
import type { ArtifactItem } from "@/lib/api";
import type { JobDetail, TaskStatsResponse } from "@/lib/types";

export function JobDetailPage() {
  const { id } = useParams<{ id: string }>();
  useTitle(id ? `Job: ${id.substring(0, 8)}` : "Job");
  const workerNames = useWorkerNames();
  const [job, setJob] = useState<JobDetail | null>(null);
  const [stats, setStats] = useState<TaskStatsResponse | null>(null);
  const [artifacts, setArtifacts] = useState<ArtifactItem[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [selectedStep, setSelectedStep] = useState<string | null>(null);
  const [graphOpen, setGraphOpen] = useState(true);
  const [cancelling, setCancelling] = useState(false);

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

  // Fetch task stats once per (workspace, task) — used for the ETA pill and
  // per-step p50 comparisons. Failure is non-fatal: ETA simply won't render.
  useEffect(() => {
    if (!job) return;
    let cancelled = false;
    getTaskStats(job.workspace, job.task_name, 50)
      .then((data) => {
        if (!cancelled) setStats(data);
      })
      .catch(() => {
        /* non-fatal */
      });
    return () => {
      cancelled = true;
    };
  }, [job?.workspace, job?.task_name]); // eslint-disable-line react-hooks/exhaustive-deps

  // Fetch artifacts whenever the job reloads. Artifacts accumulate as steps
  // upload them; piggy-backing on the job poll cadence keeps things simple.
  // Failure is non-fatal — the section just stays empty.
  useEffect(() => {
    if (!job) return;
    let cancelled = false;
    listJobArtifacts(job.job_id)
      .then((data) => {
        if (!cancelled) setArtifacts(data);
      })
      .catch(() => {
        /* non-fatal */
      });
    return () => {
      cancelled = true;
    };
  }, [job?.job_id, job?.status, job?.steps.length]); // eslint-disable-line react-hooks/exhaustive-deps

  // Re-tick once a second while running so the ETA countdown stays current
  // without re-fetching the job. (load() already fires on a 3–8s cadence.)
  const [tickNow, setTickNow] = useState(() => Date.now());
  useEffect(() => {
    if (!job || job.status !== "running") return;
    const t = setInterval(() => setTickNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, [job?.status]); // eslint-disable-line react-hooks/exhaustive-deps

  const eta = useMemo(() => {
    if (!job) return null;
    return computeEta({ job, stats, now: tickNow });
  }, [job, stats, tickNow]);

  const stepStatsMap = useMemo(() => {
    if (!stats) return undefined;
    const m = new Map<string, (typeof stats.steps)[number]>();
    for (const s of stats.steps) m.set(s.step_name, s);
    return m;
  }, [stats]);

  // Auto-refresh while pending or running (adaptive interval)
  // A job with suspended steps has status "running", so this covers approval gates too.
  // Also keep polling when any step has a future retry_at so the badge doesn't freeze.
  useEffect(() => {
    const hasRetryPending = job?.steps?.some(
      (s) => s.retry_at != null && new Date(s.retry_at) > new Date(),
    );
    if (!job || (job.status !== "pending" && job.status !== "running" && !hasRetryPending)) return;
    const hasActiveSteps = job.steps.some(
      (s) => s.status === "running" || s.status === "suspended",
    );
    const interval = setInterval(load, hasActiveSteps ? 3000 : 8000);
    return () => clearInterval(interval);
  }, [job, load]);

  // Show graph when there are any top-level steps (excluding loop instances)
  const showGraph = useMemo(() => {
    if (!job) return false;
    return job.steps.some((s) => s.loop_source === null);
  }, [job]);

  if (loading) {
    return <LoadingSpinner />;
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
            {eta?.type === "eta" && (
              <span
                data-testid="job-eta"
                className="inline-flex items-center rounded-md bg-emerald-100 px-2 py-0.5 text-xs font-medium text-emerald-900 dark:bg-emerald-950 dark:text-emerald-100"
                title={`Based on the p50 of recent runs (${stats?.task.sample_size ?? 0} samples).`}
              >
                ETA ~{formatDurationMs(eta.remainingMs)}
              </span>
            )}
            {eta?.type === "overrun" && (
              <span
                data-testid="job-overrun"
                className="inline-flex items-center rounded-md bg-amber-100 px-2 py-0.5 text-xs font-medium text-amber-900 dark:bg-amber-950 dark:text-amber-100"
                title={`Past p50 of ${formatDurationMs(eta.referenceMs)} from recent runs.`}
              >
                Overrun {formatDurationMs(eta.overrunMs)} vs p50
              </span>
            )}
          </div>
          <p className="mt-0.5 font-mono text-xs text-muted-foreground">
            {job.job_id}
          </p>
        </div>
        <div className="flex items-center gap-2">
          {(job.status === "pending" || job.status === "running") && (
            <Button
              variant="destructive"
              size="sm"
              disabled={cancelling}
              onClick={async () => {
                if (!window.confirm("Cancel this job? Running steps will be killed.")) return;
                setCancelling(true);
                try {
                  await cancelJob(job.job_id);
                } catch (err) {
                  alert(err instanceof Error ? err.message : "Failed to cancel job");
                } finally {
                  setCancelling(false);
                  await load();
                }
              }}
            >
              <XCircle className="mr-1.5 h-3.5 w-3.5" />
              {cancelling ? "Cancelling..." : "Cancel Job"}
            </Button>
          )}
          <Button variant="outline" asChild>
            <Link
              to={`/workspaces/${encodeURIComponent(job.workspace)}/tasks/${encodeURIComponent(job.task_name)}`}
              state={{ sourceJobId: job.job_id, rawInput: job.raw_input }}
            >
              Re-run
            </Link>
          </Button>
        </div>
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

      <InfoGrid
        columns={6}
        items={[
          { label: "Workspace", value: job.workspace },
          {
            label: "Source",
            value: job.source_id
              ? `${job.source_type} (${job.source_id})`
              : job.source_type,
          },
          {
            label: "Revision",
            value: job.revision ? (
              <span title={job.revision}>
                {job.revision.length > 12
                  ? job.revision.substring(0, 8)
                  : job.revision}
              </span>
            ) : (
              "\u2014"
            ),
          },
          { label: "Created", value: formatTime(job.created_at) },
          { label: "Started", value: formatTime(job.started_at) },
          {
            label: "Duration",
            value: formatDuration(job.started_at, job.completed_at),
          },
          ...(job.retry_of_job_id
            ? [
                {
                  label: "Retry of",
                  value: (
                    <Link
                      to={`/jobs/${job.retry_of_job_id}`}
                      className="font-mono text-xs text-primary hover:underline"
                    >
                      {job.retry_of_job_id.substring(0, 8)}
                    </Link>
                  ),
                },
              ]
            : []),
          ...(job.retry_job_id
            ? [
                {
                  label: "Retried",
                  value: (
                    <Link
                      to={`/jobs/${job.retry_job_id}`}
                      className="font-mono text-xs text-primary hover:underline"
                    >
                      {job.retry_job_id.substring(0, 8)}
                    </Link>
                  ),
                },
              ]
            : []),
          ...(job.source_job_id && job.source_type === "rerun"
            ? [
                {
                  label: "Re-run of",
                  value: (
                    <Link
                      to={`/jobs/${job.source_job_id}`}
                      className="font-mono text-xs text-primary hover:underline"
                    >
                      {job.source_job_id.substring(0, 8)}
                    </Link>
                  ),
                },
              ]
            : []),
        ]}
      />

      <ServerEvents jobId={job.job_id} jobStatus={job.status} />

      <ArtifactList items={artifacts} />

      {(job.input || job.output) && (
        <div className="grid gap-4 lg:grid-cols-2">
          {job.input && (
            <Card data-testid="job-input">
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
            {showGraph && (
              <Button
                variant="ghost"
                size="sm"
                onClick={() => setGraphOpen((prev) => !prev)}
              >
                <Network className="mr-1.5 h-3.5 w-3.5" />
                Graph
                {graphOpen ? (
                  <ChevronUp className="ml-1 h-3.5 w-3.5" />
                ) : (
                  <ChevronDown className="ml-1 h-3.5 w-3.5" />
                )}
              </Button>
            )}
          </div>
        </CardHeader>
        <CardContent>
          {job.steps.length === 0 ? (
            <p className="text-sm text-muted-foreground">No steps</p>
          ) : (
            <>
              {showGraph && graphOpen && (
                <ErrorBoundary
                  fallback={
                    <p className="py-8 text-center text-sm text-muted-foreground">
                      DAG visualization failed to render.
                    </p>
                  }
                >
                  <div className="mb-4">
                    <WorkflowDag
                      steps={job.steps}
                      selectedStep={selectedStep}
                      onSelectStep={setSelectedStep}
                    />
                  </div>
                </ErrorBoundary>
              )}
              <StepTimeline
                jobId={job.job_id}
                steps={job.steps}
                selectedStep={selectedStep}
                onSelectStep={setSelectedStep}
                workerNames={workerNames}
                onRefresh={load}
                stepStats={stepStatsMap}
                now={tickNow}
              />
            </>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
