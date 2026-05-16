import { useEffect, useState } from "react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { LoadingSpinner } from "@/components/loading-spinner";
import { Sparkline } from "@/components/sparkline";
import { getTaskStats } from "@/lib/api";
import { formatDurationMs } from "@/lib/formatting";
import type { TaskStatsResponse } from "@/lib/types";

interface DurationInsightsCardProps {
  workspace: string;
  taskName: string;
  /** Hide the card entirely when sample size is below this. Default 5. */
  minSample?: number;
}

function StatChip({
  label,
  value,
  tone = "neutral",
}: {
  label: string;
  value: string;
  tone?: "neutral" | "primary" | "warning";
}) {
  const toneClass = {
    neutral: "bg-muted text-foreground",
    primary: "bg-emerald-100 text-emerald-900 dark:bg-emerald-950 dark:text-emerald-100",
    warning: "bg-amber-100 text-amber-900 dark:bg-amber-950 dark:text-amber-100",
  }[tone];
  return (
    <div
      className={`flex flex-col rounded-md px-3 py-2 ${toneClass}`}
      data-testid={`stat-${label.toLowerCase()}`}
    >
      <span className="text-[10px] uppercase tracking-wide opacity-70">
        {label}
      </span>
      <span className="font-mono text-sm font-semibold tabular-nums">
        {value}
      </span>
    </div>
  );
}

export function DurationInsightsCard({
  workspace,
  taskName,
  minSample = 5,
}: DurationInsightsCardProps) {
  const [stats, setStats] = useState<TaskStatsResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);
    getTaskStats(workspace, taskName, 50)
      .then((data) => {
        if (!cancelled) setStats(data);
      })
      .catch((err: unknown) => {
        if (!cancelled) {
          setError(err instanceof Error ? err.message : "Failed to load stats");
        }
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [workspace, taskName]);

  if (loading) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="text-base">Duration insights</CardTitle>
        </CardHeader>
        <CardContent>
          <LoadingSpinner />
        </CardContent>
      </Card>
    );
  }

  if (error) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="text-base">Duration insights</CardTitle>
        </CardHeader>
        <CardContent>
          <p className="text-sm text-destructive">{error}</p>
        </CardContent>
      </Card>
    );
  }

  if (!stats || stats.task.sample_size < minSample) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="text-base">Duration insights</CardTitle>
        </CardHeader>
        <CardContent>
          <p className="text-sm text-muted-foreground">
            Insufficient data — at least {minSample} completed runs are needed
            (currently {stats?.task.sample_size ?? 0}).
          </p>
        </CardContent>
      </Card>
    );
  }

  const { task, steps } = stats;
  const sparkValues = task.recent
    .map((r) => r.duration_ms)
    .filter((v): v is number => v != null);

  return (
    <Card data-testid="duration-insights">
      <CardHeader>
        <div className="flex items-center justify-between gap-4">
          <CardTitle className="text-base">
            Duration insights
            <span className="ml-2 text-xs font-normal text-muted-foreground">
              last {task.sample_size} {task.sample_size === 1 ? "run" : "runs"}
            </span>
          </CardTitle>
        </div>
      </CardHeader>
      <CardContent className="space-y-6">
        <div className="flex flex-wrap items-center gap-3">
          <StatChip label="p50" value={formatDurationMs(task.p50_ms)} tone="primary" />
          <StatChip label="p95" value={formatDurationMs(task.p95_ms)} tone="warning" />
          <StatChip label="avg" value={formatDurationMs(task.avg_ms)} />
          <StatChip label="min" value={formatDurationMs(task.min_ms)} />
          <StatChip label="max" value={formatDurationMs(task.max_ms)} />
          <div className="ml-auto">
            <Sparkline
              values={sparkValues}
              p50={task.p50_ms}
              p95={task.p95_ms}
              ariaLabel={`Duration history for ${taskName}`}
            />
          </div>
        </div>

        {steps.length > 0 && (
          <div>
            <h4 className="mb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
              Per-step breakdown
            </h4>
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Step</TableHead>
                  <TableHead className="text-right">p50</TableHead>
                  <TableHead className="text-right">p95</TableHead>
                  <TableHead className="text-right">avg</TableHead>
                  <TableHead className="text-right text-muted-foreground">
                    n
                  </TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {steps.map((s) => (
                  <TableRow key={s.step_name}>
                    <TableCell className="font-mono text-sm">
                      {s.step_name}
                    </TableCell>
                    <TableCell className="text-right font-mono text-sm tabular-nums">
                      {formatDurationMs(s.p50_ms)}
                    </TableCell>
                    <TableCell className="text-right font-mono text-sm tabular-nums">
                      {formatDurationMs(s.p95_ms)}
                    </TableCell>
                    <TableCell className="text-right font-mono text-sm tabular-nums text-muted-foreground">
                      {formatDurationMs(s.avg_ms)}
                    </TableCell>
                    <TableCell className="text-right font-mono text-xs text-muted-foreground">
                      {s.sample_size}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>
        )}
      </CardContent>
    </Card>
  );
}
