import { describe, it, expect } from "vitest";
import { buildRows, folderKey, workspaceKey, countTasks, buildFolderTree } from "../task-tree";
import type { TaskListItem } from "../types";

function task(id: string, workspace: string, folder?: string): TaskListItem {
  return { id, mode: "manual", workspace, folder, has_triggers: false };
}

const TASKS: TaskListItem[] = [
  task("deploy", "default"),
  task("weekly", "default", "etl"),
  task("nightly", "default", "etl/batch"),
  task("cleanup", "iso"),
  task("daily", "iso", "etl"),
];

const noOpts = {
  expanded: new Set<string>(),
  collapsedWorkspaces: new Set<string>(),
  search: "",
};

function shape(rows: ReturnType<typeof buildRows>) {
  return rows.map((r) =>
    r.kind === "workspace"
      ? `ws:${r.workspace}(${r.count})@${r.depth}`
      : r.kind === "folder"
        ? `dir:${r.node.key}@${r.depth}`
        : `task:${r.task.workspace}/${r.task.id}@${r.depth}`,
  );
}

describe("buildRows merged", () => {
  it("shows collapsed top-level folders before loose tasks, sorted", () => {
    expect(shape(buildRows(TASKS, { ...noOpts, groupByWorkspace: false }))).toEqual([
      "dir:etl@0",
      "task:iso/cleanup@0",
      "task:default/deploy@0",
    ]);
  });

  it("merges same-named folders across workspaces and expands by bare path", () => {
    const rows = buildRows(TASKS, {
      ...noOpts,
      groupByWorkspace: false,
      expanded: new Set(["etl"]),
    });
    expect(shape(rows)).toEqual([
      "dir:etl@0",
      "dir:etl/batch@1",
      "task:iso/daily@1",
      "task:default/weekly@1",
      "task:iso/cleanup@0",
      "task:default/deploy@0",
    ]);
  });

  it("returns no rows for an empty list", () => {
    expect(buildRows([], { ...noOpts, groupByWorkspace: false })).toEqual([]);
  });
});

describe("buildRows grouped by workspace", () => {
  it("emits one open workspace row per workspace with nested folders", () => {
    const rows = buildRows(TASKS, { ...noOpts, groupByWorkspace: true });
    expect(shape(rows)).toEqual([
      "ws:default(3)@0",
      "dir:default::etl@1",
      "task:default/deploy@1",
      "ws:iso(2)@0",
      "dir:iso::etl@1",
      "task:iso/cleanup@1",
    ]);
  });

  it("keeps folder expansion independent per workspace", () => {
    const rows = buildRows(TASKS, {
      ...noOpts,
      groupByWorkspace: true,
      expanded: new Set([folderKey("etl", "iso")]),
    });
    expect(shape(rows)).toContain("task:iso/daily@2");
    expect(shape(rows)).not.toContain("task:default/weekly@2");
  });

  it("hides a collapsed workspace's contents but keeps its header", () => {
    const rows = buildRows(TASKS, {
      ...noOpts,
      groupByWorkspace: true,
      collapsedWorkspaces: new Set(["default"]),
    });
    expect(shape(rows)).toEqual([
      "ws:default(3)@0",
      "ws:iso(2)@0",
      "dir:iso::etl@1",
      "task:iso/cleanup@1",
    ]);
  });
});

describe("buildRows search", () => {
  it("filters by name case-insensitively and expands everything", () => {
    const rows = buildRows(TASKS, {
      ...noOpts,
      groupByWorkspace: true,
      collapsedWorkspaces: new Set(["default"]),
      search: "NIGHT",
    });
    expect(shape(rows)).toEqual([
      "ws:default(1)@0",
      "dir:default::etl@1",
      "dir:default::etl/batch@2",
      "task:default/nightly@3",
    ]);
  });

  it("matches on display name when present", () => {
    const named = [{ ...task("x", "default"), name: "Pretty Name" }];
    expect(buildRows(named, { ...noOpts, groupByWorkspace: false, search: "pretty" })).toHaveLength(1);
    expect(buildRows(named, { ...noOpts, groupByWorkspace: false, search: "x" })).toHaveLength(0);
  });

  it("drops workspaces with no matching task", () => {
    const rows = buildRows(TASKS, { ...noOpts, groupByWorkspace: true, search: "cleanup" });
    expect(shape(rows)).toEqual(["ws:iso(1)@0", "task:iso/cleanup@1"]);
  });
});

describe("helpers", () => {
  it("countTasks counts nested tasks", () => {
    const { folders } = buildFolderTree(TASKS.filter((t) => t.workspace === "default"), null);
    expect(countTasks(folders[0])).toBe(2);
  });

  it("keys are distinct between merged and grouped mode", () => {
    expect(folderKey("etl", null)).toBe("etl");
    expect(folderKey("etl", "iso")).toBe("iso::etl");
    expect(workspaceKey("iso")).toBe("ws:iso");
  });
});
