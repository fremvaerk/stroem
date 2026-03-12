import { useCallback, useEffect, useState, type FormEvent } from "react";
import { useParams, Link, useNavigate } from "react-router";
import {
  ArrowLeft,
  Clock,
  Folder,
} from "lucide-react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { WorkflowDag } from "@/components/workflow-dag";
import { ErrorBoundary } from "@/components/error-boundary";
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
import { PaginationControls } from "@/components/pagination-controls";
import { LoadingSpinner } from "@/components/loading-spinner";
import { InputFieldRow } from "@/components/task/input-field-row";
import { SECRET_SENTINEL } from "@/components/task/constants";
import { getTask, listJobs, executeTask } from "@/lib/api";
import { useTitle } from "@/hooks/use-title";
import type { TaskDetail, JobListItem, FlowStep } from "@/lib/types";
import { formatActionName } from "@/lib/utils";
import { formatTime, formatDuration, formatFutureTime } from "@/lib/formatting";

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


export function TaskDetailPage() {
  const { workspace, name } = useParams<{ workspace: string; name: string }>();
  const navigate = useNavigate();
  useTitle(name ? `Task: ${name}` : "Task");
  const [task, setTask] = useState<TaskDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [jobs, setJobs] = useState<JobListItem[]>([]);
  const [jobsTotal, setJobsTotal] = useState(0);
  const [jobsLoading, setJobsLoading] = useState(true);
  const [jobsOffset, setJobsOffset] = useState(0);
  const [selectedDagStep, setSelectedDagStep] = useState<string | null>(null);
  const [values, setValues] = useState<Record<string, unknown>>({});
  const [submitError, setSubmitError] = useState("");
  const [submitting, setSubmitting] = useState(false);

  useEffect(() => {
    if (!workspace || !name) return;
    let cancelled = false;

    async function load() {
      try {
        const data = await getTask(workspace!, name!);
        if (!cancelled) {
          setTask(data);
          const defaults: Record<string, unknown> = {};
          for (const [key, field] of Object.entries(data.input)) {
            if (field.secret && field.default !== undefined) {
              defaults[key] = SECRET_SENTINEL;
            } else if (field.default !== undefined) {
              defaults[key] = field.default;
            } else if (field.type === "boolean") {
              defaults[key] = false;
            } else {
              defaults[key] = "";
            }
          }
          setValues(defaults);
        }
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
      setJobs(data.items);
      setJobsTotal(data.total);
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

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    if (!workspace || !task) return;
    setSubmitError("");
    setSubmitting(true);
    try {
      const input: Record<string, unknown> = {};
      for (const [key, val] of Object.entries(values)) {
        const field = task.input[key];
        if (field?.secret && val === SECRET_SENTINEL) continue;
        if (field?.type === "number") {
          input[key] = Number(val);
        } else {
          input[key] = val;
        }
      }
      const res = await executeTask(workspace, task.id, input);
      navigate(`/jobs/${res.job_id}`);
    } catch (err) {
      setSubmitError(err instanceof Error ? err.message : "Failed to execute task");
    } finally {
      setSubmitting(false);
    }
  }

  if (loading) {
    return <LoadingSpinner />;
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
  const inputFields = Object.entries(task.input).sort(
    ([, a], [, b]) => (a.order ?? Infinity) - (b.order ?? Infinity),
  );

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
            {task.name ?? task.id}
          </h1>
          {task.description && (
            <p className="mt-1 text-sm text-muted-foreground">
              {task.description}
            </p>
          )}
          <div className="mt-1 flex items-center gap-2">
            {task.name && (
              <Badge variant="secondary" className="font-mono text-xs">
                {task.id}
              </Badge>
            )}
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
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Run Task</CardTitle>
        </CardHeader>
        <CardContent>
          <form onSubmit={handleSubmit}>
            {submitError && (
              <div className="mb-4 rounded-md bg-destructive/10 px-3 py-2 text-sm text-destructive">
                {submitError}
              </div>
            )}
            {inputFields.length > 0 && (
              <div className="space-y-4">
                {inputFields.map(([key, field]) => (
                  <InputFieldRow
                    key={key}
                    fieldKey={key}
                    field={field}
                    value={values[key]}
                    onChange={(v) => setValues((prev) => ({ ...prev, [key]: v }))}
                    connections={task.connections}
                  />
                ))}
              </div>
            )}
            <div className={inputFields.length > 0 ? "mt-6" : ""}>
              <Button type="submit" disabled={submitting || task.can_execute === false}>
                {submitting ? "Running..." : "Run Task"}
              </Button>
              {task.can_execute === false && (
                <p className="mt-2 text-sm text-muted-foreground">
                  You have view-only access to this task.
                </p>
              )}
            </div>
          </form>
        </CardContent>
      </Card>

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
                            {formatFutureTime(run)}
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
            <ErrorBoundary
              fallback={
                <p className="py-8 text-center text-sm text-muted-foreground">
                  DAG visualization failed to render.
                </p>
              }
            >
              <WorkflowDag
                flow={task.flow}
                selectedStep={selectedDagStep}
                onSelectStep={setSelectedDagStep}
              />
            </ErrorBoundary>
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
                  <div className="flex items-center gap-2">
                    <span className="font-mono text-sm">
                      {step.name ?? stepName}
                    </span>
                    {step.name && (
                      <span className="text-xs text-muted-foreground font-mono">
                        {stepName}
                      </span>
                    )}
                  </div>
                  {step.description && (
                    <p className="text-xs text-muted-foreground">
                      {step.description}
                    </p>
                  )}
                  <p className="text-xs text-muted-foreground">
                    action: {formatActionName(step.action)}
                    {step.depends_on && step.depends_on.length > 0 && (
                      <> &middot; depends on: {step.depends_on.join(", ")}</>
                    )}
                    {step.when && (
                      <> &middot; when: <code className="rounded bg-muted px-1 py-0.5 text-[10px]">{step.when}</code></>
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
            <LoadingSpinner />
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

          <PaginationControls
            offset={jobsOffset}
            pageSize={JOBS_PAGE_SIZE}
            total={jobsTotal}
            onPrevious={() =>
              setJobsOffset((o) => Math.max(0, o - JOBS_PAGE_SIZE))
            }
            onNext={() => setJobsOffset((o) => o + JOBS_PAGE_SIZE)}
          />
        </CardContent>
      </Card>
    </div>
  );
}
