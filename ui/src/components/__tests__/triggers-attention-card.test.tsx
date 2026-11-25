import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
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

  it("links the last run failed badge to the failed job", async () => {
    mockList.mockResolvedValue([
      item({
        last_run_failed: true,
        last_run: {
          job_id: "j9",
          status: "failed",
          created_at: "2026-11-25T02:00:00Z",
        },
      }),
    ]);
    renderCard();
    const badge = await screen.findByText("last run failed");
    const link = badge.closest("a");
    expect(link).not.toBeNull();
    expect(link).toHaveAttribute("href", "/jobs/j9");
    expect(link).toHaveAttribute("title", "j9");
    const row = screen.getAllByRole("row")[1];
    const jobLinks = within(row)
      .getAllByRole("link")
      .filter((a) => a.getAttribute("href") === "/jobs/j9");
    expect(jobLinks).toHaveLength(2);
  });

  it("renders the last run failed badge without a link when last_run is null", async () => {
    mockList.mockResolvedValue([
      item({ last_run_failed: true, last_run: null }),
    ]);
    renderCard();
    const badge = await screen.findByText("last run failed");
    expect(badge.closest("a")).toBeNull();
    const row = screen.getAllByRole("row")[1];
    expect(
      within(row)
        .getAllByRole("link")
        .filter((a) => a.getAttribute("href")?.startsWith("/jobs/")),
    ).toHaveLength(0);
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
