import { useCallback, useState } from "react";
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
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { StatusBadge } from "@/components/status-badge";
import { useTitle } from "@/hooks/use-title";
import { useWorkerNames } from "@/hooks/use-worker-names";
import { useAsyncData } from "@/hooks/use-async-data";
import { listJobs } from "@/lib/api";
import { formatDuration, formatTime } from "@/lib/formatting";
import type { JobListItem } from "@/lib/types";

const PAGE_SIZE = 20;
const STATUSES = ["all", "pending", "running", "completed", "failed"] as const;

export function JobsPage() {
  useTitle("Jobs");
  const workerNames = useWorkerNames();
  const [offset, setOffset] = useState(0);
  const [statusFilter, setStatusFilter] = useState<string>("all");

  const fetcher = useCallback(() => listJobs(PAGE_SIZE, offset), [offset]);
  const { data: jobs, loading } = useAsyncData(fetcher, {
    pollInterval: 5000,
  });
  const jobList = jobs ?? [];

  const filtered =
    statusFilter === "all"
      ? jobList
      : jobList.filter((j) => j.status === statusFilter);

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">Jobs</h1>
        <p className="text-sm text-muted-foreground">
          Workflow execution history
        </p>
      </div>

      <Card>
        <CardHeader className="flex flex-row items-center justify-between">
          <CardTitle className="text-base">All Jobs</CardTitle>
          <Tabs
            value={statusFilter}
            onValueChange={setStatusFilter}
          >
            <TabsList>
              {STATUSES.map((s) => (
                <TabsTrigger key={s} value={s} className="capitalize">
                  {s}
                </TabsTrigger>
              ))}
            </TabsList>
          </Tabs>
        </CardHeader>
        <CardContent>
          {loading ? (
            <div className="flex items-center justify-center py-12">
              <div className="h-8 w-8 animate-spin rounded-full border-4 border-muted border-t-primary" />
            </div>
          ) : filtered.length === 0 ? (
            <p className="py-8 text-center text-sm text-muted-foreground">
              No jobs found.
            </p>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Task</TableHead>
                  <TableHead>Workspace</TableHead>
                  <TableHead>Status</TableHead>
                  <TableHead>Worker</TableHead>
                  <TableHead>Source</TableHead>
                  <TableHead>Created</TableHead>
                  <TableHead className="text-right">Duration</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {filtered.map((job: JobListItem) => (
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
                      <Badge variant="secondary" className="font-mono text-xs">
                        {job.workspace}
                      </Badge>
                    </TableCell>
                    <TableCell>
                      <StatusBadge status={job.status} />
                    </TableCell>
                    <TableCell className="text-sm text-muted-foreground">
                      {job.worker_id ? (
                        <Link
                          to={`/workers/${job.worker_id}`}
                          className="hover:underline"
                        >
                          {workerNames.get(job.worker_id) ?? job.worker_id.substring(0, 8)}
                        </Link>
                      ) : "-"}
                    </TableCell>
                    <TableCell className="text-muted-foreground">
                      {job.source_id ? `${job.source_type} (${job.source_id})` : job.source_type}
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

          <div className="mt-4 flex items-center justify-between">
            <Button
              variant="outline"
              size="sm"
              disabled={offset === 0}
              onClick={() => setOffset((o) => Math.max(0, o - PAGE_SIZE))}
            >
              Previous
            </Button>
            <span className="text-xs text-muted-foreground">
              Page {Math.floor(offset / PAGE_SIZE) + 1}
            </span>
            <Button
              variant="outline"
              size="sm"
              disabled={jobList.length < PAGE_SIZE}
              onClick={() => setOffset((o) => o + PAGE_SIZE)}
            >
              Next
            </Button>
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
