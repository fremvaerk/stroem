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
import { InfoGrid } from "@/components/info-grid";
import { LoadingSpinner } from "@/components/loading-spinner";
import { useTitle } from "@/hooks/use-title";
import { getWorker } from "@/lib/api";
import { formatRelativeTime, formatTime, formatDuration } from "@/lib/formatting";
import type { WorkerDetail } from "@/lib/types";

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
            <Badge
              variant="secondary"
              className={
                worker.status === "active"
                  ? "bg-green-100 text-green-800 dark:bg-green-900/30 dark:text-green-400"
                  : "bg-gray-100 text-gray-800 dark:bg-gray-900/30 dark:text-gray-400"
              }
            >
              {worker.status === "active" && (
                <span className="mr-1.5 inline-block h-1.5 w-1.5 animate-pulse rounded-full bg-current" />
              )}
              {worker.status}
            </Badge>
          </div>
          <p className="mt-0.5 font-mono text-xs text-muted-foreground">
            {worker.worker_id}
          </p>
        </div>
      </div>

      <InfoGrid
        columns={5}
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
          <CardTitle className="text-base">Recent Jobs</CardTitle>
        </CardHeader>
        <CardContent>
          {worker.jobs.length === 0 ? (
            <p className="py-8 text-center text-sm text-muted-foreground">
              No jobs executed by this worker.
            </p>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Task</TableHead>
                  <TableHead>Workspace</TableHead>
                  <TableHead>Status</TableHead>
                  <TableHead>Created</TableHead>
                  <TableHead className="text-right">Duration</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {worker.jobs.map((job) => (
                  <TableRow key={job.job_id}>
                    <TableCell>
                      <Link
                        to={`/jobs/${job.job_id}`}
                        className="font-medium hover:underline"
                      >
                        {job.task_name}
                      </Link>
                    </TableCell>
                    <TableCell>
                      <Badge
                        variant="secondary"
                        className="font-mono text-xs"
                      >
                        {job.workspace}
                      </Badge>
                    </TableCell>
                    <TableCell>
                      <StatusBadge status={job.status} />
                    </TableCell>
                    <TableCell className="font-mono text-xs text-muted-foreground">
                      {formatTime(job.created_at)}
                    </TableCell>
                    <TableCell className="text-right font-mono text-xs text-muted-foreground">
                      {formatDuration(job.started_at, job.completed_at)}
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
