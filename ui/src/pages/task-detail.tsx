import { useCallback, useEffect, useState } from "react";
import { useParams, Link } from "react-router";
import { ArrowLeft, Clock, Folder, Play } from "lucide-react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { WorkflowDag } from "@/components/workflow-dag";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { StatusBadge } from "@/components/status-badge";
import { getTask, listJobs } from "@/lib/api";
import { useTitle } from "@/hooks/use-title";
import type { TaskDetail, JobListItem, FlowStep } from "@/lib/types";

const JOBS_PAGE_SIZE = 20;

/** Topological sort of flow steps (Kahn's algorithm).
 *  Steps with no unmet dependencies come first; ties broken alphabetically. */
function topoSortFlow(
  flow: Record<string, FlowStep>,
): [string, FlowStep][] {
  const entries = Object.entries(flow);
  const inDegree = new Map<string, number>();
  for (const [name, step] of entries) {
    inDegree.set(name, step.depends_on?.length ?? 0);
  }

  const sorted: [string, FlowStep][] = [];
  const queue: string[] = [];

  for (const [name] of entries) {
    if ((inDegree.get(name) ?? 0) === 0) queue.push(name);
  }
  queue.sort();

  while (queue.length > 0) {
    const name = queue.shift()!;
    sorted.push([name, flow[name]]);
    // Decrease in-degree for dependents
    for (const [dep, step] of entries) {
      if (step.depends_on?.includes(name)) {
        const d = (inDegree.get(dep) ?? 1) - 1;
        inDegree.set(dep, d);
        if (d === 0) {
          // Insert into queue in sorted position
          const idx = queue.findIndex((q) => q > dep);
          queue.splice(idx === -1 ? queue.length : idx, 0, dep);
        }
      }
    }
  }

  // Append any remaining (cycle) steps at the end
  for (const [name] of entries) {
    if (!sorted.some(([n]) => n === name)) {
      sorted.push([name, flow[name]]);
    }
  }

  return sorted;
}

function formatTime(dateStr: string | null): string {
  if (!dateStr) return "-";
  return new Date(dateStr).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
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
  return `${minutes}m ${secs}s`;
}

