import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router";
import type { TaskListItem, TriggerInfo, WorkspaceInfo } from "@/lib/types";

vi.mock("@/lib/api", () => ({
  listWorkspaces: vi.fn(),
  listTasks: vi.fn(),
  listTriggers: vi.fn(),
  refreshWorkspace: vi.fn(),
}));

import {
  listWorkspaces,
  listTasks,
  listTriggers,
  refreshWorkspace,
} from "@/lib/api";
import { WorkspaceDetailPage } from "../workspace-detail";

const mockWs = vi.mocked(listWorkspaces);
const mockTasks = vi.mocked(listTasks);
const mockTriggers = vi.mocked(listTriggers);
const mockRefresh = vi.mocked(refreshWorkspace);

function ws(overrides: Partial<WorkspaceInfo> = {}): WorkspaceInfo {
  return {
    name: "iso",
    tasks_count: 2,
    actions_count: 4,
    triggers_count: 1,
    triggers_enabled: true,
    revision: "abcdef1234567890",
    ...overrides,
  };
}

function task(id: string, folder?: string): TaskListItem {
  return { id, mode: "manual", workspace: "iso", folder, has_triggers: false };
}

const trigger: TriggerInfo = {
  name: "daily",
  type: "scheduler",
  cron: "0 6 * * *",
  task: "cleanup",
  enabled: true,
  input: {},
  next_runs: ["2026-09-10T06:00:00Z"],
};

function renderAt(path: string) {
  return render(
    <MemoryRouter initialEntries={[path]}>
      <Routes>
        <Route path="/workspaces/:workspace" element={<WorkspaceDetailPage />} />
      </Routes>
    </MemoryRouter>,
  );
}

beforeEach(() => {
  vi.clearAllMocks();
  localStorage.clear();
  mockWs.mockResolvedValue([ws({ name: "default" }), ws()]);
  mockTasks.mockResolvedValue([task("cleanup"), task("daily", "reports")]);
  mockTriggers.mockResolvedValue([trigger]);
});

describe("WorkspaceDetailPage", () => {
  it("renders header, task tree and triggers for the workspace only", async () => {
    renderAt("/workspaces/iso");
    expect(await screen.findByRole("heading", { name: "iso" })).toBeInTheDocument();
    expect(mockTasks).toHaveBeenCalledWith("iso");
    expect(mockTriggers).toHaveBeenCalledWith("iso");
    expect(screen.getByText("abcdef12")).toBeInTheDocument();
    expect(screen.getByText("2 tasks")).toBeInTheDocument();
    expect(screen.getByText("reports")).toBeInTheDocument();
    expect(screen.getByText("cleanup", { selector: "a.font-medium" })).toHaveAttribute(
      "href",
      "/workspaces/iso/tasks/cleanup",
    );
    expect(screen.queryByText("Workspace", { selector: "th" })).toBeNull();
    expect(screen.getByText("daily")).toBeInTheDocument();
    expect(screen.getByText("0 6 * * *")).toBeInTheDocument();
  });

  it("decodes the workspace name from the URL", async () => {
    mockWs.mockResolvedValue([ws({ name: "my ws" })]);
    renderAt("/workspaces/my%20ws");
    await screen.findByRole("heading", { name: "my ws" });
    expect(mockTasks).toHaveBeenCalledWith("my ws");
  });

  it("shows not found for an unknown workspace without listing tasks", async () => {
    renderAt("/workspaces/nope");
    expect(await screen.findByText(/Workspace "nope" not found/)).toBeInTheDocument();
    expect(screen.getByText("Back to workspaces")).toHaveAttribute("href", "/workspaces");
    expect(mockTasks).not.toHaveBeenCalled();
  });

  it("shows load error, warnings and the triggers-off badge", async () => {
    mockWs.mockResolvedValue([
      ws({ error: "clone failed", warnings: ["dup action"], triggers_enabled: false }),
    ]);
    mockTasks.mockRejectedValue(new Error("workspace not loaded"));
    renderAt("/workspaces/iso");
    expect(await screen.findByText("clone failed")).toBeInTheDocument();
    expect(screen.getByText("dup action")).toBeInTheDocument();
    expect(screen.getByText("off")).toBeInTheDocument();
    expect(screen.getAllByText("workspace not loaded")).toHaveLength(2);
  });

  it("refreshes the workspace and refetches", async () => {
    mockRefresh.mockResolvedValue({ workspace: "iso", revision: "x", refreshed: true });
    renderAt("/workspaces/iso");
    const button = await screen.findByRole("button", { name: /refresh workspace iso/i });
    expect(mockWs).toHaveBeenCalledTimes(1);
    fireEvent.click(button);
    await waitFor(() => expect(mockRefresh).toHaveBeenCalledWith("iso"));
    await waitFor(() => expect(mockWs).toHaveBeenCalledTimes(2));
  });

  it("reports a failed refresh inline", async () => {
    mockRefresh.mockRejectedValue(new Error("rate-limited"));
    renderAt("/workspaces/iso");
    fireEvent.click(await screen.findByRole("button", { name: /refresh workspace iso/i }));
    expect(await screen.findByText(/Refresh failed: rate-limited/)).toBeInTheDocument();
  });
});
