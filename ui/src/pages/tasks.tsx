import { useEffect, useState } from "react";
import { Link } from "react-router";
import { ChevronRight, Clock, Folder, Search, X } from "lucide-react";
import { LoadingSpinner } from "@/components/loading-spinner";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Badge } from "@/components/ui/badge";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
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
      const sortedTasks = [...node.tasks].sort((a, b) =>
        (a.name ?? a.id).localeCompare(b.name ?? b.id),
      );
      for (const task of sortedTasks) {
        rows.push({ kind: "task", task, depth: depth + 1 });
      }
    }
  }

  for (const folder of folders) {
    walkFolder(folder, 0);
  }
  const sortedRootTasks = [...rootTasks].sort((a, b) =>
    (a.name ?? a.id).localeCompare(b.name ?? b.id),
  );
  for (const task of sortedRootTasks) {
    rows.push({ kind: "task", task, depth: 0 });
  }

  return rows;
}

export function TasksPage() {
  useTitle("Tasks");
  const [tasks, setTasks] = useState<TaskListItem[]>([]);
  const [loading, setLoading] = useState(true);
  const [search, setSearch] = useState("");
  const [expanded, setExpanded] = useState<Set<string>>(() => {
    try {
      const saved = localStorage.getItem("stroem_tasks_expanded_folders");
      if (saved) return new Set(JSON.parse(saved));
    } catch {
      // ignore corrupt localStorage
    }
    return new Set();
  });

  useEffect(() => {
    let cancelled = false;
    async function load() {
      try {
        const data = await listAllTasks();
        if (!cancelled) {
          setTasks(data);
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
    return <LoadingSpinner />;
  }

  const filteredTasks = search
    ? tasks.filter((t) =>
        (t.name ?? t.id).toLowerCase().includes(search.toLowerCase()),
      )
    : tasks;

  const workspaces = new Set(tasks.map((t) => t.workspace));
  const showWorkspace = workspaces.size > 1;
  const { rootTasks, folders } = buildFolderTree(filteredTasks);

  // When searching, collect all folder paths that contain matching tasks so
  // they are automatically expanded regardless of the persisted toggle state.
  const searchExpanded: Set<string> = new Set();
  if (search) {
    for (const task of filteredTasks) {
      if (task.folder) {
        const segments = task.folder.split("/");
        for (let i = 1; i <= segments.length; i++) {
          searchExpanded.add(segments.slice(0, i).join("/"));
        }
      }
    }
  }
  const effectiveExpanded = search ? searchExpanded : expanded;

  const rows = flattenTree(folders, rootTasks, effectiveExpanded);

  function toggleFolder(path: string) {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(path)) {
        next.delete(path);
      } else {
        next.add(path);
      }
      localStorage.setItem(
        "stroem_tasks_expanded_folders",
        JSON.stringify([...next]),
      );
      return next;
    });
  }

  return (
    <div className="space-y-6">
      <div className="flex items-start justify-between gap-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Tasks</h1>
          <p className="text-sm text-muted-foreground">
            Available workflow tasks
          </p>
        </div>
        <div className="relative">
          <Search className="absolute left-3 top-1/2 -translate-y-1/2 h-4 w-4 text-muted-foreground" />
          <Input
            placeholder="Search tasks..."
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            className="pl-9 max-w-sm"
          />
          {search && (
            <button
              onClick={() => setSearch("")}
              className="absolute right-3 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
              aria-label="Clear search"
            >
              <X className="h-4 w-4" />
            </button>
          )}
        </div>
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
          ) : filteredTasks.length === 0 ? (
            <p className="py-8 text-center text-sm text-muted-foreground">
              No tasks match &ldquo;{search}&rdquo;.
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
                    <TableRow key={`task:${task.workspace}/${task.id}`}>
                      <TableCell>
                        <div
                          style={{ paddingLeft: `${depth * 1.25 + 2.75}rem` }}
                        >
                          <div className="flex items-center gap-1.5">
                            <Link
                              to={`/workspaces/${encodeURIComponent(task.workspace)}/tasks/${encodeURIComponent(task.id)}`}
                              className="font-medium hover:underline"
                            >
                              {task.name ?? task.id}
                            </Link>
                            {task.has_triggers && (
                              <Tooltip>
                                <TooltipTrigger asChild>
                                  <span>
                                    <Clock className="h-3.5 w-3.5 text-muted-foreground" />
                                  </span>
                                </TooltipTrigger>
                                <TooltipContent>Has scheduled triggers</TooltipContent>
                              </Tooltip>
                            )}
                          </div>
                          {task.description && (
                            <p className="mt-0.5 text-xs text-muted-foreground">
                              {task.description}
                            </p>
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
