import { useEffect, useMemo, useState } from "react";
import { Link } from "react-router";
import {
  AlertCircle,
  ChevronDown,
  ChevronRight,
  Circle,
  Repeat,
} from "lucide-react";
import { StepDetail } from "@/components/step-detail";
import { formatDuration } from "@/lib/formatting";
import { statusIcons } from "@/lib/status-icons";
import type { JobStep } from "@/lib/types";
import { cn, formatActionName } from "@/lib/utils";

interface StepTimelineProps {
  jobId: string;
  steps: JobStep[];
  selectedStep: string | null;
  onSelectStep: (stepName: string | null) => void;
  workerNames?: Map<string, string>;
  onRefresh?: () => void;
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
}: StepRowProps) {
  return (
    <div id={`step-${step.step_name}`}>
      <div
        role="button"
        tabIndex={0}
        aria-label={[
          `${step.step_name}, status: ${step.status}`,
          step.retry_attempt > 0 && step.max_retries != null
            ? `attempt ${step.retry_attempt + 1} of ${step.max_retries + 1}`
            : null,
          step.status === "ready" && step.retry_at && new Date(step.retry_at) > new Date()
            ? "waiting for retry"
            : null,
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
            {step.max_retries != null && step.max_retries > 0 && step.retry_attempt > 0 && (
              <span className="rounded bg-violet-100 px-1.5 py-0.5 text-[10px] font-medium text-violet-700 dark:bg-violet-900/30 dark:text-violet-400">
                attempt {step.retry_attempt + 1}/{step.max_retries + 1}
              </span>
            )}
            {step.max_retries != null && step.max_retries > 0 && step.retry_attempt === 0 && (
              <span className="rounded bg-muted px-1.5 py-0.5 text-[10px] font-medium text-muted-foreground">
                retry: {step.max_retries}
              </span>
            )}
            {step.status === "ready" && step.retry_at && new Date(step.retry_at) > new Date() && (
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
      </div>
      {isExpanded && (
        <div className="ml-9 mb-4">
          <StepDetail jobId={jobId} step={step} onRefresh={onRefresh} />
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
}: LoopGroupProps) {
  // Placeholder steps have no logs/input of their own — toggling expands iterations instead
  const isPlaceholderExpanded = instancesExpanded;

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
            {(placeholder.started_at || placeholder.completed_at) && (
              <span className="ml-auto font-mono text-xs text-muted-foreground">
                {formatDuration(placeholder.started_at, placeholder.completed_at)}
              </span>
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
}: StepTimelineProps) {
  // User-toggled loop expansion state, keyed by placeholder step name
  const [expandedLoops, setExpandedLoops] = useState<Record<string, boolean>>({});

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
  const instancesBySource = new Map<string, JobStep[]>();

  for (const step of steps) {
    if (step.loop_source !== null) {
      const existing = instancesBySource.get(step.loop_source) ?? [];
      existing.push(step);
      instancesBySource.set(step.loop_source, existing);
    }
  }
  // Sort instances by loop_index so they render in order regardless of API order
  for (const arr of instancesBySource.values()) {
    arr.sort((a, b) => (a.loop_index ?? 0) - (b.loop_index ?? 0));
  }

  // Top-level steps: not an instance step themselves.
  const topLevelSteps = steps.filter((s) => s.loop_source === null);

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
          />
        );
      })}
    </div>
  );
}
