import { describe, it, expect, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router";

vi.mock("@/lib/api", () => ({
  getStats: vi.fn().mockResolvedValue({
    pending: 0,
    running: 0,
    completed: 1,
    failed: 0,
    cancelled: 0,
    skipped: 0,
  }),
  listJobs: vi.fn().mockResolvedValue({ items: [], total: 0 }),
  listAttentionTriggers: vi.fn().mockRejectedValue(new Error("boom")),
}));

import { DashboardPage } from "../dashboard";

describe("DashboardPage", () => {
  it("still shows Recent Jobs when the attention call fails", async () => {
    render(
      <MemoryRouter>
        <DashboardPage />
      </MemoryRouter>,
    );
    expect(await screen.findByText("Recent Jobs")).toBeInTheDocument();
    expect(
      screen.queryByText("Triggers that need attention"),
    ).not.toBeInTheDocument();
  });
});
