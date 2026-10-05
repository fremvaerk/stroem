import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
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
  ApiError,
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
    skip_reason: null,
    action_workspace: null,
    action_revision: null,
    action_ref: null,
    task_workspace: null,
    task_ref: null,
    task_revision: null,
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
    parent_step_name: null,
    revision: null,
    ref: null,
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
    vi.mocked(getStepLogs).mockResolvedValue({ logs: "", truncated: false, total_bytes: 0, returned_bytes: 0 });
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

describe("JobDetailPage — navigation to the task and the source job", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.mocked(getTaskStats).mockRejectedValue(new Error("no stats"));
    vi.mocked(listJobArtifacts).mockResolvedValue([]);
    vi.mocked(listWorkers).mockResolvedValue({ items: [], total: 0 });
    vi.mocked(getStepLogs).mockResolvedValue({ logs: "", truncated: false, total_bytes: 0, returned_bytes: 0 });
    mockGetTask.mockResolvedValue(task(true));
  });

  it("links the task name in the header to the task page", async () => {
    mockGetJob.mockResolvedValue(
      job({ workspace: "my ws", task_name: "nightly/sync" }),
    );
    renderPage();

    const heading = await screen.findByRole("heading", { level: 1 });
    const taskLink = await within(heading).findByRole("link", {
      name: "nightly/sync",
    });
    expect(taskLink.getAttribute("href")).toBe(
      "/workspaces/my%20ws/tasks/nightly%2Fsync",
    );
  });

  it("shows the ref and short commit of a pinned job", async () => {
    mockGetJob.mockResolvedValue(
      job({ ref: "release/2.3", revision: "3f2a9c0e1b2c3d4e5f60718293a4b5c6d7e8f901" }),
    );
    renderPage();

    const pin = await screen.findByTestId("job-pin");
    expect(pin.textContent).toBe("@ release/2.3 · 3f2a9c0");
  });

  it("shows no pin badge for an unpinned job", async () => {
    mockGetJob.mockResolvedValue(job());
    renderPage();

    await screen.findByText("pipeline");
    expect(screen.queryByTestId("job-pin")).toBeNull();
  });

  it("hides Re-run on a pinned job whose task is gone from the live config", async () => {
    mockGetTask.mockRejectedValue(new ApiError(404, "Task not found"));
    mockGetJob.mockResolvedValue(job({ ref: "release/2.3", revision: "3f2a9c0e" }));
    renderPage();

    await screen.findByTestId("job-pin");
    await waitFor(() => expect(mockGetTask).toHaveBeenCalled());
    await mockGetTask.mock.results[0].value.catch(() => {});
    await waitFor(() => expect(rerunLink()).toBeNull());
  });

  it("keeps Re-run on an unpinned job whose task is gone", async () => {
    mockGetTask.mockRejectedValue(new ApiError(404, "Task not found"));
    mockGetJob.mockResolvedValue(job());
    renderPage();

    await screen.findByText("pipeline");
    await waitFor(() => expect(mockGetTask).toHaveBeenCalled());
    await mockGetTask.mock.results[0].value.catch(() => {});
    expect(rerunLink()).toBeTruthy();
  });

  it("shows the task name as text when the task does not exist", async () => {
    // A single-step hook job's `_hook:<action>` (and `_global_state`, or a
    // task since removed from the workspace) has no task page to link to.
    mockGetTask.mockRejectedValue(new ApiError(404, "Task not found"));
    mockGetJob.mockResolvedValue(
      job({
        task_name: "_hook:notify",
        source_type: "hook",
        source_job_id: "66666666-6666-6666-6666-666666666666",
      }),
    );
    renderPage();

    const heading = await screen.findByRole("heading", { level: 1 });
    await waitFor(() => expect(mockGetTask).toHaveBeenCalled());
    // Let the page's own rejection handler run before asserting.
    await mockGetTask.mock.results[0].value.catch(() => {});
    expect(within(heading).getByText("_hook:notify")).toBeTruthy();
    expect(within(heading).queryByRole("link", { name: "_hook:notify" })).toBeNull();
  });

  it.each([
    ["a server error", new ApiError(500, "Internal server error")],
    ["a network error", new TypeError("Failed to fetch")],
  ])("keeps the task link after %s fetching the task", async (_, err) => {
    mockGetTask.mockRejectedValue(err);
    mockGetJob.mockResolvedValue(job());
    renderPage();

    const heading = await screen.findByRole("heading", { level: 1 });
    await waitFor(() => expect(mockGetTask).toHaveBeenCalled());
    await mockGetTask.mock.results[0].value.catch(() => {});
    expect(within(heading).getByRole("link", { name: "pipeline" })).toBeTruthy();
  });

  it("links a re-run back to its source job from the header and Source card", async () => {
    mockGetJob.mockResolvedValue(
      job({
        source_type: "rerun",
        source_id: "ala@example.com",
        source_job_id: "33333333-3333-3333-3333-333333333333",
      }),
    );
    renderPage();

    const header = await screen.findByTestId("job-lineage");
    expect(header.textContent).toBe("re-run of 33333333");
    const links = screen.getAllByRole("link", { name: "33333333" });
    expect(links).toHaveLength(2);
    for (const link of links) {
      expect(link.getAttribute("href")).toBe(
        "/jobs/33333333-3333-3333-3333-333333333333",
      );
    }
    // Who started the re-run stays visible in the Source card.
    expect(screen.getByText("ala@example.com")).toBeTruthy();
  });

  it("names the step a restart began at", async () => {
    mockGetJob.mockResolvedValue(
      job({
        source_type: "restart",
        source_job_id: "33333333-3333-3333-3333-333333333333",
        restart_from_step: "build",
      }),
    );
    renderPage();

    const header = await screen.findByTestId("job-lineage");
    expect(header.textContent).toBe("restart of 33333333 from build");
  });

  it("links a task retry to the first attempt", async () => {
    mockGetJob.mockResolvedValue(
      job({
        source_type: "retry",
        source_id: "44444444-4444-4444-4444-444444444444",
        retry_of_job_id: "55555555-5555-5555-5555-555555555555",
        retry_attempt: 2,
      }),
    );
    renderPage();

    const header = await screen.findByTestId("job-lineage");
    expect(header.textContent).toBe("retry of 55555555");
    // retry_attempt counts retries; the card counts executions.
    expect(screen.getByText("attempt 3")).toBeTruthy();
    expect(screen.queryByText("44444444-4444-4444-4444-444444444444")).toBeNull();
  });

  it("links a type: task child to its parent job and the step that started it", async () => {
    mockGetJob.mockResolvedValue(
      job({
        source_type: "task",
        source_id: "22222222-2222-2222-2222-222222222222/deploy",
        parent_job_id: "22222222-2222-2222-2222-222222222222",
        parent_step_name: "deploy",
      }),
    );
    renderPage();

    const header = await screen.findByTestId("job-lineage");
    expect(header.textContent).toBe("child of 22222222 at step deploy");
    const links = screen.getAllByRole("link", { name: "22222222" });
    expect(links).toHaveLength(2);
    for (const link of links) {
      expect(link.getAttribute("href")).toBe(
        "/jobs/22222222-2222-2222-2222-222222222222",
      );
    }
  });

  it("links a hook job to the job that fired it", async () => {
    mockGetJob.mockResolvedValue(
      job({
        task_name: "_hook:notify",
        source_type: "hook",
        source_id: "66666666-6666-6666-6666-666666666666",
        source_job_id: "66666666-6666-6666-6666-666666666666",
      }),
    );
    renderPage();

    const header = await screen.findByTestId("job-lineage");
    expect(header.textContent).toBe("hook for 66666666");
    expect(screen.getAllByRole("link", { name: "66666666" })).toHaveLength(2);
    // source_id repeats the linked id, so the Source card does not show it.
    expect(screen.queryByText("66666666-6666-6666-6666-666666666666")).toBeNull();
  });

  it("shows no lineage for a job that was not created from another job", async () => {
    mockGetJob.mockResolvedValue(job({ source_type: "user", source_id: "ala@example.com" }));
    renderPage();

    expect(await screen.findByText("user (ala@example.com)")).toBeTruthy();
    expect(screen.queryByTestId("job-lineage")).toBeNull();
  });
});
