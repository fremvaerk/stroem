import { useEffect, useState } from "react";
import { useParams, Link } from "react-router";
import { ArrowLeft, Play } from "lucide-react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { getTask } from "@/lib/api";
import type { TaskDetail } from "@/lib/types";

export function TaskDetailPage() {
  const { workspace, name } = useParams<{ workspace: string; name: string }>();
  const [task, setTask] = useState<TaskDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");

  useEffect(() => {
    if (!workspace || !name) return;
    let cancelled = false;

    async function load() {
      try {
        const data = await getTask(workspace!, name!);
        if (!cancelled) setTask(data);
      } catch (err) {
        if (!cancelled)
          setError(err instanceof Error ? err.message : "Failed to load task");
      } finally {
        if (!cancelled) setLoading(false);
      }
    }

    load();
    return () => {
      cancelled = true;
    };
  }, [workspace, name]);

  if (loading) {
    return (
      <div className="flex items-center justify-center py-20">
        <div className="h-8 w-8 animate-spin rounded-full border-4 border-muted border-t-primary" />
      </div>
    );
  }

  if (error || !task) {
    return (
      <div className="py-20 text-center">
        <p className="text-sm text-destructive">{error || "Task not found"}</p>
        <Button variant="link" asChild className="mt-2">
          <Link to="/tasks">Back to tasks</Link>
        </Button>
      </div>
    );
  }

  const flowSteps = Object.entries(task.flow);
  const inputFields = Object.entries(task.input);

  return (
    <div className="space-y-6">
      <div className="flex items-center gap-4">
        <Button variant="ghost" size="icon" asChild>
          <Link to="/tasks">
            <ArrowLeft className="h-4 w-4" />
          </Link>
        </Button>
        <div className="flex-1">
          <h1 className="text-2xl font-semibold tracking-tight">
            {task.name}
          </h1>
          <div className="mt-1 flex items-center gap-2">
            <Badge variant="outline" className="font-mono text-xs">
              {task.mode}
            </Badge>
            {workspace && (
              <Badge variant="secondary" className="font-mono text-xs">
                {workspace}
              </Badge>
            )}
            <span className="text-sm text-muted-foreground">
              {flowSteps.length} step{flowSteps.length !== 1 ? "s" : ""}
            </span>
          </div>
        </div>
        <Button asChild>
          <Link to="run">
            <Play className="mr-2 h-4 w-4" />
            Run Task
          </Link>
        </Button>
      </div>

      {inputFields.length > 0 && (
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Input Parameters</CardTitle>
          </CardHeader>
          <CardContent>
            <div className="space-y-3">
              {inputFields.map(([key, field]) => (
                <div
                  key={key}
                  className="flex items-start justify-between rounded-md border px-3 py-2"
                >
                  <div>
                    <span className="font-mono text-sm">{key}</span>
                    {field.description && (
                      <p className="text-xs text-muted-foreground">
                        {field.description}
                      </p>
                    )}
                  </div>
                  <div className="flex items-center gap-2">
                    <Badge variant="secondary" className="font-mono text-xs">
                      {field.type}
                    </Badge>
                    {field.required && (
                      <Badge
                        variant="secondary"
                        className="bg-yellow-100 text-xs text-yellow-800 dark:bg-yellow-900/30 dark:text-yellow-400"
                      >
                        required
                      </Badge>
                    )}
                  </div>
                </div>
              ))}
            </div>
          </CardContent>
        </Card>
      )}

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Flow Steps</CardTitle>
        </CardHeader>
        <CardContent>
          <div className="space-y-2">
            {flowSteps.map(([stepName, step], index) => (
              <div
                key={stepName}
                className="flex items-center gap-3 rounded-md border px-3 py-2"
              >
                <span className="flex h-6 w-6 shrink-0 items-center justify-center rounded-full bg-muted text-xs font-medium">
                  {index + 1}
                </span>
                <div className="min-w-0 flex-1">
                  <span className="font-mono text-sm">{stepName}</span>
                  <p className="text-xs text-muted-foreground">
                    action: {step.action}
                    {step.depends_on && step.depends_on.length > 0 && (
                      <> &middot; depends on: {step.depends_on.join(", ")}</>
                    )}
                  </p>
                </div>
              </div>
            ))}
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
