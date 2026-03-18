import { useState } from "react";
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
}

function StepRow({
  jobId,
  step,
  isExpanded,
  onToggle,
  workerNames,
  isLast,
  indented,
}: StepRowProps) {
  return (
    <div>
      <div
        role="button"
        tabIndex={0}
        aria-label={`${step.step_name}, status: ${step.status}`}
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
          <StepDetail jobId={jobId} step={step} />
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
}: LoopGroupProps) {
  const isPlaceholderExpanded = selectedStep === placeholder.step_name;

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
    <div>
      {/* Placeholder row header */}
      <div
        role="button"
        tabIndex={0}
        aria-label={`${placeholder.step_name}, status: ${placeholder.status}`}
        aria-expanded={isPlaceholderExpanded}
        className="flex w-full gap-3 text-left hover:bg-muted/50 rounded-md px-1 -mx-1 transition-colors cursor-pointer"
        onClick={() =>
          onSelectStep(isPlaceholderExpanded ? null : placeholder.step_name)
        }
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            onSelectStep(isPlaceholderExpanded ? null : placeholder.step_name);
          }
        }}
      >
        <div className="flex flex-col items-center">
          <div className="flex h-6 w-6 items-center justify-center">
            {statusIcons[placeholder.status] ?? (
              <Circle className="h-4 w-4 text-muted-foreground" />
            )}
          </div>
          {(!isLast || isPlaceholderExpanded || instances.length > 0) && (
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

      {isPlaceholderExpanded && (
        <div className="ml-9 mb-4">
          <StepDetail jobId={jobId} step={placeholder} />
        </div>
      )}

      {/* Collapsible instance list */}
      {instances.length > 0 && (
        <div className="ml-6">
          <button
            type="button"
            aria-expanded={instancesExpanded}
            className="flex items-center gap-1.5 mb-1 text-xs text-muted-foreground hover:text-foreground transition-colors"
            onClick={onToggleInstances}
          >
            {instancesExpanded ? (
              <ChevronDown className="h-3 w-3" />
            ) : (
              <ChevronRight className="h-3 w-3" />
            )}
            {instancesExpanded ? "hide" : "show"} {instances.length} iteration
            {instances.length !== 1 ? "s" : ""}
          </button>
          {instancesExpanded && (
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
                />
              ))}
            </div>
          )}
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
}: StepTimelineProps) {
  // Expanded state for loop instance lists, keyed by placeholder step name
  const [expandedLoops, setExpandedLoops] = useState<Record<string, boolean>>({});

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
              instancesExpanded={expandedLoops[step.step_name] ?? false}
              onToggleInstances={() =>
                setExpandedLoops((prev) => ({
                  ...prev,
                  [step.step_name]: !(prev[step.step_name] ?? false),
                }))
              }
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
          />
        );
      })}
    </div>
  );
}
