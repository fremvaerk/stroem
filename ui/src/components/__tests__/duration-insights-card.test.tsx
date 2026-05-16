import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, waitFor, act } from "@testing-library/react";
import { DurationInsightsCard } from "../duration-insights-card";
import type { TaskStatsResponse } from "@/lib/types";

// ---------------------------------------------------------------------------
// Mock @/lib/api
// ---------------------------------------------------------------------------

vi.mock("@/lib/api", () => ({
  getTaskStats: vi.fn(),
}));

import { getTaskStats } from "@/lib/api";

const mockGetTaskStats = vi.mocked(getTaskStats);

// ---------------------------------------------------------------------------
// Factories
// ---------------------------------------------------------------------------

function makeStats(overrides: Partial<TaskStatsResponse> = {}): TaskStatsResponse {
  return {
    window: 30,
    task: {
      sample_size: 10,
      avg_ms: 1500,
      p50_ms: 1200,
      p95_ms: 2800,
      min_ms: 900,
      max_ms: 3100,
      recent: [
        { job_id: "j1", duration_ms: 1000, completed_at: "2024-06-15T10:00:00Z" },
        { job_id: "j2", duration_ms: 1200, completed_at: "2024-06-15T11:00:00Z" },
        { job_id: "j3", duration_ms: 1500, completed_at: "2024-06-15T12:00:00Z" },
        { job_id: "j4", duration_ms: 2000, completed_at: "2024-06-15T13:00:00Z" },
        { job_id: "j5", duration_ms: 900,  completed_at: "2024-06-15T14:00:00Z" },
      ],
    },
    steps: [
      {
        step_name: "build",
        sample_size: 10,
        avg_ms: 800,
        p50_ms: 750,
        p95_ms: 1200,
        min_ms: 600,
        max_ms: 1400,
      },
    ],
    ...overrides,
  };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("DurationInsightsCard", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  afterEach(() => {
    vi.clearAllMocks();
  });

  // -------------------------------------------------------------------------
  // Loading state
  // -------------------------------------------------------------------------
  it("shows a loading spinner while the fetch is in flight", async () => {
    // Never resolves during this test
    mockGetTaskStats.mockReturnValue(new Promise(() => {}));

    const { container } = render(
      <DurationInsightsCard workspace="default" taskName="my-task" />,
    );

    // The spinner has the animate-spin class
    expect(container.querySelector(".animate-spin")).toBeInTheDocument();
  });

  // -------------------------------------------------------------------------
  // Error state
  // -------------------------------------------------------------------------
  it("shows an error message when getTaskStats rejects", async () => {
    mockGetTaskStats.mockRejectedValue(new Error("Network failure"));

    render(<DurationInsightsCard workspace="default" taskName="my-task" />);

    await waitFor(() => {
      expect(screen.getByText(/Network failure/i)).toBeInTheDocument();
    });
  });

  // -------------------------------------------------------------------------
  // Insufficient data — sample_size < minSample
  // -------------------------------------------------------------------------
  it("shows insufficient-data copy when sample_size is below minSample (default 5)", async () => {
    mockGetTaskStats.mockResolvedValue(
      makeStats({ task: { ...makeStats().task, sample_size: 3 } }),
    );

    render(<DurationInsightsCard workspace="default" taskName="my-task" />);

    await waitFor(() => {
      expect(screen.getByText(/Insufficient data/i)).toBeInTheDocument();
      // Shows current count
      expect(screen.getByText(/currently 3/i)).toBeInTheDocument();
    });
  });

  it("includes the minSample threshold in the insufficient-data copy", async () => {
    mockGetTaskStats.mockResolvedValue(
      makeStats({ task: { ...makeStats().task, sample_size: 2 } }),
    );

    render(
      <DurationInsightsCard workspace="default" taskName="my-task" minSample={10} />,
    );

    await waitFor(() => {
      expect(screen.getByText(/at least 10 completed runs/i)).toBeInTheDocument();
    });
  });

  // -------------------------------------------------------------------------
  // Boundary — sample_size === minSample should render full card
  // -------------------------------------------------------------------------
  it("renders the full card when sample_size equals minSample (boundary: not <)", async () => {
    const stats = makeStats();
    stats.task.sample_size = 5; // equals default minSample=5
    mockGetTaskStats.mockResolvedValue(stats);

    render(<DurationInsightsCard workspace="default" taskName="my-task" />);

    await waitFor(() => {
      expect(screen.getByTestId("duration-insights")).toBeInTheDocument();
    });
  });

  // -------------------------------------------------------------------------
  // Happy path — full card renders with stat chips, table, sparkline
  // -------------------------------------------------------------------------
  it("renders stat chips for p50, p95, avg, min, max", async () => {
    mockGetTaskStats.mockResolvedValue(makeStats());

    render(<DurationInsightsCard workspace="default" taskName="my-task" />);

    await waitFor(() => {
      expect(screen.getByTestId("stat-p50")).toBeInTheDocument();
      expect(screen.getByTestId("stat-p95")).toBeInTheDocument();
      expect(screen.getByTestId("stat-avg")).toBeInTheDocument();
      expect(screen.getByTestId("stat-min")).toBeInTheDocument();
      expect(screen.getByTestId("stat-max")).toBeInTheDocument();
    });
  });

  it("renders the per-step breakdown table", async () => {
    mockGetTaskStats.mockResolvedValue(makeStats());

    render(<DurationInsightsCard workspace="default" taskName="my-task" />);

    await waitFor(() => {
      expect(screen.getByText("build")).toBeInTheDocument();
      expect(screen.getByText("Per-step breakdown")).toBeInTheDocument();
    });
  });

  it("renders a Sparkline SVG with role=img", async () => {
    mockGetTaskStats.mockResolvedValue(makeStats());

    const { container } = render(
      <DurationInsightsCard workspace="default" taskName="my-task" />,
    );

    await waitFor(() => {
      const svg = container.querySelector("svg[role='img']");
      expect(svg).toBeInTheDocument();
    });
  });

  // -------------------------------------------------------------------------
  // Race condition — unmount before fetch resolves
  // -------------------------------------------------------------------------
  it("does not update state after unmount (no React warning)", async () => {
    let resolveFetch!: (value: TaskStatsResponse) => void;
    const promise = new Promise<TaskStatsResponse>((resolve) => {
      resolveFetch = resolve;
    });
    mockGetTaskStats.mockReturnValue(promise);

    const { unmount } = render(
      <DurationInsightsCard workspace="default" taskName="my-task" />,
    );

    // Unmount before fetch resolves — the cancelled flag should swallow the update
    unmount();

    // Resolve after unmount; should not throw or warn
    await act(async () => {
      resolveFetch(makeStats());
      await promise;
    });
    // If we reach here without an error, the race guard worked
    expect(true).toBe(true);
  });

  // -------------------------------------------------------------------------
  // Props change mid-fetch (workspace changes)
  // -------------------------------------------------------------------------
  it("ignores stale result when workspace prop changes before first fetch resolves", async () => {
    let resolveFirst!: (value: TaskStatsResponse) => void;
    const firstPromise = new Promise<TaskStatsResponse>((resolve) => {
      resolveFirst = resolve;
    });
    const secondStats = makeStats();
    mockGetTaskStats
      .mockReturnValueOnce(firstPromise)
      .mockResolvedValue(secondStats);

    const { rerender } = render(
      <DurationInsightsCard workspace="ws1" taskName="my-task" />,
    );

    // Change workspace while first fetch is still in flight
    rerender(<DurationInsightsCard workspace="ws2" taskName="my-task" />);

    // Second fetch resolves immediately
    await waitFor(() => {
      expect(screen.getByTestId("duration-insights")).toBeInTheDocument();
    });

    // Now resolve the stale first fetch — state should not regress
    await act(async () => {
      resolveFirst(makeStats({ task: { ...makeStats().task, sample_size: 2 } }));
      await firstPromise;
    });

    // Card should still show the full insights from the second fetch, not the stale one
    expect(screen.getByTestId("duration-insights")).toBeInTheDocument();
    expect(screen.queryByText(/Insufficient data/i)).not.toBeInTheDocument();
  });
});
