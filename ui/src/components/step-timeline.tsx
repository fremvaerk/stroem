import { useEffect, useMemo, useState } from "react";
import { Link, useNavigate } from "react-router";
import {
  AlertCircle,
  ChevronDown,
  ChevronRight,
  Circle,
  Repeat,
  RotateCcw,
} from "lucide-react";
import { StepDetail } from "@/components/step-detail";
import { RestartDialog } from "@/components/restart-dialog";
import { Button } from "@/components/ui/button";
import { formatDuration, formatDurationMs } from "@/lib/formatting";
import { isTerminalJobStatus } from "@/lib/job-status";
import { restartJob } from "@/lib/api";
import type { RestartPlanResponse } from "@/lib/api";
import { statusIcons } from "@/lib/status-icons";
import type { JobStep, StepDurationStats } from "@/lib/types";
import { cn, formatActionName } from "@/lib/utils";

/** Shared props threaded from the job page down to every restartable row. */
interface RestartContext {
  /** Status of the owning job — restart is only offered once it is terminal. */
  jobStatus: string;
  /** Task-level `can_execute`; false hides every restart affordance. */
  canRestart: boolean;
  /** Source job of a restart, used to link carried-over steps to their logs. */
  sourceJobId?: string | null;
  /** Opens the shared restart dialog for the named step. */
  onRestart: (stepName: string) => void;
  /** Step whose restart request is currently in flight, if any. */
  restartPendingStep: string | null;
}

interface StepTimelineProps {
  jobId: string;
  steps: JobStep[];
  selectedStep: string | null;
  onSelectStep: (stepName: string | null) => void;
  workerNames?: Map<string, string>;
  onRefresh?: () => void;
  /** Per-step duration stats keyed by step_name. Optional. */
  stepStats?: Map<string, StepDurationStats>;
  /** Current epoch ms — passed in so child overrun calculations stay pure. */
  now?: number;
  /** Status of the owning job — restart is only offered once it is terminal. */
  jobStatus: string;
  /** Task-level `can_execute`; false hides every restart affordance. */
  canRestart: boolean;
  /** Source job of a restart, used to link carried-over steps to their logs. */
  sourceJobId?: string | null;
  /** Task name, shown in the restart confirmation dialog. */
  taskName?: string;
}

interface StepRowProps {
  jobId: string;
  step: JobStep;
  isExpanded: boolean;
  onToggle: () => void;
  workerNames?: Map<string, string>;
  isLast: boolean;
  /** When true, renders with indentation (instance step inside a loop group) */
  indented?: boolean;
  onRefresh?: () => void;
  stats?: StepDurationStats;
  now?: number;
  restart: RestartContext;
}

