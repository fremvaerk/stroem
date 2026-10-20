import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import type { AttentionTrigger } from "@/lib/types";

vi.mock("@/lib/api", () => ({
  listAttentionTriggers: vi.fn(),
}));

import { listAttentionTriggers } from "@/lib/api";
import { TriggersAttentionCard } from "../triggers-attention-card";

const mockList = vi.mocked(listAttentionTriggers);

function item(overrides: Partial<AttentionTrigger> = {}): AttentionTrigger {
  return {
    name: "daily",
    type: "scheduler",
    cron: "0 6 * * *",
    task: "cleanup",
    enabled: true,
    input: {},
    next_runs: [],
    workspace: "iso",
    last_run_failed: false,
    ...overrides,
  };
}

function renderCard() {
  return render(
    <MemoryRouter>
      <TriggersAttentionCard />
    </MemoryRouter>,
  );
}

describe("TriggersAttentionCard", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("renders a row per item with both problem badges", async () => {
    mockList.mockResolvedValue([
      item({
        missed: { count: 3, first: "2026-10-19T06:00:00Z", capped: false },
        last_run_failed: true,
        last_run: {
          job_id: "j1",
          status: "failed",
          created_at: "2026-10-18T06:00:00Z",
        },
      }),
      item({ name: "weekly", workspace: "other ws", last_run_failed: true }),
    ]);
    renderCard();
    expect(
      await screen.findByText("Triggers that need attention"),
    ).toBeInTheDocument();
    expect(screen.getByText(/3 missed/)).toBeInTheDocument();
    expect(screen.getAllByText("last run failed")).toHaveLength(2);
    expect(screen.getByText("weekly")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "other ws" })).toHaveAttribute(
      "href",
      "/workspaces/other%20ws",
    );
    expect(screen.getAllByRole("row")).toHaveLength(3);
  });

  it("renders nothing for an empty list", async () => {
    mockList.mockResolvedValue([]);
    const { container } = renderCard();
    await waitFor(() => expect(mockList).toHaveBeenCalled());
    expect(container).toBeEmptyDOMElement();
  });

  it("renders nothing when the call rejects", async () => {
    mockList.mockRejectedValue(new Error("boom"));
    const { container } = renderCard();
    await waitFor(() => expect(mockList).toHaveBeenCalled());
    expect(container).toBeEmptyDOMElement();
  });
});
