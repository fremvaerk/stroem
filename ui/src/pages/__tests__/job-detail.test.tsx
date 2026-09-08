import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter, Routes, Route } from "react-router";
import type { JobDetail, JobStep, TaskDetail } from "@/lib/types";

vi.mock("@/lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/api")>();
  return {
    ...actual,
    getJob: vi.fn(),
    getTask: vi.fn(),
    getTaskStats: vi.fn(),
    listJobArtifacts: vi.fn(),
    listWorkers: vi.fn(),
    getStepLogs: vi.fn(),
    cancelJob: vi.fn(),
    restartJob: vi.fn(),
  };
});

import {
  getJob,
  getTask,
  getTaskStats,
  listJobArtifacts,
  listWorkers,
  getStepLogs,
} from "@/lib/api";
import { JobDetailPage } from "../job-detail";

const mockGetJob = vi.mocked(getJob);
const mockGetTask = vi.mocked(getTask);

function step(overrides: Partial<JobStep> = {}): JobStep {
  return {
    step_name: "loop",
    action_name: "loop-action",
    action_type: "script",
    action_image: null,
    runner: "local",
    input: null,
    output: null,
    status: "failed",
    worker_id: null,
    started_at: "2026-09-01T10:00:00Z",
    completed_at: "2026-09-01T10:00:05Z",
    suspended_at: null,
    error_message: null,
    when_condition: null,
    depends_on: [],
    for_each_expr: "{{ items }}",
    loop_source: null,
    loop_index: null,
    loop_total: 1,
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

/**
 * A terminal job with a `for_each` placeholder and one instance. The loop group
 * header carries the "Restart from here" button without needing a step
 * selection, which makes it the cheapest probe for the restart affordance.
 */
function job(overrides: Partial<JobDetail> = {}): JobDetail {
  return {
    job_id: "11111111-1111-1111-1111-111111111111",
    workspace: "default",
    task_name: "pipeline",
    mode: "distributed",
    input: {},
    raw_input: {},
    output: null,
    status: "failed",
    source_type: "api",
    source_id: null,
    source_job_id: null,
    restart_from_step: null,
    parent_job_id: null,
    revision: null,
    worker_id: null,
    created_at: "2026-09-01T10:00:00Z",
    started_at: "2026-09-01T10:00:00Z",
    completed_at: "2026-09-01T10:00:10Z",
    retry_of_job_id: null,
    retry_job_id: null,
    retry_attempt: 0,
    max_retries: null,
    steps: [
      step(),
      step({
        step_name: "loop[0]",
        loop_source: "loop",
        loop_index: 0,
        loop_total: 1,
        for_each_expr: null,
      }),
    ],
    ...overrides,
  };
}

function task(canExecute: boolean | undefined = true): TaskDetail {
  return {
    id: "pipeline",
    mode: "distributed",
    input: {},
    flow: {},
    triggers: [],
    can_execute: canExecute,
  };
}

function renderPage() {
  return render(
    <MemoryRouter
      initialEntries={["/jobs/11111111-1111-1111-1111-111111111111"]}
    >
      <Routes>
        <Route path="/jobs/:id" element={<JobDetailPage />} />
      </Routes>
    </MemoryRouter>,
  );
}

const rerunLink = () => screen.queryByRole("link", { name: "Re-run" });
const restartButton = () =>
  screen.queryByRole("button", { name: /restart from here/i });

describe("JobDetailPage — Re-run and Restart availability", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.mocked(getTaskStats).mockRejectedValue(new Error("no stats"));
    vi.mocked(listJobArtifacts).mockResolvedValue([]);
    vi.mocked(listWorkers).mockResolvedValue({ items: [], total: 0 });
    vi.mocked(getStepLogs).mockResolvedValue({ logs: "" });
    mockGetTask.mockResolvedValue(task(true));
  });

  it("offers both actions on a terminal top-level job", async () => {
    mockGetJob.mockResolvedValue(job());
    renderPage();

    expect(await screen.findByText("pipeline")).toBeTruthy();
    await waitFor(() => expect(restartButton()).toBeTruthy());
    expect(rerunLink()).toBeTruthy();
  });

  it("hides both actions on a type: task child job", async () => {
    mockGetJob.mockResolvedValue(
      job({
        source_type: "task",
        parent_job_id: "22222222-2222-2222-2222-222222222222",
      }),
    );
    renderPage();

    expect(await screen.findByText("pipeline")).toBeTruthy();
    // Wait for the permission fetch to settle so this is not just a race.
    await waitFor(() => expect(mockGetTask).toHaveBeenCalled());
    expect(rerunLink()).toBeNull();
    expect(restartButton()).toBeNull();
  });

  it("hides both actions on a hook job", async () => {
    mockGetJob.mockResolvedValue(job({ source_type: "hook" }));
    renderPage();

    expect(await screen.findByText("pipeline")).toBeTruthy();
    await waitFor(() => expect(mockGetTask).toHaveBeenCalled());
    expect(rerunLink()).toBeNull();
    expect(restartButton()).toBeNull();
  });

  it("keeps restart hidden until the permission fetch resolves", async () => {
    mockGetJob.mockResolvedValue(job());
    let resolveTask: (t: TaskDetail) => void = () => {};
    mockGetTask.mockReturnValue(
      new Promise<TaskDetail>((r) => {
        resolveTask = r;
      }),
    );
    renderPage();

    expect(await screen.findByText("pipeline")).toBeTruthy();
    // Permission unknown: Re-run is a plain link and stays, restart does not.
    expect(restartButton()).toBeNull();
    expect(rerunLink()).toBeTruthy();

    resolveTask(task(true));
    await waitFor(() => expect(restartButton()).toBeTruthy());
  });

  it("keeps restart hidden when the permission fetch fails", async () => {
    mockGetJob.mockResolvedValue(job());
    mockGetTask.mockRejectedValue(new Error("403"));
    renderPage();

    expect(await screen.findByText("pipeline")).toBeTruthy();
    await waitFor(() => expect(mockGetTask).toHaveBeenCalled());
    expect(restartButton()).toBeNull();
  });

  it("hides restart when the task denies execute", async () => {
    mockGetJob.mockResolvedValue(job());
    mockGetTask.mockResolvedValue(task(false));
    renderPage();

    expect(await screen.findByText("pipeline")).toBeTruthy();
    await waitFor(() => expect(mockGetTask).toHaveBeenCalled());
    expect(restartButton()).toBeNull();
  });
});