function StepRow({
  jobId,
  step,
  isExpanded,
  onToggle,
  workerNames,
  isLast,
  indented,
  onRefresh,
  stats,
  now,
  restart,
}: StepRowProps) {
  // Compute overrun for currently-running steps where we have stats.
  // `now` is supplied by the parent (kept pure for render); if absent we skip.
  const runningOverrunMs =
    now != null && stats?.p50_ms != null && step.status === "running" && step.started_at
      ? now - new Date(step.started_at).getTime() - stats.p50_ms
      : null;
  const isOverrun = runningOverrunMs != null && runningOverrunMs > 0;

  // Determine whether a retry-at badge should appear. When `now` is provided
  // (the normal path — ticker-driven), compare against it. When absent (SSR /
  // tests without a ticker), skip the comparison to stay render-pure.
  const isRetryPending =
    step.status === "ready" &&
    step.retry_at != null &&
    now != null &&
    new Date(step.retry_at).getTime() > now;
  return (
    <div id={`step-${step.step_name}`}>
      <div
        role="button"
        tabIndex={0}
        aria-label={[
          `${step.step_name}, status: ${step.status}`,
          step.max_retries != null && step.max_retries > 0
            ? `attempt ${step.retry_attempt + 1} of ${step.max_retries + 1}`
            : null,
          isRetryPending ? "waiting for retry" : null,
        ].filter(Boolean).join(", ")}
        aria-expanded={isExpanded}
        className={cn(
          "flex w-full gap-3 text-left hover:bg-muted/50 rounded-md px-1 -mx-1 transition-colors cursor-pointer",
          indented && "pl-3",
        )}
        onClick={onToggle}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            onToggle();
          }
        }}
      >
        <div className="flex flex-col items-center">
          <div className="flex h-6 w-6 items-center justify-center">
            {statusIcons[step.status] ?? (
              <Circle className="h-4 w-4 text-muted-foreground" />
            )}
          </div>
          {(!isLast || isExpanded) && (
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
              {formatActionName(step.action_name)}
            </span>
            {step.status === "suspended" && (
              <span className="rounded bg-amber-100 px-1.5 py-0.5 text-[10px] font-medium text-amber-700 dark:bg-amber-900/30 dark:text-amber-400">
                awaiting approval
              </span>
            )}
            {step.when_condition && step.status === "skipped" && (
              <span className="rounded bg-amber-100 px-1.5 py-0.5 text-[10px] font-medium text-amber-700 dark:bg-amber-900/30 dark:text-amber-400">
                condition
              </span>
            )}
            {step.when_condition && step.status !== "skipped" && (
              <span className="rounded bg-blue-100 px-1.5 py-0.5 text-[10px] font-medium text-blue-700 dark:bg-blue-900/30 dark:text-blue-400">
                when
              </span>
            )}
            {step.max_retries != null && step.max_retries > 0 && (
              // One format for every execution: "attempt 1/3" on the first run,
              // "attempt 2/3" after a retry. Muted until a retry has happened.
              <span
                className={
                  step.retry_attempt > 0
                    ? "rounded bg-violet-100 px-1.5 py-0.5 text-[10px] font-medium text-violet-700 dark:bg-violet-900/30 dark:text-violet-400"
                    : "rounded bg-muted px-1.5 py-0.5 text-[10px] font-medium text-muted-foreground"
                }
              >
                attempt {step.retry_attempt + 1}/{step.max_retries + 1}
              </span>
            )}
            {isRetryPending && (
              <span className="rounded bg-amber-100 px-1.5 py-0.5 text-[10px] font-medium text-amber-700 dark:bg-amber-900/30 dark:text-amber-400">
                waiting for retry
              </span>
            )}
            {step.worker_id && workerNames && (
              <Link
                to={`/workers/${step.worker_id}`}
                className="text-xs text-muted-foreground/60 hover:underline"
                onClick={(e) => e.stopPropagation()}
              >
                {workerNames.get(step.worker_id) ?? step.worker_id.substring(0, 8)}
              </Link>
            )}
            {step.carried_over && (
              <span className="rounded bg-muted px-1.5 py-0.5 text-[10px] font-medium text-muted-foreground">
                carried over
              </span>
            )}
            {/* Carried-over rows never ran in this job — their timings belong
                to the source job, so duration and p50 comparisons are hidden. */}
            {!step.carried_over && (step.started_at || step.completed_at) && (
              <span
                data-testid={`step-duration-${step.step_name}`}
                className={cn(
                  "ml-auto font-mono text-xs",
                  isOverrun ? "text-amber-700 dark:text-amber-400" : "text-muted-foreground",
                )}
              >
                {formatDuration(step.started_at, step.completed_at)}
              </span>
            )}
            {!step.carried_over && stats?.p50_ms != null && (
              <span
                className="font-mono text-[10px] text-muted-foreground/70"
                title={`p50 ${formatDurationMs(stats.p50_ms)} / p95 ${formatDurationMs(stats.p95_ms)} over ${stats.sample_size} runs`}
                data-testid={`step-p50-${step.step_name}`}
              >
                p50 {formatDurationMs(stats.p50_ms)}
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
      </div>
      {isExpanded && (
        <div className="ml-9 mb-4">
          <StepDetail
            jobId={jobId}
            step={step}
            onRefresh={onRefresh}
            jobStatus={restart.jobStatus}
            canRestart={restart.canRestart}
            sourceJobId={restart.sourceJobId}
            onRestart={restart.onRestart}
            restartPending={restart.restartPendingStep === step.step_name}
          />
        </div>
      )}
    </div>
  );
}

interface LoopGroupProps {
  jobId: string;
  placeholder: JobStep;
  instances: JobStep[];
  selectedStep: string | null;
  onSelectStep: (stepName: string | null) => void;
  workerNames?: Map<string, string>;
  isLast: boolean;
  instancesExpanded: boolean;
  onToggleInstances: () => void;
  onRefresh?: () => void;
  restart: RestartContext;
}

function LoopGroup({
  jobId,
  placeholder,
  instances,
  selectedStep,
  onSelectStep,
  workerNames,
  isLast,
  instancesExpanded,
  onToggleInstances,
  onRefresh,
  restart,
}: LoopGroupProps) {
  // Placeholder steps have no logs/input of their own — toggling expands iterations instead
  const isPlaceholderExpanded = instancesExpanded;

  const showRestart =
    restart.canRestart && isTerminalJobStatus(restart.jobStatus);

  const completedCount = instances.filter(
    (s) => s.status === "completed" || s.status === "skipped",
  ).length;
  const failedCount = instances.filter(
    (s) => s.status === "failed" || s.status === "cancelled",
  ).length;
  const doneCount = completedCount + failedCount;
  const total = placeholder.loop_total ?? instances.length;

  // Empty loop: loop_total is 0, or no instances and status is terminal
  const terminalStatuses = ["completed", "failed", "cancelled", "skipped"];
  const isEmpty =
    placeholder.loop_total === 0 ||
    (instances.length === 0 && terminalStatuses.includes(placeholder.status));

  return (
    <div id={`step-${placeholder.step_name}`}>
      {/* Placeholder row header */}
      <div
        role="button"
        tabIndex={0}
        aria-label={`${placeholder.step_name}, status: ${placeholder.status}`}
        aria-expanded={isPlaceholderExpanded}
        className="flex w-full gap-3 text-left hover:bg-muted/50 rounded-md px-1 -mx-1 transition-colors cursor-pointer"
        onClick={onToggleInstances}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            onToggleInstances();
          }
        }}
      >
        <div className="flex flex-col items-center">
          <div className="flex h-6 w-6 items-center justify-center">
            {statusIcons[placeholder.status] ?? (
              <Circle className="h-4 w-4 text-muted-foreground" />
            )}
          </div>
          {(!isLast || isPlaceholderExpanded) && (
            <div className="w-px flex-1 bg-border" />
          )}
        </div>
        <div className="flex-1 pb-4">
          <div className="flex items-center gap-2">
            {isPlaceholderExpanded ? (
              <ChevronDown className="h-3.5 w-3.5 text-muted-foreground" />
            ) : (
              <ChevronRight className="h-3.5 w-3.5 text-muted-foreground" />
            )}
            <span className="font-mono text-sm font-medium">
              {placeholder.step_name}
            </span>
            <span className="text-xs text-muted-foreground">
              {formatActionName(placeholder.action_name)}
            </span>
            <Repeat className="h-3 w-3 text-violet-500" />
            {isEmpty ? (
              <span className="rounded bg-muted px-1.5 py-0.5 text-[10px] font-medium text-muted-foreground">
                0 iterations
              </span>
            ) : instances.length > 0 && (
              <span
                className={cn(
                  "rounded px-1.5 py-0.5 text-[10px] font-medium",
                  failedCount > 0
                    ? "bg-red-100 text-red-700 dark:bg-red-900/30 dark:text-red-400"
                    : "bg-violet-100 text-violet-700 dark:bg-violet-900/30 dark:text-violet-400",
                )}
              >
                {doneCount}/{total}
                {failedCount > 0 && ` (${failedCount} failed)`}
              </span>
            )}
            {placeholder.when_condition && placeholder.status === "skipped" && (
              <span className="rounded bg-amber-100 px-1.5 py-0.5 text-[10px] font-medium text-amber-700 dark:bg-amber-900/30 dark:text-amber-400">
                condition
              </span>
            )}
            {placeholder.when_condition && placeholder.status !== "skipped" && (
              <span className="rounded bg-blue-100 px-1.5 py-0.5 text-[10px] font-medium text-blue-700 dark:bg-blue-900/30 dark:text-blue-400">
                when
              </span>
            )}
            {placeholder.carried_over && (
              <span className="rounded bg-muted px-1.5 py-0.5 text-[10px] font-medium text-muted-foreground">
                carried over
              </span>
            )}
            {!placeholder.carried_over &&
              (placeholder.started_at || placeholder.completed_at) && (
                <span
                  data-testid={`step-duration-${placeholder.step_name}`}
                  className="ml-auto font-mono text-xs text-muted-foreground"
                >
                  {formatDuration(
                    placeholder.started_at,
                    placeholder.completed_at,
                  )}
                </span>
              )}
            {/* The placeholder header only toggles its instances and never
                opens StepDetail, so the restart affordance lives here. Both
                click and key events are stopped so activating the button does
                not also expand/collapse the group. */}
            {showRestart && (
              <Button
                variant="outline"
                size="sm"
                className="ml-2 h-6 px-2 text-xs"
                disabled={restart.restartPendingStep === placeholder.step_name}
                onClick={(e) => {
                  e.stopPropagation();
                  restart.onRestart(placeholder.step_name);
                }}
                onKeyDown={(e) => e.stopPropagation()}
              >
                <RotateCcw className="mr-1 h-3 w-3" aria-hidden="true" />
                Restart from here
              </Button>
            )}
          </div>
          {placeholder.error_message && (
            <div className="mt-1 ml-5.5 flex items-start gap-1.5 rounded-md bg-red-50 px-2 py-1.5 dark:bg-red-900/20">
              <AlertCircle className="mt-0.5 h-3.5 w-3.5 shrink-0 text-red-500" />
              <pre className="whitespace-pre-wrap break-all text-xs text-red-700 dark:text-red-400">
                {placeholder.error_message}
              </pre>
            </div>
          )}
        </div>
      </div>

      {/* Instance list — shown directly when placeholder is expanded */}
      {instances.length > 0 && instancesExpanded && (
        <div className="ml-6">
          <div className="border-l pl-3 ml-2 space-y-0">
            {instances.map((instance, idx) => (
              <StepRow
                key={instance.step_name}
                jobId={jobId}
                step={instance}
                isExpanded={selectedStep === instance.step_name}
                onToggle={() =>
                  onSelectStep(
                    selectedStep === instance.step_name
                      ? null
                      : instance.step_name,
                  )
                }
                workerNames={workerNames}
                isLast={idx === instances.length - 1}
                indented
                onRefresh={onRefresh}
                restart={restart}
              />
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

export function StepTimeline({
  jobId,
  steps,
  selectedStep,
  onSelectStep,
  workerNames,
  onRefresh,
  stepStats,
  now,
  jobStatus,
  canRestart,
  sourceJobId,
  taskName = "",
}: StepTimelineProps) {
  // User-toggled loop expansion state, keyed by placeholder step name
  const [expandedLoops, setExpandedLoops] = useState<Record<string, boolean>>({});

  // Restart state — a single dialog serves both the StepDetail button and the
  // loop-group header button, so the plan lives here rather than per-row.
  const navigate = useNavigate();
  const [restartStep, setRestartStep] = useState<string | null>(null);
  const [restartPlan, setRestartPlan] = useState<RestartPlanResponse | null>(null);
  const [restartPending, setRestartPending] = useState<string | null>(null);
  const [restartConfirming, setRestartConfirming] = useState(false);

  const handleRestart = async (stepName: string) => {
    setRestartPending(stepName);
    try {
      const plan = await restartJob(jobId, stepName, true);
      setRestartPlan(plan);
      setRestartStep(stepName);
    } catch (err) {
      alert(err instanceof Error ? err.message : "Failed to plan restart");
    } finally {
      setRestartPending(null);
    }
  };

  const closeRestartDialog = () => {
    setRestartStep(null);
    setRestartPlan(null);
  };

  const confirmRestart = async () => {
    if (!restartStep) return;
    setRestartConfirming(true);
    try {
      const res = await restartJob(jobId, restartStep, false);
      closeRestartDialog();
      if (res.job_id) navigate(`/jobs/${res.job_id}`);
    } catch (err) {
      alert(err instanceof Error ? err.message : "Failed to restart job");
    } finally {
      setRestartConfirming(false);
    }
  };

  const restartContext: RestartContext = {
    jobStatus,
    canRestart,
    sourceJobId,
    onRestart: handleRestart,
    restartPendingStep: restartPending,
  };

  // Auto-expand: if the selected step is a loop placeholder or instance, include its group
  const effectiveExpandedLoops = useMemo(() => {
    if (!selectedStep) return expandedLoops;
    const step = steps.find((s) => s.step_name === selectedStep);
    if (!step) return expandedLoops;
    const isLoopPlaceholder = step.loop_total !== null || step.for_each_expr !== null;
    const loopKey = isLoopPlaceholder ? selectedStep : step.loop_source;
    if (loopKey && expandedLoops[loopKey] === undefined) {
      return { ...expandedLoops, [loopKey]: true };
    }
    return expandedLoops;
  }, [expandedLoops, selectedStep, steps]);

  // Scroll selected step into view after DOM updates
  useEffect(() => {
    if (!selectedStep) return;
    const handle = requestAnimationFrame(() => {
      document.getElementById(`step-${selectedStep}`)?.scrollIntoView({ behavior: "smooth", block: "nearest" });
    });
    return () => cancelAnimationFrame(handle);
  }, [selectedStep]);

  // Separate placeholder steps (loop_source === null but loop_total !== null)
  // from instance steps (loop_source !== null).
  // Only `steps` is the input — `now` does not affect grouping — so we can
  // memoize this expensive Map rebuild independently of the 1s ticker.
  const { instancesBySource, topLevelSteps } = useMemo(() => {
    const map = new Map<string, JobStep[]>();
    for (const step of steps) {
      if (step.loop_source !== null) {
        const existing = map.get(step.loop_source) ?? [];
        existing.push(step);
        map.set(step.loop_source, existing);
      }
    }
    // Sort instances by loop_index so they render in order regardless of API order
    for (const arr of map.values()) {
      arr.sort((a, b) => (a.loop_index ?? 0) - (b.loop_index ?? 0));
    }
    return {
      instancesBySource: map,
      topLevelSteps: steps.filter((s) => s.loop_source === null),
    };
  }, [steps]);

  return (
    <div className="space-y-0">
      {topLevelSteps.map((step, index) => {
        const isLast = index === topLevelSteps.length - 1;
        const instances = instancesBySource.get(step.step_name);

        if (instances) {
          return (
            <LoopGroup
              key={step.step_name}
              jobId={jobId}
              placeholder={step}
              instances={instances}
              selectedStep={selectedStep}
              onSelectStep={onSelectStep}
              workerNames={workerNames}
              isLast={isLast}
              instancesExpanded={effectiveExpandedLoops[step.step_name] ?? false}
              onToggleInstances={() => {
                const wasExpanded = effectiveExpandedLoops[step.step_name] ?? false;
                setExpandedLoops((prev) => ({
                  ...prev,
                  [step.step_name]: !wasExpanded,
                }));
                // Clear orphaned instance selection when collapsing
                if (wasExpanded && selectedStep !== null) {
                  const sel = steps.find((s) => s.step_name === selectedStep);
                  if (sel?.loop_source === step.step_name) {
                    onSelectStep(null);
                  }
                }
              }}
              onRefresh={onRefresh}
              restart={restartContext}
            />
          );
        }

        return (
          <StepRow
            key={step.step_name}
            jobId={jobId}
            step={step}
            isExpanded={selectedStep === step.step_name}
            onToggle={() =>
              onSelectStep(
                selectedStep === step.step_name ? null : step.step_name,
              )
            }
            workerNames={workerNames}
            isLast={isLast}
            onRefresh={onRefresh}
            stats={stepStats?.get(step.step_name)}
            now={now}
            restart={restartContext}
          />
        );
      })}
      <RestartDialog
        open={restartStep !== null}
        plan={restartPlan}
        taskName={taskName}
        stepName={restartStep ?? ""}
        busy={restartConfirming}
        onConfirm={confirmRestart}
        onCancel={closeRestartDialog}
      />
    </div>
  );
}