function formatRelativeTime(isoStr: string): string {
  const target = new Date(isoStr).getTime();
  const now = Date.now();
  const diff = target - now;
  if (diff < 0) return "past";
  const minutes = Math.floor(diff / 60000);
  if (minutes < 60) return `in ${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `in ${hours}h ${minutes % 60}m`;
  const days = Math.floor(hours / 24);
  return `in ${days}d ${hours % 24}h`;
}

export function TaskDetailPage() {
  const { workspace, name } = useParams<{ workspace: string; name: string }>();
  useTitle(name ? `Task: ${name}` : "Task");
  const [task, setTask] = useState<TaskDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [jobs, setJobs] = useState<JobListItem[]>([]);
  const [jobsLoading, setJobsLoading] = useState(true);
  const [jobsOffset, setJobsOffset] = useState(0);
  const [selectedDagStep, setSelectedDagStep] = useState<string | null>(null);

  useEffect(() => {
    if (!workspace || !name) return;
    let cancelled = false;

    async function load() {
      try {
        const data = await getTask(workspace!, name!);
        if (!cancelled) setTask(data);
      } catch (err) {
        if (!cancelled)
          setError(err instanceof Error ? err.message : "Failed to load task");
      } finally {
        if (!cancelled) setLoading(false);
      }
    }

    load();
    return () => {
      cancelled = true;
    };
  }, [workspace, name]);

  const loadJobs = useCallback(async () => {
    if (!workspace || !name) return;
    try {
      const data = await listJobs(JOBS_PAGE_SIZE, jobsOffset, {
        workspace,
        taskName: name,
      });
      setJobs(data);
    } catch {
      // ignore
    } finally {
      setJobsLoading(false);
    }
  }, [workspace, name, jobsOffset]);

  useEffect(() => {
    setJobsLoading(true);
    loadJobs();
  }, [loadJobs]);

  if (loading) {
    return (
      <div className="flex items-center justify-center py-20">
        <div className="h-8 w-8 animate-spin rounded-full border-4 border-muted border-t-primary" />
      </div>
    );
  }

  if (error || !task) {
    return (
      <div className="py-20 text-center">
        <p className="text-sm text-destructive">{error || "Task not found"}</p>
        <Button variant="link" asChild className="mt-2">
          <Link to="/tasks">Back to tasks</Link>
        </Button>
      </div>
    );
  }

  const flowSteps = topoSortFlow(task.flow);
  const inputFields = Object.entries(task.input);

  return (
    <div className="space-y-6">
      <div className="flex items-center gap-4">
        <Button variant="ghost" size="icon" asChild>
          <Link to="/tasks">
            <ArrowLeft className="h-4 w-4" />
          </Link>
        </Button>
        <div className="flex-1">
          <h1 className="text-2xl font-semibold tracking-tight">
            {task.name}
          </h1>
          <div className="mt-1 flex items-center gap-2">
            <Badge variant="outline" className="font-mono text-xs">
              {task.mode}
            </Badge>
            {task.folder && (
              <Badge variant="secondary" className="font-mono text-xs">
                <Folder className="mr-1 h-3 w-3" />
                {task.folder}
              </Badge>
            )}
            {workspace && (
              <Badge variant="secondary" className="font-mono text-xs">
                {workspace}
              </Badge>
            )}
            <span className="text-sm text-muted-foreground">
              {flowSteps.length} step{flowSteps.length !== 1 ? "s" : ""}
            </span>
          </div>
        </div>
        <Button asChild>
          <Link to="run">
            <Play className="mr-2 h-4 w-4" />
            Run Task
          </Link>
        </Button>
      </div>

      {inputFields.length > 0 && (
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Input Parameters</CardTitle>
          </CardHeader>
          <CardContent>
            <div className="space-y-3">
              {inputFields.map(([key, field]) => (
                <div
                  key={key}
                  className="flex items-start justify-between rounded-md border px-3 py-2"
                >
                  <div>
                    <span className="font-mono text-sm">{key}</span>
                    {field.description && (
                      <p className="text-xs text-muted-foreground">
                        {field.description}
                      </p>
                    )}
                  </div>
                  <div className="flex items-center gap-2">
                    <Badge variant="secondary" className="font-mono text-xs">
                      {field.type}
                    </Badge>
                    {field.required && (
                      <Badge
                        variant="secondary"
                        className="bg-yellow-100 text-xs text-yellow-800 dark:bg-yellow-900/30 dark:text-yellow-400"
                      >
                        required
                      </Badge>
                    )}
                  </div>
                </div>
              ))}
            </div>
          </CardContent>
        </Card>
      )}

      {task.triggers.length > 0 && (
        <Card>
          <CardHeader>
            <CardTitle className="text-base">
              <Clock className="mr-2 inline-block h-4 w-4" />
              Triggers
            </CardTitle>
          </CardHeader>
          <CardContent>
            <div className="space-y-3">
              {task.triggers.map((trigger) => (
                <div
                  key={trigger.name}
                  className="rounded-md border px-3 py-2"
                >
                  <div className="flex items-center gap-2">
                    <span className="font-mono text-sm font-medium">
                      {trigger.name}
                    </span>
                    <Badge variant="outline" className="text-xs">
                      {trigger.type}
                    </Badge>
                    {trigger.cron && (
                      <Badge
                        variant="secondary"
                        className="font-mono text-xs"
                      >
                        {trigger.cron}
                      </Badge>
                    )}
                    <Badge
                      variant="secondary"
                      className={`text-xs ${trigger.enabled ? "bg-green-100 text-green-800 dark:bg-green-900/30 dark:text-green-400" : "bg-red-100 text-red-800 dark:bg-red-900/30 dark:text-red-400"}`}
                    >
                      {trigger.enabled ? "enabled" : "disabled"}
                    </Badge>
                  </div>
                  {trigger.next_runs.length > 0 && (
                    <div className="mt-2 space-y-1">
                      <p className="text-xs font-medium text-muted-foreground">
                        Upcoming
                      </p>
                      {trigger.next_runs.map((run, i) => (
                        <div
                          key={i}
                          className="flex items-center gap-2 text-xs text-muted-foreground"
                        >
                          <span className="font-mono">
                            {formatRelativeTime(run)}
                          </span>
                          <span className="text-muted-foreground/60">
                            {new Date(run).toLocaleString()}
                          </span>
                        </div>
                      ))}
                    </div>
                  )}
                </div>
              ))}
            </div>
          </CardContent>
        </Card>
      )}

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Flow Steps</CardTitle>
        </CardHeader>
        <CardContent>
          <div className="mb-4">
            <WorkflowDag
              flow={task.flow}
              selectedStep={selectedDagStep}
              onSelectStep={setSelectedDagStep}
            />
          </div>
          <div className="space-y-2">
            {flowSteps.map(([stepName, step], index) => (
              <div
                key={stepName}
                className="flex items-center gap-3 rounded-md border px-3 py-2"
              >
                <span className="flex h-6 w-6 shrink-0 items-center justify-center rounded-full bg-muted text-xs font-medium">
                  {index + 1}
                </span>
                <div className="min-w-0 flex-1">
                  <span className="font-mono text-sm">{stepName}</span>
                  <p className="text-xs text-muted-foreground">
                    action: {step.action}
                    {step.depends_on && step.depends_on.length > 0 && (
                      <> &middot; depends on: {step.depends_on.join(", ")}</>
                    )}
                  </p>
                </div>
              </div>
            ))}
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Recent Runs</CardTitle>
        </CardHeader>
        <CardContent>
          {jobsLoading ? (
            <div className="flex items-center justify-center py-12">
              <div className="h-8 w-8 animate-spin rounded-full border-4 border-muted border-t-primary" />
            </div>
          ) : jobs.length === 0 ? (
            <p className="py-8 text-center text-sm text-muted-foreground">
              No runs yet.
            </p>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Job ID</TableHead>
                  <TableHead>Status</TableHead>
                  <TableHead>Source</TableHead>
                  <TableHead>Created</TableHead>
                  <TableHead className="text-right">Duration</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {jobs.map((job) => (
                  <TableRow key={job.job_id}>
                    <TableCell>
                      <Link
                        to={`/jobs/${job.job_id}`}
                        className="font-mono text-sm font-medium hover:underline"
                      >
                        {job.job_id.substring(0, 8)}...
                      </Link>
                    </TableCell>
                    <TableCell>
                      <StatusBadge status={job.status} />
                    </TableCell>
                    <TableCell className="text-xs text-muted-foreground">
                      {job.source_type}
                      {job.source_id && (
                        <span className="ml-1 text-muted-foreground/60">
                          ({job.source_id})
                        </span>
                      )}
                    </TableCell>
                    <TableCell className="font-mono text-xs text-muted-foreground">
                      {formatTime(job.created_at)}
                    </TableCell>
                    <TableCell className="text-right font-mono text-xs text-muted-foreground">
                      {formatDuration(job.started_at, job.completed_at)}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}

          <div className="mt-4 flex items-center justify-between">
            <Button
              variant="outline"
              size="sm"
              disabled={jobsOffset === 0}
              onClick={() =>
                setJobsOffset((o) => Math.max(0, o - JOBS_PAGE_SIZE))
              }
            >
              Previous
            </Button>
            <span className="text-xs text-muted-foreground">
              Page {Math.floor(jobsOffset / JOBS_PAGE_SIZE) + 1}
            </span>
            <Button
              variant="outline"
              size="sm"
              disabled={jobs.length < JOBS_PAGE_SIZE}
              onClick={() => setJobsOffset((o) => o + JOBS_PAGE_SIZE)}
            >
              Next
            </Button>
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
