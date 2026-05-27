import { useState } from "react";
import { Link } from "react-router";
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
import { Button } from "@/components/ui/button";
import { LoadingSpinner } from "@/components/loading-spinner";
import { AlertTriangle, RefreshCw } from "lucide-react";
import { listWorkspaces, refreshWorkspace } from "@/lib/api";
import { useTitle } from "@/hooks/use-title";
import { useAsyncData } from "@/hooks/use-async-data";
import type { WorkspaceInfo } from "@/lib/types";

export function WorkspacesPage() {
  useTitle("Workspaces");
  const { data, loading, refresh } = useAsyncData<WorkspaceInfo[]>(listWorkspaces);
  const workspaces = data ?? [];
  const [refreshing, setRefreshing] = useState<Set<string>>(new Set());
  const [refreshError, setRefreshError] = useState<Record<string, string>>({});

  const handleRefresh = async (ws: string) => {
    setRefreshing((prev) => new Set(prev).add(ws));
    setRefreshError((prev) => {
      const next = { ...prev };
      delete next[ws];
      return next;
    });
    try {
      await refreshWorkspace(ws);
      await refresh();
    } catch (err) {
      setRefreshError((prev) => ({
        ...prev,
        [ws]: err instanceof Error ? err.message : "Failed to refresh",
      }));
    } finally {
      setRefreshing((prev) => {
        const next = new Set(prev);
        next.delete(ws);
        return next;
      });
    }
  };

  if (loading) {
    return <LoadingSpinner />;
  }

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">Workspaces</h1>
        <p className="text-sm text-muted-foreground">
          Configured workflow workspaces
        </p>
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">All Workspaces</CardTitle>
        </CardHeader>
        <CardContent>
          {workspaces.length === 0 ? (
            <p className="py-8 text-center text-sm text-muted-foreground">
              No workspaces found.
            </p>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Name</TableHead>
                  <TableHead>Tasks</TableHead>
                  <TableHead>Actions</TableHead>
                  <TableHead>Revision</TableHead>
                  <TableHead className="w-24 text-right">
                    <span className="sr-only">Refresh</span>
                  </TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {workspaces.map((ws) => (
                  <TableRow key={ws.name}>
                    <TableCell>
                      <div className="flex flex-col gap-1">
                        <Link
                          to={`/tasks`}
                          className="font-medium hover:underline"
                        >
                          {ws.name}
                        </Link>
                        {ws.error && (
                          <div className="flex items-start gap-1.5 text-destructive">
                            <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                            <span className="text-xs">{ws.error}</span>
                          </div>
                        )}
                        {ws.warnings?.map((w, i) => (
                          <div
                            key={i}
                            className="flex items-start gap-1.5 text-amber-600 dark:text-amber-500"
                          >
                            <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                            <span className="text-xs">{w}</span>
                          </div>
                        ))}
                        {refreshError[ws.name] && (
                          <div className="flex items-start gap-1.5 text-destructive">
                            <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                            <span className="text-xs">
                              Refresh failed: {refreshError[ws.name]}
                            </span>
                          </div>
                        )}
                      </div>
                    </TableCell>
                    <TableCell>
                      <Badge variant="secondary" className="font-mono text-xs">
                        {ws.tasks_count}
                      </Badge>
                    </TableCell>
                    <TableCell>
                      <Badge variant="secondary" className="font-mono text-xs">
                        {ws.actions_count}
                      </Badge>
                    </TableCell>
                    <TableCell>
                      {ws.revision ? (
                        <code className="text-xs text-muted-foreground">
                          {ws.revision.slice(0, 8)}
                        </code>
                      ) : (
                        <span className="text-xs text-muted-foreground">—</span>
                      )}
                    </TableCell>
                    <TableCell className="text-right">
                      <Button
                        size="sm"
                        variant="outline"
                        onClick={() => handleRefresh(ws.name)}
                        disabled={refreshing.has(ws.name)}
                        aria-label={`Refresh workspace ${ws.name}`}
                      >
                        <RefreshCw
                          className={`h-3.5 w-3.5 ${
                            refreshing.has(ws.name) ? "animate-spin" : ""
                          }`}
                        />
                        <span className="ml-1.5">Refresh</span>
                      </Button>
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
