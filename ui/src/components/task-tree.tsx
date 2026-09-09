import { useEffect, useMemo, useState } from "react";
import { Link } from "react-router";
import { ChevronRight, Clock, Folder, FolderOpen } from "lucide-react";
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
import {
  buildRows,
  countTasks,
  taskHref,
  workspaceHref,
} from "@/lib/task-tree";
import type { TaskListItem } from "@/lib/types";

const EXPANDED_KEY = "stroem_tasks_expanded_folders";
const COLLAPSED_WS_KEY = "stroem_tasks_collapsed_workspaces";

function readSet(key: string): Set<string> {
  try {
    const saved = localStorage.getItem(key);
    if (saved) return new Set(JSON.parse(saved));
  } catch {
    // ignore corrupt localStorage
  }
  return new Set();
}

function writeSet(key: string, value: Set<string>) {
  try {
    localStorage.setItem(key, JSON.stringify([...value]));
  } catch {
    // storage may be unavailable (private mode); expansion is a convenience only
  }
}

function toggled(prev: Set<string>, key: string): Set<string> {
  const next = new Set(prev);
  if (next.has(key)) next.delete(key);
  else next.add(key);
  return next;
}

export interface TaskTreeProps {
  tasks: TaskListItem[];
  /** Free-text filter; the caller owns the input so it can sit in the page header. */
  search: string;
  /** Top-level collapsible row per workspace. */
  groupByWorkspace?: boolean;
  /** Show a Workspace column with a link per task row. */
  showWorkspaceColumn?: boolean;
}

const INDENT_REM = 1.25;
const TASK_EXTRA_REM = 2.75;

/**
 * Collapsible folder tree of tasks rendered as table rows. Expansion state is
 * persisted in localStorage and shared by every instance (Tasks page and
 * workspace page use the same keys, so a folder opened on one stays open on
 * the other).
 */
export function TaskTree({
  tasks,
  search,
  groupByWorkspace = false,
  showWorkspaceColumn = false,
}: TaskTreeProps) {
  const [expanded, setExpanded] = useState(() => readSet(EXPANDED_KEY));
  const [collapsedWorkspaces, setCollapsedWorkspaces] = useState(() =>
    readSet(COLLAPSED_WS_KEY),
  );
  useEffect(() => writeSet(EXPANDED_KEY, expanded), [expanded]);
  useEffect(
    () => writeSet(COLLAPSED_WS_KEY, collapsedWorkspaces),
    [collapsedWorkspaces],
  );

  const searching = search.trim().length > 0;
  const rows = useMemo(
    () =>
      buildRows(tasks, {
        groupByWorkspace,
        expanded,
        collapsedWorkspaces,
        search,
      }),
    [tasks, groupByWorkspace, expanded, collapsedWorkspaces, search],
  );

  if (tasks.length === 0) {
    return (
      <p className="py-8 text-center text-sm text-muted-foreground">
        No tasks found.
      </p>
    );
  }
  if (rows.length === 0) {
    return (
      <p className="py-8 text-center text-sm text-muted-foreground">
        No tasks match &ldquo;{search}&rdquo;.
      </p>
    );
  }

  const columns = showWorkspaceColumn ? 3 : 2;

  return (
    <Table>
      <TableHeader>
        <TableRow>
          <TableHead>Name</TableHead>
          {showWorkspaceColumn && <TableHead>Workspace</TableHead>}
          <TableHead>Mode</TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        {rows.map((row) => {
          if (row.kind === "workspace") {
            const isOpen = searching || !collapsedWorkspaces.has(row.workspace);
            return (
              <TableRow
                key={`ws:${row.workspace}`}
                className="cursor-pointer bg-muted/30 hover:bg-muted/50"
                onClick={() =>
                  setCollapsedWorkspaces((prev) => toggled(prev, row.workspace))
                }
                aria-expanded={isOpen}
              >
                <TableCell colSpan={columns}>
                  <div className="flex items-center gap-1.5">
                    <ChevronRight
                      className={`h-4 w-4 shrink-0 text-muted-foreground transition-transform duration-200 ${isOpen ? "rotate-90" : ""}`}
                    />
                    {isOpen ? (
                      <FolderOpen className="h-4 w-4 shrink-0 text-muted-foreground" />
                    ) : (
                      <Folder className="h-4 w-4 shrink-0 text-muted-foreground" />
                    )}
                    <Link
                      to={workspaceHref(row.workspace)}
                      className="font-mono text-sm font-semibold hover:underline"
                      onClick={(e) => e.stopPropagation()}
                    >
                      {row.workspace}
                    </Link>
                    <Badge variant="secondary" className="ml-1 text-xs">
                      {row.count}
                    </Badge>
                  </div>
                </TableCell>
              </TableRow>
            );
          }

          if (row.kind === "folder") {
            const { node, depth } = row;
            const isOpen = searching || expanded.has(node.key);
            return (
              <TableRow
                key={`folder:${node.key}`}
                className="cursor-pointer hover:bg-muted/50"
                onClick={() => setExpanded((prev) => toggled(prev, node.key))}
                aria-expanded={isOpen}
              >
                <TableCell colSpan={columns}>
                  <div
                    className="flex items-center gap-1.5"
                    style={{ paddingLeft: `${depth * INDENT_REM}rem` }}
                  >
                    <ChevronRight
                      className={`h-4 w-4 shrink-0 text-muted-foreground transition-transform duration-200 ${isOpen ? "rotate-90" : ""}`}
                    />
                    <Folder className="h-4 w-4 shrink-0 text-muted-foreground" />
                    <span className="font-medium">{node.name}</span>
                    <Badge variant="secondary" className="ml-1 text-xs">
                      {countTasks(node)}
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
                  style={{
                    paddingLeft: `${depth * INDENT_REM + TASK_EXTRA_REM}rem`,
                  }}
                >
                  <div className="flex items-center gap-1.5">
                    <Link
                      to={taskHref(task)}
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
              {showWorkspaceColumn && (
                <TableCell>
                  <Link to={workspaceHref(task.workspace)}>
                    <Badge
                      variant="secondary"
                      className="font-mono text-xs hover:underline"
                    >
                      {task.workspace}
                    </Badge>
                  </Link>
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
  );
}
