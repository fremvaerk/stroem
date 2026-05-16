import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { StepTimeline } from "../step-timeline";
import type { JobStep, StepDurationStats } from "@/lib/types";

// ---------------------------------------------------------------------------
// Factories
// ---------------------------------------------------------------------------

function makeStep(overrides: Partial<JobStep> = {}): JobStep {
  return {
    step_name: "build",
    action_name: "build-action",
    action_type: "script",
    action_image: null,
    runner: "local",
    input: null,
    output: null,
    status: "completed",
    worker_id: null,
    started_at: "2024-06-15T10:00:00Z",
    completed_at: "2024-06-15T10:00:05Z",
    suspended_at: null,
    error_message: null,
    when_condition: null,
    depends_on: [],
    for_each_expr: null,
    loop_source: null,
    loop_index: null,
    loop_total: null,
    retry_attempt: 0,
    max_retries: null,
    retry_history: [],
    retry_at: null,
    approval_message: null,
    approval_fields: null,
    ...overrides,
  };
}

function makeStats(overrides: Partial<StepDurationStats> = {}): StepDurationStats {
  return {
    step_name: "build",
    sample_size: 10,
    avg_ms: 5000,
    p50_ms: 4500,
    p95_ms: 8000,
    min_ms: 3000,
    max_ms: 9000,
    ...overrides,
  };
}

function renderTimeline(
  steps: JobStep[],
  stepStats?: Map<string, StepDurationStats>,
  now?: number,
) {
  return render(
    <MemoryRouter>
      <StepTimeline
        jobId="job-1"
        steps={steps}
        selectedStep={null}
        onSelectStep={() => {}}
        stepStats={stepStats}
        now={now}
      />
    </MemoryRouter>,
  );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("StepTimeline", () => {
  // -------------------------------------------------------------------------
  // p50 badge — present when stepStats contains the step
  // -------------------------------------------------------------------------
  it("shows the p50 badge when stepStats contains the step", () => {
    const step = makeStep({ step_name: "build", status: "completed" });
    const stats = new Map([["build", makeStats({ p50_ms: 4500 })]]);

    renderTimeline([step], stats);

    expect(screen.getByTestId("step-p50-build")).toBeInTheDocument();
    expect(screen.getByTestId("step-p50-build")).toHaveTextContent(/p50/i);
  });

  // -------------------------------------------------------------------------
  // p50 badge — absent when stepStats is undefined
  // -------------------------------------------------------------------------
  it("does not show p50 badge when stepStats is undefined", () => {
    const step = makeStep({ step_name: "build" });

    renderTimeline([step], undefined);

    expect(screen.queryByTestId("step-p50-build")).not.toBeInTheDocument();
  });

  // -------------------------------------------------------------------------
  // p50 badge — absent when step is missing from the map
  // -------------------------------------------------------------------------
  it("does not show p50 badge when step is missing from stepStats map", () => {
    const step = makeStep({ step_name: "deploy" });
    // Map only has "build", not "deploy"
    const stats = new Map([["build", makeStats()]]);

    renderTimeline([step], stats);

    expect(screen.queryByTestId("step-p50-deploy")).not.toBeInTheDocument();
  });

  // -------------------------------------------------------------------------
  // For-each instance rows — no badge (map lookup misses placeholder name)
  // -------------------------------------------------------------------------
  it("does not show p50 badge for for-each instance rows", () => {
    // Placeholder (loop_source is null but has loop_total)
    const placeholder = makeStep({
      step_name: "process",
      loop_source: null,
      loop_total: 2,
      for_each_expr: "items",
    });
    // Instance steps: loop_source is the placeholder name
    const instance0 = makeStep({
      step_name: "process[0]",
      loop_source: "process",
      loop_index: 0,
      loop_total: null,
    });
    const instance1 = makeStep({
      step_name: "process[1]",
      loop_source: "process",
      loop_index: 1,
      loop_total: null,
    });

    // Stats keyed by "process[0]" and "process[1]" — instance names don't appear
    // in the stats map (stats use the placeholder/step name from the DB)
    const stats = new Map([
      ["process[0]", makeStats({ step_name: "process[0]" })],
      ["process[1]", makeStats({ step_name: "process[1]" })],
    ]);

    renderTimeline([placeholder, instance0, instance1], stats);

    // Instance rows render inside a collapsed LoopGroup — they are not shown
    // unless expanded. The placeholder row itself should not get a badge since
    // it is rendered as a LoopGroup header (not a StepRow).
    expect(screen.queryByTestId("step-p50-process[0]")).not.toBeInTheDocument();
    expect(screen.queryByTestId("step-p50-process[1]")).not.toBeInTheDocument();
  });

  // -------------------------------------------------------------------------
  // Overrun colouring — running step past p50 gets amber text class
  // -------------------------------------------------------------------------
  it("applies amber colour class to duration when running step exceeds p50", () => {
    const startedAt = "2024-06-15T10:00:00Z";
    const step = makeStep({
      step_name: "slow-build",
      status: "running",
      started_at: startedAt,
      completed_at: null,
    });
    const stats = new Map([
      [
        "slow-build",
        makeStats({ step_name: "slow-build", p50_ms: 1_000 }), // p50 = 1s
      ],
    ]);
    // now = startedAt + 10s = well past p50 of 1s
    const startMs = new Date(startedAt).getTime();
    const now = startMs + 10_000;

    const { container } = renderTimeline([step], stats, now);

    // The duration span should have amber text class when overrun
    const amberSpan = container.querySelector(".text-amber-700, .dark\\:text-amber-400");
    expect(amberSpan).toBeInTheDocument();
  });

  // -------------------------------------------------------------------------
  // No overrun colouring when running step is within p50
  // -------------------------------------------------------------------------
  it("does not apply amber colour class when running step is within p50", () => {
    const startedAt = "2024-06-15T10:00:00Z";
    const step = makeStep({
      step_name: "fast-build",
      status: "running",
      started_at: startedAt,
      completed_at: null,
    });
    const stats = new Map([
      [
        "fast-build",
        makeStats({ step_name: "fast-build", p50_ms: 30_000 }), // p50 = 30s
      ],
    ]);
    // now = startedAt + 1s — well within p50
    const startMs = new Date(startedAt).getTime();
    const now = startMs + 1_000;

    const { container } = renderTimeline([step], stats, now);

    // The duration span should NOT have amber colouring
    const amberSpan = container.querySelector(".text-amber-700");
    expect(amberSpan).not.toBeInTheDocument();
  });
});
