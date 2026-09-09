import { useCallback, useMemo, useState } from "react";
import { Search, X } from "lucide-react";
import { LoadingSpinner } from "@/components/loading-spinner";
import { TaskTree } from "@/components/task-tree";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { listAllTasks } from "@/lib/api";
import { useAsyncData } from "@/hooks/use-async-data";
import { useTitle } from "@/hooks/use-title";
import type { TaskListItem } from "@/lib/types";

export type TasksView = "merged" | "workspace";
const VIEW_KEY = "stroem_tasks_view";

function readView(): TasksView {
  try {
    return localStorage.getItem(VIEW_KEY) === "workspace" ? "workspace" : "merged";
  } catch {
    return "merged";
  }
}

export function TasksPage() {
  useTitle("Tasks");
  const fetcher = useCallback(() => listAllTasks(), []);
  const { data, loading, error } = useAsyncData<TaskListItem[]>(fetcher);
  const tasks = useMemo(() => data ?? [], [data]);
  const [search, setSearch] = useState("");
  const [view, setView] = useState<TasksView>(readView);

  const workspaceCount = useMemo(
    () => new Set(tasks.map((t) => t.workspace)).size,
    [tasks],
  );
  const multiWorkspace = workspaceCount > 1;
  const grouped = multiWorkspace && view === "workspace";

  function changeView(next: string) {
    const value: TasksView = next === "workspace" ? "workspace" : "merged";
    setView(value);
    try {
      localStorage.setItem(VIEW_KEY, value);
    } catch {
      // storage unavailable; the choice just won't persist
    }
  }

  if (loading) {
    return <LoadingSpinner />;
  }

  return (
    <div className="space-y-6">
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Tasks</h1>
          <p className="text-sm text-muted-foreground">
            Available workflow tasks
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-3">
          {multiWorkspace && (
            <Tabs value={view} onValueChange={changeView}>
              <TabsList aria-label="Task list layout">
                <TabsTrigger value="merged">Merged</TabsTrigger>
                <TabsTrigger value="workspace">By workspace</TabsTrigger>
              </TabsList>
            </Tabs>
          )}
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
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">All Tasks</CardTitle>
        </CardHeader>
        <CardContent>
          {error ? (
            <p className="py-8 text-center text-sm text-destructive">{error}</p>
          ) : (
            <TaskTree
              tasks={tasks}
              search={search}
              groupByWorkspace={grouped}
              showWorkspaceColumn={multiWorkspace && !grouped}
            />
          )}
        </CardContent>
      </Card>
    </div>
  );
}
