import { useCallback, useEffect, useState } from "react";
import { useParams, Link } from "react-router";
import { ArrowLeft } from "lucide-react";
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
import { StatusBadge } from "@/components/status-badge";
import { WorkerStatusBadge } from "@/components/worker-status-badge";
import { InfoGrid } from "@/components/info-grid";
import { LoadingSpinner } from "@/components/loading-spinner";
import { useTitle } from "@/hooks/use-title";
import { getWorker } from "@/lib/api";
import { formatRelativeTime, formatTime, formatDuration } from "@/lib/formatting";
import type { WorkerDetail, WorkerStepItem } from "@/lib/types";

export function WorkerDetailPage() {
  const { id } = useParams<{ id: string }>();
  const [worker, setWorker] = useState<WorkerDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");

  useTitle(worker ? `Worker: ${worker.name}` : "Worker");

  const load = useCallback(async () => {
    if (!id) return;
    try {
      const data = await getWorker(id);
      setWorker(data);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load worker");
    } finally {
      setLoading(false);
    }
  }, [id]);

  useEffect(() => {
    load();
  }, [load]);

  // Auto-refresh while worker is active
  useEffect(() => {
    if (!worker || worker.status !== "active") return;
    const interval = setInterval(load, 5000);
    return () => clearInterval(interval);
  }, [worker, load]);

  if (loading) {
    return <LoadingSpinner />;
  }

  if (error || !worker) {
    return (
      <div className="py-20 text-center">
        <p className="text-sm text-destructive">
          {error || "Worker not found"}
        </p>
        <Button variant="link" asChild className="mt-2">
          <Link to="/workers">Back to workers</Link>
        </Button>
      </div>
    );
  }

  return (
    <div className="space-y-6">
      <div className="flex items-center gap-4">
        <Button variant="ghost" size="icon" asChild>
          <Link to="/workers">
            <ArrowLeft className="h-4 w-4" />
          </Link>
        </Button>
        <div className="flex-1">
          <div className="flex items-center gap-3">
            <h1 className="text-2xl font-semibold tracking-tight">
              {worker.name}
            </h1>
            <WorkerStatusBadge status={worker.status} />
          </div>
          <p className="mt-0.5 font-mono text-xs text-muted-foreground">
            {worker.worker_id}
          </p>
        </div>
      </div>

      <InfoGrid
        columns={6}
        items={[
          {
            label: "Worker ID",
            value: (
              <span className="truncate font-mono">
                {worker.worker_id.substring(0, 8)}...
              </span>
            ),
          },
          { label: "Status", value: worker.status },
          {
            label: "Version",
            value: worker.version ?? (
              <span className="text-muted-foreground">—</span>
            ),
          },
          {
            label: "Tags",
            value:
              worker.tags.length > 0 ? (
                <div className="flex flex-wrap gap-1">
                  {worker.tags.map((tag) => (
                    <Badge key={tag} variant="outline" className="text-xs">
                      {tag}
                    </Badge>
                  ))}
                </div>
              ) : (
                <span className="text-muted-foreground">-</span>
              ),
          },
          {
            label: "Last Heartbeat",
            value: formatRelativeTime(worker.last_heartbeat),
          },
          { label: "Registered", value: formatTime(worker.registered_at) },
        ]}
      />

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Recent Steps</CardTitle>
        </CardHeader>
        <CardContent>
          {worker.steps.items.length === 0 ? (
            <p className="py-8 text-center text-sm text-muted-foreground">
              No steps executed by this worker.
            </p>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Step</TableHead>
                  <TableHead>Task</TableHead>
                  <TableHead>Workspace</TableHead>
                  <TableHead>Status</TableHead>
                  <TableHead>Started</TableHead>
                  <TableHead className="text-right">Duration</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {worker.steps.items.map((step: WorkerStepItem) => (
                  <TableRow key={`${step.job_id}/${step.step_name}`}>
                    <TableCell className="font-medium">
                      {step.step_name}
                    </TableCell>
                    <TableCell>
                      <Link
                        to={`/jobs/${step.job_id}`}
                        className="hover:underline"
                      >
                        {step.task_name}
                      </Link>
                    </TableCell>
                    <TableCell>
                      <Badge
                        variant="secondary"
                        className="font-mono text-xs"
                      >
                        {step.workspace}
                      </Badge>
                    </TableCell>
                    <TableCell>
                      <StatusBadge status={step.status} />
                    </TableCell>
                    <TableCell className="font-mono text-xs text-muted-foreground">
                      {formatTime(step.started_at)}
                    </TableCell>
                    <TableCell className="text-right font-mono text-xs text-muted-foreground">
                      {formatDuration(step.started_at, step.completed_at)}
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
