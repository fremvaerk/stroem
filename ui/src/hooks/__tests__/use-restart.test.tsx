import { describe, it, expect, vi, beforeEach } from "vitest";
import type { ReactNode } from "react";
import { act, renderHook, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router";

vi.mock("@/lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/api")>();
  return { ...actual, restartJob: vi.fn() };
});

import { restartJob, type RestartPlanResponse } from "@/lib/api";
import { useRestart } from "../use-restart";

const wrapper = ({ children }: { children: ReactNode }) => (
  <MemoryRouter>{children}</MemoryRouter>
);

const plan = (steps: string[]): RestartPlanResponse => ({
  start_steps: steps,
  restart_steps: steps,
  carried_over: [],
  carried_failed: [],
});

function deferred<T>() {
  let resolve: (v: T) => void = () => {};
  const promise = new Promise<T>((r) => {
    resolve = r;
  });
  return { promise, resolve };
}

describe("useRestart", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("opens the dialog with the plan, then confirms for the same job", async () => {
    vi.mocked(restartJob).mockResolvedValueOnce(plan(["b"]));
    const { result } = renderHook(() => useRestart("job-a"), { wrapper });

    await act(() => result.current.open({ kind: "failed" }));
    expect(result.current.target).toEqual({ kind: "failed" });
    expect(result.current.plan).toEqual(plan(["b"]));

    vi.mocked(restartJob).mockResolvedValueOnce({ ...plan(["b"]), job_id: "new" });
    await act(() => result.current.confirm());
    expect(vi.mocked(restartJob)).toHaveBeenLastCalledWith("job-a", { kind: "failed" }, false);
    expect(result.current.target).toBeNull();
  });

  it("drops a plan that arrives after the page moved to another job", async () => {
    const late = deferred<RestartPlanResponse>();
    vi.mocked(restartJob).mockReturnValueOnce(late.promise);
    const { result, rerender } = renderHook(({ id }) => useRestart(id), {
      wrapper,
      initialProps: { id: "job-a" },
    });

    let opening: Promise<void> = Promise.resolve();
    act(() => {
      opening = result.current.open({ kind: "failed" });
    });
    expect(result.current.pending).toEqual({ kind: "failed" });

    rerender({ id: "job-b" });
    expect(result.current.pending).toBeNull();

    await act(async () => {
      late.resolve(plan(["a-step"]));
      await opening;
    });
    expect(result.current.target).toBeNull();
    expect(result.current.plan).toBeNull();

    // Nothing to confirm on job B: no create request is sent.
    await act(() => result.current.confirm());
    expect(vi.mocked(restartJob)).toHaveBeenCalledTimes(1);

    // Going back to job A does not resurrect the stale plan either.
    rerender({ id: "job-a" });
    expect(result.current.plan).toBeNull();
  });

  it("hides an open dialog of another job and confirms only its own", async () => {
    vi.mocked(restartJob).mockResolvedValueOnce(plan(["x"]));
    const { result, rerender } = renderHook(({ id }) => useRestart(id), {
      wrapper,
      initialProps: { id: "job-a" },
    });
    await act(() => result.current.open({ kind: "step", step: "x" }));
    expect(result.current.plan).not.toBeNull();

    rerender({ id: "job-b" });
    expect(result.current.target).toBeNull();
    expect(result.current.plan).toBeNull();
    await act(() => result.current.confirm());
    await waitFor(() => expect(vi.mocked(restartJob)).toHaveBeenCalledTimes(1));

    rerender({ id: "job-a" });
    expect(result.current.plan).toBeNull();
  });
});
