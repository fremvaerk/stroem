import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { StepTimeline } from "../step-timeline";
import type { JobStep, SkipReason, StepDurationStats } from "@/lib/types";

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
    carried_over: false,
    skip_reason: null,
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
  opts: {
    jobStatus?: string;
    canRestart?: boolean;
    selectedStep?: string | null;
  } = {},
) {
  return render(
    <MemoryRouter>
      <StepTimeline
        jobId="job-1"
        steps={steps}
        selectedStep={opts.selectedStep ?? null}
        onSelectStep={() => {}}
        stepStats={stepStats}
        now={now}
        jobStatus={opts.jobStatus ?? "completed"}
        canRestart={opts.canRestart ?? false}
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

  // -------------------------------------------------------------------------
  // Retry badge — same "attempt N/M" format before and after a retry
  // -------------------------------------------------------------------------
  it("shows 'attempt 1/M' (M = max_retries + 1) on the first execution", () => {
    // max_retries = 2 means max_attempts: 3 was configured (2 retries, 3 total executions).
    const step = makeStep({ max_retries: 2, retry_attempt: 0 });

    renderTimeline([step]);

    expect(screen.getByText("attempt 1/3")).toBeInTheDocument();
    expect(screen.getByLabelText(/attempt 1 of 3/)).toBeInTheDocument();
  });

  it("does not show the attempt badge when max_retries is 0", () => {
    const step = makeStep({ max_retries: 0, retry_attempt: 0 });

    renderTimeline([step]);

    expect(screen.queryByText(/attempt \d/)).not.toBeInTheDocument();
  });

  it("does not show the attempt badge when max_retries is null", () => {
    const step = makeStep({ max_retries: null, retry_attempt: 0 });

    renderTimeline([step]);

    expect(screen.queryByText(/attempt \d/)).not.toBeInTheDocument();
  });

  // -------------------------------------------------------------------------
  // Carried-over rows
  // -------------------------------------------------------------------------
  it("shows a 'carried over' badge and suppresses duration/p50 for carried rows", () => {
    const step = makeStep({ step_name: "build", carried_over: true });
    const stats = new Map([["build", makeStats({ p50_ms: 4500 })]]);

    renderTimeline([step], stats);

    expect(screen.getByText("carried over")).toBeInTheDocument();
    expect(screen.queryByTestId("step-duration-build")).not.toBeInTheDocument();
    expect(screen.queryByTestId("step-p50-build")).not.toBeInTheDocument();
  });

  it("keeps the duration badge for rows that are not carried over", () => {
    const step = makeStep({ step_name: "build", carried_over: false });

    renderTimeline([step]);

    expect(screen.queryByText("carried over")).not.toBeInTheDocument();
    expect(screen.getByTestId("step-duration-build")).toBeInTheDocument();
  });

  // -------------------------------------------------------------------------
  // Restart from here — loop group header
  // -------------------------------------------------------------------------
  function loopSteps() {
    const placeholder = makeStep({
      step_name: "process",
      loop_source: null,
      loop_total: 2,
      for_each_expr: "items",
    });
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
    return [placeholder, instance0, instance1];
  }

  it("renders a 'Restart from here' button on the loop header for a terminal job", () => {
    renderTimeline(loopSteps(), undefined, undefined, {
      jobStatus: "failed",
      canRestart: true,
    });

    expect(
      screen.getByRole("button", { name: /restart from here/i }),
    ).toBeInTheDocument();
  });

  it("does not render the loop-header restart button while the job is running", () => {
    renderTimeline(loopSteps(), undefined, undefined, {
      jobStatus: "running",
      canRestart: true,
    });

    expect(
      screen.queryByRole("button", { name: /restart from here/i }),
    ).not.toBeInTheDocument();
  });

  it("does not render the loop-header restart button without execute permission", () => {
    renderTimeline(loopSteps(), undefined, undefined, {
      jobStatus: "failed",
      canRestart: false,
    });

    expect(
      screen.queryByRole("button", { name: /restart from here/i }),
    ).not.toBeInTheDocument();
  });

  it("never renders a restart button on loop instance rows", () => {
    // Selecting the placeholder auto-expands the group so instance rows render.
    renderTimeline(loopSteps(), undefined, undefined, {
      jobStatus: "failed",
      canRestart: true,
      selectedStep: "process",
    });

    expect(screen.getByText("process[0]")).toBeInTheDocument();
    expect(screen.getByText("process[1]")).toBeInTheDocument();
    // Only the loop header carries the button — never the instances.
    expect(
      screen.getAllByRole("button", { name: /restart from here/i }),
    ).toHaveLength(1);
  });

  describe("skip reason badge", () => {
    const cases: Array<[SkipReason, string]> = [
      ["condition", "condition"],
      ["empty", "empty loop"],
      ["cascade", "upstream skipped"],
      ["unreachable", "upstream failed"],
    ];
    it.each(cases)("shows '%s' as '%s'", (reason, label) => {
      renderTimeline([makeStep({ status: "skipped", skip_reason: reason })]);
      expect(screen.getByTestId("step-skip-build")).toHaveTextContent(label);
    });

    it("falls back to 'condition' for a reasonless row with a when", () => {
      renderTimeline([
        makeStep({ status: "skipped", skip_reason: null, when_condition: "{{ x }}" }),
      ]);
      expect(screen.getByTestId("step-skip-build")).toHaveTextContent("condition");
    });

    it("shows no badge for a reasonless row without a when", () => {
      renderTimeline([makeStep({ status: "skipped", skip_reason: null })]);
      expect(screen.queryByTestId("step-skip-build")).not.toBeInTheDocument();
    });

    it("keeps the blue 'when' badge on a non-skipped conditional step", () => {
      renderTimeline([makeStep({ status: "completed", when_condition: "{{ x }}" })]);
      expect(screen.getByText("when")).toBeInTheDocument();
      expect(screen.queryByTestId("step-skip-build")).not.toBeInTheDocument();
    });
  });
});
