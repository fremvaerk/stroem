import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor, fireEvent } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { StepDetail } from "../step-detail";
import type { JobStep } from "@/lib/types";

const getStepLogs = vi.fn();

vi.mock("@/lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/api")>();
  return {
    ...actual,
    getStepLogs: (...args: unknown[]) => getStepLogs(...args),
  };
});

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
    ...overrides,
  };
}

function renderDetail(
  step: JobStep,
  props: Partial<React.ComponentProps<typeof StepDetail>> = {},
) {
  return render(
    <MemoryRouter>
      <StepDetail jobId="job-1" step={step} {...props} />
    </MemoryRouter>,
  );
}

beforeEach(() => {
  getStepLogs.mockReset();
  getStepLogs.mockResolvedValue({ logs: "hello" });
});

describe("StepDetail carried-over steps", () => {
  it("does not fetch logs and points at the source job", async () => {
    renderDetail(makeStep({ carried_over: true }), {
      sourceJobId: "abcdef12-3456-7890-abcd-ef1234567890",
    });

    const notice = await screen.findByTestId("carried-over-notice");
    expect(notice.textContent).toContain("Carried over from job");
    expect(notice.textContent).toContain("logs and artifacts live there");
    expect(screen.getByRole("link", { name: "abcdef12" })).toHaveAttribute(
      "href",
      "/jobs/abcdef12-3456-7890-abcd-ef1234567890",
    );
    expect(getStepLogs).not.toHaveBeenCalled();
  });

  it("falls back to the no-link variant when the source job is gone", async () => {
    renderDetail(makeStep({ carried_over: true }), { sourceJobId: null });

    const notice = await screen.findByTestId("carried-over-notice");
    expect(notice.textContent).toContain("Carried over from an earlier job");
    expect(screen.queryByRole("link")).not.toBeInTheDocument();
    expect(getStepLogs).not.toHaveBeenCalled();
  });

  it("still fetches logs for a step that actually ran", async () => {
    renderDetail(makeStep({ carried_over: false }));

    await waitFor(() => expect(getStepLogs).toHaveBeenCalledWith("job-1", "build"));
    expect(screen.queryByTestId("carried-over-notice")).not.toBeInTheDocument();
  });
});

describe("StepDetail restart button", () => {
  const onRestart = vi.fn();

  beforeEach(() => onRestart.mockReset());

  it("appears for a terminal job when the user may execute the task", () => {
    renderDetail(makeStep(), {
      jobStatus: "failed",
      canRestart: true,
      onRestart,
    });

    fireEvent.click(screen.getByRole("button", { name: /restart from here/i }));
    expect(onRestart).toHaveBeenCalledWith("build");
  });

  it("is hidden while the job is still running", () => {
    renderDetail(makeStep({ status: "running" }), {
      jobStatus: "running",
      canRestart: true,
      onRestart,
    });

    expect(
      screen.queryByRole("button", { name: /restart from here/i }),
    ).not.toBeInTheDocument();
  });

  it("is hidden without execute permission on the task", () => {
    renderDetail(makeStep(), {
      jobStatus: "completed",
      canRestart: false,
      onRestart,
    });

    expect(
      screen.queryByRole("button", { name: /restart from here/i }),
    ).not.toBeInTheDocument();
  });

  it("is hidden on a loop instance row", () => {
    renderDetail(
      makeStep({ step_name: "process[0]", loop_source: "process", loop_index: 0 }),
      { jobStatus: "failed", canRestart: true, onRestart },
    );

    expect(
      screen.queryByRole("button", { name: /restart from here/i }),
    ).not.toBeInTheDocument();
  });
});
