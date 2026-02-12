import { useEffect, useState } from "react";
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
import { listWorkspaces } from "@/lib/api";
import type { WorkspaceInfo } from "@/lib/types";

export function WorkspacesPage() {
  const [workspaces, setWorkspaces] = useState<WorkspaceInfo[]>([]);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    let cancelled = false;
    async function load() {
      try {
        const data = await listWorkspaces();
        if (!cancelled) setWorkspaces(data);
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
                      <Link
                        to={`/tasks`}
                        className="font-medium hover:underline"
                      >
                        {ws.name}
                      </Link>
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
