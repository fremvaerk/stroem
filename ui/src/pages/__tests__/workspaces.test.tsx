import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor, fireEvent } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import type { WorkspaceInfo } from "@/lib/types";

vi.mock("@/lib/api", () => ({
  listWorkspaces: vi.fn(),
  refreshWorkspace: vi.fn(),
}));

import { listWorkspaces, refreshWorkspace } from "@/lib/api";
import { WorkspacesPage } from "../workspaces";

const mockList = vi.mocked(listWorkspaces);
const mockRefresh = vi.mocked(refreshWorkspace);

function ws(overrides: Partial<WorkspaceInfo> = {}): WorkspaceInfo {
  return {
    name: "default",
    tasks_count: 3,
    actions_count: 5,
    triggers_count: 0,
    triggers_enabled: true,
    revision: "abcdef1234567890",
    ...overrides,
  };
}

function renderPage() {
  return render(
    <MemoryRouter>
      <WorkspacesPage />
    </MemoryRouter>,
  );
}

describe("WorkspacesPage triggers column", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("shows the trigger count and no badge when triggers are enabled", async () => {
    mockList.mockResolvedValue([ws({ triggers_count: 2, triggers_enabled: true })]);
    renderPage();
    await screen.findByText("default");
    expect(screen.getByText("2")).toBeInTheDocument();
    expect(screen.queryByText("off")).not.toBeInTheDocument();
  });

  it("shows an 'off' badge when the server config disables triggers", async () => {
    mockList.mockResolvedValue([
      ws({ name: "staging", triggers_count: 4, triggers_enabled: false }),
    ]);
    renderPage();
    await screen.findByText("staging");
    expect(screen.getByText("4")).toBeInTheDocument();
    const badge = screen.getByText("off");
    expect(badge).toBeInTheDocument();
    expect(badge.getAttribute("title")).toMatch(/triggers: false/);
  });
});

describe("WorkspacesPage refresh button", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mockList.mockResolvedValue([ws()]);
  });

  it("calls refreshWorkspace with the workspace name on click", async () => {
    mockRefresh.mockResolvedValue({
      workspace: "default",
      revision: "newrev",
      refreshed: true,
    });
    renderPage();

    const button = await screen.findByRole("button", {
      name: /refresh workspace default/i,
    });
    fireEvent.click(button);

    await waitFor(() => {
      expect(mockRefresh).toHaveBeenCalledWith("default");
    });
  });

  it("refetches the workspace list after a successful refresh", async () => {
    mockRefresh.mockResolvedValue({
      workspace: "default",
      revision: "newrev",
      refreshed: true,
    });
    renderPage();

    // Initial fetch on mount.
    await waitFor(() => expect(mockList).toHaveBeenCalledTimes(1));

    const button = await screen.findByRole("button", {
      name: /refresh workspace default/i,
    });
    fireEvent.click(button);

    // listWorkspaces must be invoked a second time after refresh resolves.
    await waitFor(() => expect(mockList).toHaveBeenCalledTimes(2));
  });

  it("renders an inline error when refresh fails", async () => {
    mockRefresh.mockRejectedValue(new Error("rate-limited; retry after 4s"));
    renderPage();

    const button = await screen.findByRole("button", {
      name: /refresh workspace default/i,
    });
    fireEvent.click(button);

    expect(
      await screen.findByText(/refresh failed:.*rate-limited/i),
    ).toBeInTheDocument();
  });

  it("clears the previous error on the next refresh attempt", async () => {
    mockRefresh.mockRejectedValueOnce(new Error("transient failure"));
    mockRefresh.mockResolvedValueOnce({
      workspace: "default",
      revision: "newrev",
      refreshed: true,
    });
    renderPage();

    const button = await screen.findByRole("button", {
      name: /refresh workspace default/i,
    });
    fireEvent.click(button);
    expect(await screen.findByText(/transient failure/i)).toBeInTheDocument();

    fireEvent.click(button);
    await waitFor(() => {
      expect(screen.queryByText(/transient failure/i)).not.toBeInTheDocument();
    });
  });
});
