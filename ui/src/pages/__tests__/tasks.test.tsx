import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import type { TaskListItem } from "@/lib/types";

vi.mock("@/lib/api", () => ({
  listAllTasks: vi.fn(),
}));

import { listAllTasks } from "@/lib/api";
import { TasksPage } from "../tasks";

const mockList = vi.mocked(listAllTasks);

function task(id: string, workspace: string, folder?: string): TaskListItem {
  return { id, mode: "manual", workspace, folder, has_triggers: false };
}

function renderPage() {
  return render(
    <MemoryRouter>
      <TasksPage />
    </MemoryRouter>,
  );
}

beforeEach(() => {
  vi.clearAllMocks();
  localStorage.clear();
});

describe("TasksPage layout toggle", () => {
  it("hides the toggle and workspace column with a single workspace", async () => {
    mockList.mockResolvedValue([task("a", "default"), task("b", "default")]);
    renderPage();
    await screen.findByText("a");
    expect(screen.queryByRole("tab", { name: "By workspace" })).toBeNull();
    expect(screen.queryByText("Workspace")).toBeNull();
  });

  it("shows merged rows with workspace badges by default", async () => {
    mockList.mockResolvedValue([task("a", "default"), task("b", "iso")]);
    renderPage();
    await screen.findByText("a");
    expect(screen.getByRole("tab", { name: "Merged" })).toHaveAttribute(
      "aria-selected",
      "true",
    );
    expect(screen.getByText("Workspace")).toBeInTheDocument();
    expect(screen.getByText("iso").closest("a")).toHaveAttribute(
      "href",
      "/workspaces/iso",
    );
  });

  it("groups by workspace when toggled and remembers the choice", async () => {
    mockList.mockResolvedValue([
      task("a", "default", "etl"),
      task("b", "iso", "etl"),
    ]);
    renderPage();
    await screen.findByText("etl");
    fireEvent.mouseDown(screen.getByRole("tab", { name: "By workspace" }), {
      button: 0,
    });

    const rows = screen.getAllByRole("row").map((r) => r.textContent ?? "");
    // header, default group, its etl folder, iso group, its etl folder
    expect(rows.slice(1)).toEqual(["default1", "etl1", "iso1", "etl1"]);
    expect(screen.queryByText("Workspace")).toBeNull();
    expect(screen.getByText("default").closest("a")).toHaveAttribute(
      "href",
      "/workspaces/default",
    );
    expect(localStorage.getItem("stroem_tasks_view")).toBe("workspace");
  });

  it("restores the grouped layout from localStorage", async () => {
    localStorage.setItem("stroem_tasks_view", "workspace");
    mockList.mockResolvedValue([task("a", "default"), task("b", "iso")]);
    renderPage();
    await screen.findByText("a");
    expect(screen.getByRole("tab", { name: "By workspace" })).toHaveAttribute(
      "aria-selected",
      "true",
    );
    expect(screen.getAllByRole("row")).toHaveLength(5);
  });

  it("collapsing a workspace group hides its tasks and persists", async () => {
    localStorage.setItem("stroem_tasks_view", "workspace");
    mockList.mockResolvedValue([task("a", "default"), task("b", "iso")]);
    renderPage();
    const groupRow = (await screen.findByText("default")).closest("tr")!;
    fireEvent.click(groupRow);
    expect(screen.queryByText("a")).toBeNull();
    expect(screen.getByText("b")).toBeInTheDocument();
    expect(
      JSON.parse(localStorage.getItem("stroem_tasks_collapsed_workspaces")!),
    ).toEqual(["default"]);
  });

  it("search expands collapsed groups and folders", async () => {
    localStorage.setItem("stroem_tasks_view", "workspace");
    localStorage.setItem(
      "stroem_tasks_collapsed_workspaces",
      JSON.stringify(["default"]),
    );
    mockList.mockResolvedValue([
      task("deploy", "default", "etl"),
      task("b", "iso"),
    ]);
    renderPage();
    await screen.findByText("b");
    expect(screen.queryByText("deploy")).toBeNull();
    fireEvent.change(screen.getByPlaceholderText("Search tasks..."), {
      target: { value: "dep" },
    });
    expect(screen.getByText("deploy")).toBeInTheDocument();
    expect(screen.queryByText("b")).toBeNull();
    expect(
      screen.getByRole("button", { name: /collapse folder etl/i }),
    ).toHaveAttribute("aria-expanded", "true");
  });

  it("folders and workspace groups toggle from the keyboard", async () => {
    localStorage.setItem("stroem_tasks_view", "workspace");
    mockList.mockResolvedValue([task("a", "default", "etl"), task("b", "iso")]);
    renderPage();
    const folderButton = await screen.findByRole("button", {
      name: /expand folder etl/i,
    });
    expect(screen.queryByText("a")).toBeNull();
    folderButton.focus();
    fireEvent.click(folderButton); // native buttons fire click on Enter/Space
    expect(screen.getByText("a")).toBeInTheDocument();
    // toggling via the button must not double-toggle through the row handler
    expect(
      JSON.parse(localStorage.getItem("stroem_tasks_expanded_folders")!),
    ).toEqual(["default::etl"]);

    fireEvent.click(
      screen.getByRole("button", { name: /collapse workspace default/i }),
    );
    expect(screen.queryByText("a")).toBeNull();
    expect(screen.queryByText("etl")).toBeNull();
  });

  it("shows the API error", async () => {
    mockList.mockRejectedValue(new Error("boom"));
    renderPage();
    expect(await screen.findByText("boom")).toBeInTheDocument();
  });
});
