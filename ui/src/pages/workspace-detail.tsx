import { useCallback, useMemo, useState } from "react";
import { Link, useParams } from "react-router";
import { AlertTriangle, ArrowLeft, RefreshCw, Search, X } from "lucide-react";
import { LoadingSpinner } from "@/components/loading-spinner";
import { TaskTree } from "@/components/task-tree";
import { taskHref } from "@/lib/task-tree";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
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
import {
  listTasks,
  listTriggers,
  listWorkspaces,
  refreshWorkspace,
} from "@/lib/api";
import { formatTime } from "@/lib/formatting";
import { useAsyncData } from "@/hooks/use-async-data";
import { useTitle } from "@/hooks/use-title";
import type { TaskListItem, TriggerInfo, WorkspaceInfo } from "@/lib/types";

interface WorkspacePageData {
  info: WorkspaceInfo | null;
  tasks: TaskListItem[];
  triggers: TriggerInfo[];
  /** Set when the workspace exists but its tasks/triggers could not be listed. */
  contentError: string | null;
}

async function loadWorkspace(name: string): Promise<WorkspacePageData> {
  const all = await listWorkspaces();
  const info = all.find((w) => w.name === name) ?? null;
  if (!info) return { info: null, tasks: [], triggers: [], contentError: null };
  try {
    const [tasks, triggers] = await Promise.all([
      listTasks(name),
      listTriggers(name),
    ]);
    return { info, tasks, triggers, contentError: null };
  } catch (err) {
    return {
      info,
      tasks: [],
      triggers: [],
      contentError: err instanceof Error ? err.message : "Failed to load",
    };
  }
}

export function WorkspaceDetailPage() {
  const { workspace = "" } = useParams<{ workspace: string }>();
  useTitle(workspace || "Workspace");
  const fetcher = useCallback(() => loadWorkspace(workspace), [workspace]);
  const { data, loading, error, refresh } = useAsyncData(fetcher);
  const [search, setSearch] = useState("");
  const [refreshing, setRefreshing] = useState(false);
  const [refreshError, setRefreshError] = useState<string | null>(null);

  const tasks = useMemo(() => data?.tasks ?? [], [data]);

  const handleRefresh = async () => {
    setRefreshing(true);
    setRefreshError(null);
    try {
      await refreshWorkspace(workspace);
      await refresh();
    } catch (err) {
      setRefreshError(err instanceof Error ? err.message : "Failed to refresh");
    } finally {
      setRefreshing(false);
    }
  };

  if (loading) {
    return <LoadingSpinner />;
  }

  if (error || !data?.info) {
    return (
      <div className="py-20 text-center">
        <p className="text-sm text-destructive">
          {error ?? `Workspace "${workspace}" not found`}
        </p>
        <Button variant="link" asChild className="mt-2">
          <Link to="/workspaces">Back to workspaces</Link>
        </Button>
      </div>
    );
  }

  const { info, triggers, contentError } = data;

  return (
    <div className="space-y-6">
      <div className="flex flex-wrap items-start gap-4">
        <Button variant="ghost" size="icon" asChild>
          <Link to="/workspaces" aria-label="Back to workspaces">
            <ArrowLeft className="h-4 w-4" />
          </Link>
        </Button>
        <div className="flex-1 min-w-0">
          <h1 className="font-mono text-2xl font-semibold tracking-tight">
            {info.name}
          </h1>
          <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-sm text-muted-foreground">
            <span>
              revision{" "}
              {info.revision ? (
                <code className="text-xs">{info.revision.slice(0, 8)}</code>
              ) : (
                "—"
              )}
            </span>
            <span>·</span>
            <span>{info.tasks_count} tasks</span>
            <span>·</span>
            <span>{info.actions_count} actions</span>
            <span>·</span>
            <span>
              {info.triggers_count} triggers
              {!info.triggers_enabled && (
                <Badge
                  variant="outline"
                  className="ml-1.5 text-xs"
                  title="Triggers are disabled for this workspace in the server config (triggers: false). Schedules, webhooks and event sources will not fire here; tasks can still be run manually."
                >
                  off
                </Badge>
              )}
            </span>
          </div>
          {info.error && (
            <div className="mt-2 flex items-start gap-1.5 text-sm text-destructive">
              <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
              <span>{info.error}</span>
            </div>
          )}
          {info.warnings?.map((w, i) => (
            <div
              key={i}
              className="mt-1 flex items-start gap-1.5 text-sm text-amber-600 dark:text-amber-500"
            >
              <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
              <span>{w}</span>
            </div>
          ))}
          {refreshError && (
            <div className="mt-2 flex items-start gap-1.5 text-sm text-destructive">
              <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
              <span>Refresh failed: {refreshError}</span>
            </div>
          )}
        </div>
        <Button
          size="sm"
          variant="outline"
          onClick={handleRefresh}
          disabled={refreshing}
          aria-label={`Refresh workspace ${info.name}`}
        >
          <RefreshCw
            className={`h-3.5 w-3.5 ${refreshing ? "animate-spin" : ""}`}
          />
          <span className="ml-1.5">Refresh</span>
        </Button>
      </div>

      <Card>
        <CardHeader>
          <div className="flex flex-wrap items-center justify-between gap-4">
            <CardTitle className="text-base">Tasks</CardTitle>
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
        </CardHeader>
        <CardContent>
          {contentError ? (
            <p className="py-8 text-center text-sm text-destructive">
              {contentError}
            </p>
          ) : (
            <TaskTree tasks={tasks} search={search} />
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Triggers</CardTitle>
        </CardHeader>
        <CardContent>
          {contentError ? (
            <p className="py-8 text-center text-sm text-destructive">
              {contentError}
            </p>
          ) : triggers.length === 0 ? (
            <p className="py-8 text-center text-sm text-muted-foreground">
              No triggers defined.
            </p>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Name</TableHead>
                  <TableHead>Type</TableHead>
                  <TableHead>Task</TableHead>
                  <TableHead>Schedule</TableHead>
                  <TableHead>Next run</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {triggers.map((t) => (
                  <TableRow key={t.name}>
                    <TableCell className="font-medium">
                      <span className="flex items-center gap-1.5">
                        {t.name}
                        {!t.enabled && (
                          <Badge variant="outline" className="text-xs">
                            disabled
                          </Badge>
                        )}
                      </span>
                    </TableCell>
                    <TableCell>
                      <Badge variant="outline" className="font-mono text-xs">
                        {t.type}
                      </Badge>
                    </TableCell>
                    <TableCell>
                      <Link
                        to={taskHref({ workspace: info.name, id: t.task })}
                        className="hover:underline"
                      >
                        {t.task}
                      </Link>
                    </TableCell>
                    <TableCell>
                      {t.cron ? (
                        <code className="text-xs">{t.cron}</code>
                      ) : (
                        <span className="text-xs text-muted-foreground">—</span>
                      )}
                    </TableCell>
                    <TableCell className="text-xs text-muted-foreground">
                      {t.next_runs[0] ? formatTime(t.next_runs[0]) : "—"}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
