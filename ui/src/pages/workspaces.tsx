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
import { LoadingSpinner } from "@/components/loading-spinner";
import { AlertTriangle } from "lucide-react";
import { listWorkspaces } from "@/lib/api";
import { useTitle } from "@/hooks/use-title";
import { useAsyncData } from "@/hooks/use-async-data";
import type { WorkspaceInfo } from "@/lib/types";

export function WorkspacesPage() {
  useTitle("Workspaces");
  const { data, loading } = useAsyncData<WorkspaceInfo[]>(listWorkspaces);
  const workspaces = data ?? [];

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
