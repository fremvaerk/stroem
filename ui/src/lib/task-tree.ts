import type { TaskListItem } from "./types";

/**
 * Pure helpers that turn a flat task list into the collapsible rows rendered
 * by `<TaskTree>`. Kept free of React so the grouping/expansion rules can be
 * unit-tested directly.
 */

export interface FolderNode {
  name: string;
  /** Folder path inside the workspace, e.g. `etl/nightly`. */
  fullPath: string;
  /** Expansion key — unique across workspaces when grouping, see `folderKey`. */
  key: string;
  children: FolderNode[];
  tasks: TaskListItem[];
}

export type TreeRow =
  | { kind: "workspace"; workspace: string; count: number; depth: 0 }
  | { kind: "folder"; node: FolderNode; depth: number }
  | { kind: "task"; task: TaskListItem; depth: number };

export interface BuildRowsOptions {
  /** Insert a top-level collapsible row per workspace. */
  groupByWorkspace: boolean;
  /** Expansion keys (`folderKey` / `workspaceKey`) that are currently open. */
  expanded: Set<string>;
  /** Workspace groups are open unless listed here. */
  collapsedWorkspaces: Set<string>;
  /** Free-text filter on the task name; when set, everything is expanded. */
  search: string;
}

/** Expansion key for a folder. Workspace-qualified only when grouping. */
export function folderKey(
  folderPath: string,
  workspace: string | null,
): string {
  return workspace === null ? folderPath : `${workspace}::${folderPath}`;
}

export function taskHref(
  task: Pick<TaskListItem, "workspace" | "id">,
): string {
  return `/workspaces/${encodeURIComponent(task.workspace)}/tasks/${encodeURIComponent(task.id)}`;
}

export function workspaceHref(workspace: string): string {
  return `/workspaces/${encodeURIComponent(workspace)}`;
}

export function workspaceKey(workspace: string): string {
  return `ws:${workspace}`;
}

export function countTasks(node: FolderNode): number {
  let count = node.tasks.length;
  for (const child of node.children) {
    count += countTasks(child);
  }
  return count;
}

function taskLabel(task: TaskListItem): string {
  return task.name ?? task.id;
}

function byName(a: { name: string }, b: { name: string }): number {
  return a.name.localeCompare(b.name);
}

function byTaskLabel(a: TaskListItem, b: TaskListItem): number {
  return taskLabel(a).localeCompare(taskLabel(b));
}

export function filterTasks(
  tasks: TaskListItem[],
  search: string,
): TaskListItem[] {
  const needle = search.trim().toLowerCase();
  if (!needle) return tasks;
  return tasks.filter((t) => taskLabel(t).toLowerCase().includes(needle));
}

/** Build the folder forest for one set of tasks (already scoped to a workspace when grouping). */
export function buildFolderTree(
  tasks: TaskListItem[],
  workspace: string | null,
): { rootTasks: TaskListItem[]; folders: FolderNode[] } {
  const rootTasks: TaskListItem[] = [];
  const folderMap = new Map<string, FolderNode>();

  function ensureFolder(path: string): FolderNode {
    const existing = folderMap.get(path);
    if (existing) return existing;

    const segments = path.split("/");
    const node: FolderNode = {
      name: segments[segments.length - 1],
      fullPath: path,
      key: folderKey(path, workspace),
      children: [],
      tasks: [],
    };
    folderMap.set(path, node);

    if (segments.length > 1) {
      const parent = ensureFolder(segments.slice(0, -1).join("/"));
      parent.children.push(node);
    }
    return node;
  }

  for (const task of tasks) {
    if (task.folder) {
      ensureFolder(task.folder).tasks.push(task);
    } else {
      rootTasks.push(task);
    }
  }

  const folders = [...folderMap.values()].filter(
    (n) => !n.fullPath.includes("/"),
  );
  folders.sort(byName);
  return { rootTasks, folders };
}

function walkForest(
  folders: FolderNode[],
  rootTasks: TaskListItem[],
  baseDepth: number,
  isOpen: (key: string) => boolean,
  rows: TreeRow[],
) {
  function walkFolder(node: FolderNode, depth: number) {
    rows.push({ kind: "folder", node, depth });
    if (!isOpen(node.key)) return;
    for (const child of [...node.children].sort(byName)) {
      walkFolder(child, depth + 1);
    }
    for (const task of [...node.tasks].sort(byTaskLabel)) {
      rows.push({ kind: "task", task, depth: depth + 1 });
    }
  }

  for (const folder of folders) walkFolder(folder, baseDepth);
  for (const task of [...rootTasks].sort(byTaskLabel)) {
    rows.push({ kind: "task", task, depth: baseDepth });
  }
}

/**
 * Flatten `tasks` into table rows. Sorting is alphabetical at every level;
 * folders precede loose tasks. With `groupByWorkspace`, each workspace gets
 * a depth-0 row and its folders/tasks are indented one level beneath it.
 */
export function buildRows(
  tasks: TaskListItem[],
  opts: BuildRowsOptions,
): TreeRow[] {
  const searching = opts.search.trim().length > 0;
  const filtered = filterTasks(tasks, opts.search);
  const rows: TreeRow[] = [];
  const isOpen = (key: string) => searching || opts.expanded.has(key);

  if (!opts.groupByWorkspace) {
    const { rootTasks, folders } = buildFolderTree(filtered, null);
    walkForest(folders, rootTasks, 0, isOpen, rows);
    return rows;
  }

  const byWorkspace = new Map<string, TaskListItem[]>();
  for (const task of filtered) {
    const list = byWorkspace.get(task.workspace);
    if (list) list.push(task);
    else byWorkspace.set(task.workspace, [task]);
  }

  for (const workspace of [...byWorkspace.keys()].sort()) {
    const wsTasks = byWorkspace.get(workspace)!;
    rows.push({ kind: "workspace", workspace, count: wsTasks.length, depth: 0 });
    if (!searching && opts.collapsedWorkspaces.has(workspace)) continue;
    const { rootTasks, folders } = buildFolderTree(wsTasks, workspace);
    walkForest(folders, rootTasks, 1, isOpen, rows);
  }
  return rows;
}
