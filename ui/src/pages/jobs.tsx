import { useCallback, useState } from "react";
import { Link } from "react-router";
import { Search, X } from "lucide-react";
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
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Badge } from "@/components/ui/badge";
import { StatusBadge } from "@/components/status-badge";
import { PaginationControls } from "@/components/pagination-controls";
import { LoadingSpinner } from "@/components/loading-spinner";
import { useTitle } from "@/hooks/use-title";
import { useAsyncData } from "@/hooks/use-async-data";
import { listJobs } from "@/lib/api";
import { formatDuration, formatTime } from "@/lib/formatting";
import type { JobListItem } from "@/lib/types";

const PAGE_SIZE = 20;
const STATUSES = ["all", "pending", "running", "completed", "failed", "cancelled", "skipped"] as const;

export function JobsPage() {
  useTitle("Jobs");
  const [offset, setOffset] = useState(0);
  const [statusFilter, setStatusFilter] = useState<string>("all");
  const [search, setSearch] = useState("");

  const fetcher = useCallback(
    () =>
      listJobs(PAGE_SIZE, offset, {
        status: statusFilter === "all" ? undefined : statusFilter,
      }),
    [offset, statusFilter],
  );
  const { data, loading } = useAsyncData(fetcher, {
    pollInterval: 5000,
  });
  const jobs = data?.items ?? [];
  const total = data?.total ?? 0;
  const filtered = search
    ? jobs.filter((j) => {
        const q = search.toLowerCase();
        return (
          j.task_name.toLowerCase().includes(q) ||
          j.status.toLowerCase().includes(q) ||
          j.workspace.toLowerCase().includes(q) ||
          j.job_id.toLowerCase().includes(q)
        );
      })
    : jobs;

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">Jobs</h1>
        <p className="text-sm text-muted-foreground">
          Workflow execution history
        </p>
      </div>

      <Card>
        <CardHeader className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
          <CardTitle className="text-base">All Jobs</CardTitle>
          <div className="flex flex-wrap items-center gap-3">
            <div className="relative">
              <Search className="absolute left-3 top-1/2 -translate-y-1/2 h-4 w-4 text-muted-foreground" />
              <Input
                placeholder="Search jobs..."
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
            <Tabs
              value={statusFilter}
              onValueChange={(v) => {
                setStatusFilter(v);
                setOffset(0);
              }}
            >
              <TabsList>
                {STATUSES.map((s) => (
                  <TabsTrigger key={s} value={s} className="capitalize">
                    {s}
                  </TabsTrigger>
                ))}
              </TabsList>
            </Tabs>
          </div>
        </CardHeader>
        <CardContent>
          {loading ? (
            <LoadingSpinner />
          ) : filtered.length === 0 ? (
            <p className="py-8 text-center text-sm text-muted-foreground">
              {search ? `No jobs match "${search}".` : "No jobs found."}
            </p>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Task</TableHead>
                  <TableHead>Workspace</TableHead>
                  <TableHead>Status</TableHead>
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

          <PaginationControls
            offset={offset}
            pageSize={PAGE_SIZE}
            total={total}
            onPrevious={() => setOffset((o) => Math.max(0, o - PAGE_SIZE))}
            onNext={() => setOffset((o) => o + PAGE_SIZE)}
          />
        </CardContent>
      </Card>
    </div>
  );
}
