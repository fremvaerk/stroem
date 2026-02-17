import { useEffect, useState } from "react";
import { Link } from "react-router";
import { Clock, CheckCircle2, XCircle, Loader2 } from "lucide-react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { StatusBadge } from "@/components/status-badge";
import { listJobs } from "@/lib/api";
import { useTitle } from "@/hooks/use-title";
import type { JobListItem } from "@/lib/types";

function formatRelative(dateStr: string): string {
  const now = Date.now();
  const then = new Date(dateStr).getTime();
  const diff = now - then;
  const seconds = Math.floor(diff / 1000);
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}

export function DashboardPage() {
  useTitle("Dashboard");
  const [jobs, setJobs] = useState<JobListItem[]>([]);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    let cancelled = false;

    async function load() {
      try {
        const data = await listJobs(100, 0);
        if (!cancelled) setJobs(data);
      } catch {
        // ignore
      } finally {
        if (!cancelled) setLoading(false);
      }
    }

    load();
    const interval = setInterval(load, 5000);
    return () => {
      cancelled = true;
      clearInterval(interval);
    };
  }, []);

  const counts = {
    pending: jobs.filter((j) => j.status === "pending").length,
    running: jobs.filter((j) => j.status === "running").length,
    completed: jobs.filter((j) => j.status === "completed").length,
    failed: jobs.filter((j) => j.status === "failed").length,
  };

  const recentJobs = jobs.slice(0, 10);

  const statCards = [
    {
      label: "Pending",
      value: counts.pending,
      icon: Clock,
      color: "text-yellow-600 dark:text-yellow-400",
    },
    {
      label: "Running",
      value: counts.running,
      icon: Loader2,
      color: "text-blue-600 dark:text-blue-400",
    },
    {
      label: "Completed",
      value: counts.completed,
      icon: CheckCircle2,
      color: "text-green-600 dark:text-green-400",
    },
    {
      label: "Failed",
      value: counts.failed,
      icon: XCircle,
      color: "text-red-600 dark:text-red-400",
    },
  ];

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
        <h1 className="text-2xl font-semibold tracking-tight">Dashboard</h1>
        <p className="text-sm text-muted-foreground">
          Overview of workflow execution
        </p>
      </div>

      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
        {statCards.map((card) => (
          <Card key={card.label}>
            <CardHeader className="flex flex-row items-center justify-between pb-2">
              <CardTitle className="text-sm font-medium text-muted-foreground">
                {card.label}
              </CardTitle>
              <card.icon className={`h-4 w-4 ${card.color}`} />
            </CardHeader>
            <CardContent>
              <div className="text-3xl font-bold tabular-nums">
                {card.value}
              </div>
            </CardContent>
          </Card>
        ))}
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Recent Jobs</CardTitle>
        </CardHeader>
        <CardContent>
          {recentJobs.length === 0 ? (
            <p className="py-8 text-center text-sm text-muted-foreground">
              No jobs yet. Trigger a task to get started.
            </p>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Task</TableHead>
                  <TableHead>Status</TableHead>
                  <TableHead>Source</TableHead>
                  <TableHead className="text-right">Created</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {recentJobs.map((job) => (
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
                      <StatusBadge status={job.status} />
                    </TableCell>
                    <TableCell className="text-muted-foreground">
                      {job.source_type}
                    </TableCell>
                    <TableCell className="text-right font-mono text-xs text-muted-foreground">
                      {formatRelative(job.created_at)}
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
