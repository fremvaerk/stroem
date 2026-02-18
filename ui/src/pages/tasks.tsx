import { useEffect, useState } from "react";
import { Link } from "react-router";
import { ChevronRight, Clock, Folder } from "lucide-react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Badge } from "@/components/ui/badge";
import { listAllTasks } from "@/lib/api";
import { useTitle } from "@/hooks/use-title";
import type { TaskListItem } from "@/lib/types";

interface FolderNode {
  name: string;
  fullPath: string;
  children: FolderNode[];
  tasks: TaskListItem[];
}

function buildFolderTree(tasks: TaskListItem[]): {
  rootTasks: TaskListItem[];
  folders: FolderNode[];
} {
  const rootTasks: TaskListItem[] = [];
  const folderMap = new Map<string, FolderNode>();

  function ensureFolder(path: string): FolderNode {
    const existing = folderMap.get(path);
    if (existing) return existing;

    const segments = path.split("/");
    const name = segments[segments.length - 1];
    const node: FolderNode = { name, fullPath: path, children: [], tasks: [] };
    folderMap.set(path, node);

    if (segments.length > 1) {
      const parentPath = segments.slice(0, -1).join("/");
      const parent = ensureFolder(parentPath);
      if (!parent.children.find((c) => c.fullPath === path)) {
        parent.children.push(node);
      }
    }

    return node;
  }

  for (const task of tasks) {
    if (task.folder) {
      const folder = ensureFolder(task.folder);
      folder.tasks.push(task);
    } else {
      rootTasks.push(task);
    }
  }

  const topFolders: FolderNode[] = [];
  for (const [path, node] of folderMap) {
    const segments = path.split("/");
    if (segments.length === 1) {
      topFolders.push(node);
    }
  }

  topFolders.sort((a, b) => a.name.localeCompare(b.name));
  return { rootTasks, folders: topFolders };
}

function countTasks(node: FolderNode): number {
  let count = node.tasks.length;
  for (const child of node.children) {
    count += countTasks(child);
  }
  return count;
}

/** Flatten the folder tree into table rows with depth info. */
type TreeRow =
  | { kind: "folder"; node: FolderNode; depth: number }
  | { kind: "task"; task: TaskListItem; depth: number };

function flattenTree(
  folders: FolderNode[],
  rootTasks: TaskListItem[],
  expanded: Set<string>,
): TreeRow[] {
  const rows: TreeRow[] = [];

  function walkFolder(node: FolderNode, depth: number) {
    rows.push({ kind: "folder", node, depth });
    if (expanded.has(node.fullPath)) {
      const sortedChildren = [...node.children].sort((a, b) =>
        a.name.localeCompare(b.name),
      );
      for (const child of sortedChildren) {
        walkFolder(child, depth + 1);
      }
      for (const task of node.tasks) {
        rows.push({ kind: "task", task, depth: depth + 1 });
      }
    }
  }

  for (const folder of folders) {
    walkFolder(folder, 0);
  }
  for (const task of rootTasks) {
    rows.push({ kind: "task", task, depth: 0 });
  }

  return rows;
}

/** Collect all folder paths for initial expanded state. */
function allFolderPaths(folders: FolderNode[]): string[] {
  const paths: string[] = [];
  function walk(node: FolderNode) {
    paths.push(node.fullPath);
    for (const child of node.children) {
      walk(child);
    }
  }
  for (const folder of folders) {
    walk(folder);
  }
  return paths;
}

export function TasksPage() {
  useTitle("Tasks");
  const [tasks, setTasks] = useState<TaskListItem[]>([]);
  const [loading, setLoading] = useState(true);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());

  useEffect(() => {
    let cancelled = false;
    async function load() {
      try {
        const data = await listAllTasks();
        if (!cancelled) {
          setTasks(data);
          // Expand all folders by default
          const { folders } = buildFolderTree(data);
          setExpanded(new Set(allFolderPaths(folders)));
        }
      } catch {
        // ignore
      } finally {
        if (!cancelled) setLoading(false);
      }
    }
    load();
    return () => {
      cancelled = true;
    };
  }, []);

  if (loading) {
    return (
      <div className="flex items-center justify-center py-20">
        <div className="h-8 w-8 animate-spin rounded-full border-4 border-muted border-t-primary" />
      </div>
    );
  }

  const workspaces = new Set(tasks.map((t) => t.workspace));
  const showWorkspace = workspaces.size > 1;
  const { rootTasks, folders } = buildFolderTree(tasks);
  const rows = flattenTree(folders, rootTasks, expanded);

  function toggleFolder(path: string) {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(path)) {
        next.delete(path);
      } else {
        next.add(path);
      }
      return next;
    });
  }

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">Tasks</h1>
        <p className="text-sm text-muted-foreground">
          Available workflow tasks
        </p>
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">All Tasks</CardTitle>
        </CardHeader>
        <CardContent>
          {tasks.length === 0 ? (
            <p className="py-8 text-center text-sm text-muted-foreground">
              No tasks found.
            </p>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Name</TableHead>
                  {showWorkspace && <TableHead>Workspace</TableHead>}
                  <TableHead>Mode</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {rows.map((row) => {
                  if (row.kind === "folder") {
                    const { node, depth } = row;
                    const isOpen = expanded.has(node.fullPath);
                    const total = countTasks(node);
                    return (
                      <TableRow
                        key={`folder:${node.fullPath}`}
                        className="cursor-pointer hover:bg-muted/50"
                        onClick={() => toggleFolder(node.fullPath)}
                      >
                        <TableCell colSpan={showWorkspace ? 3 : 2}>
                          <div
                            className="flex items-center gap-1.5"
                            style={{ paddingLeft: `${depth * 1.25}rem` }}
                          >
                            <ChevronRight
                              className={`h-4 w-4 shrink-0 text-muted-foreground transition-transform duration-200 ${isOpen ? "rotate-90" : ""}`}
                            />
                            <Folder className="h-4 w-4 shrink-0 text-muted-foreground" />
                            <span className="font-medium">{node.name}</span>
                            <Badge variant="secondary" className="ml-1 text-xs">
                              {total}
                            </Badge>
                          </div>
                        </TableCell>
                      </TableRow>
                    );
                  }

                  const { task, depth } = row;
                  return (
                    <TableRow key={`task:${task.workspace}/${task.name}`}>
                      <TableCell>
                        <div
                          className="flex items-center gap-1.5"
                          style={{ paddingLeft: `${depth * 1.25 + 2.75}rem` }}
                        >
                          <Link
                            to={`/workspaces/${encodeURIComponent(task.workspace)}/tasks/${encodeURIComponent(task.name)}`}
                            className="font-medium hover:underline"
                          >
                            {task.name}
                          </Link>
                          {task.has_triggers && (
                            <Clock
                              className="h-3.5 w-3.5 text-muted-foreground"
                              title="Has scheduled triggers"
                            />
                          )}
                        </div>
                      </TableCell>
                      {showWorkspace && (
                        <TableCell>
                          <Badge variant="secondary" className="font-mono text-xs">
                            {task.workspace}
                          </Badge>
                        </TableCell>
                      )}
                      <TableCell>
                        <Badge variant="outline" className="font-mono text-xs">
                          {task.mode}
                        </Badge>
                      </TableCell>
                    </TableRow>
                  );
                })}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
