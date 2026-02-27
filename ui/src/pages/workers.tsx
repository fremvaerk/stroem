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
import { Badge } from "@/components/ui/badge";
import { PaginationControls } from "@/components/pagination-controls";
import { useTitle } from "@/hooks/use-title";
import { useAsyncData } from "@/hooks/use-async-data";
import { listWorkers } from "@/lib/api";
import { formatRelativeTime, formatTime } from "@/lib/formatting";
import type { WorkerListItem } from "@/lib/types";

const PAGE_SIZE = 20;

export function WorkersPage() {
  useTitle("Workers");
  const [offset, setOffset] = useState(0);

  const fetcher = useCallback(() => listWorkers(PAGE_SIZE, offset), [offset]);
  const { data, loading } = useAsyncData(fetcher, {
    pollInterval: 5000,
  });
  const workerList = data?.items ?? [];
  const total = data?.total ?? 0;

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">Workers</h1>
        <p className="text-sm text-muted-foreground">
          Registered worker processes
        </p>
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">All Workers</CardTitle>
        </CardHeader>
        <CardContent>
          {loading ? (
            <div className="flex items-center justify-center py-12">
              <div className="h-8 w-8 animate-spin rounded-full border-4 border-muted border-t-primary" />
            </div>
          ) : workerList.length === 0 ? (
            <p className="py-8 text-center text-sm text-muted-foreground">
              No workers registered.
            </p>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Name</TableHead>
                  <TableHead>Status</TableHead>
                  <TableHead>Tags</TableHead>
                  <TableHead>Last Heartbeat</TableHead>
                  <TableHead>Registered</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {workerList.map((w: WorkerListItem) => (
                  <TableRow key={w.worker_id}>
                    <TableCell>
                      <Link
                        to={`/workers/${w.worker_id}`}
                        className="font-medium hover:underline"
                      >
                        {w.name}
                      </Link>
                    </TableCell>
                    <TableCell>
                      <Badge
                        variant="secondary"
                        className={
                          w.status === "active"
                            ? "bg-green-100 text-green-800 dark:bg-green-900/30 dark:text-green-400"
                            : "bg-gray-100 text-gray-800 dark:bg-gray-900/30 dark:text-gray-400"
                        }
                      >
                        {w.status === "active" && (
                          <span className="mr-1.5 inline-block h-1.5 w-1.5 animate-pulse rounded-full bg-current" />
                        )}
                        {w.status}
                      </Badge>
                    </TableCell>
                    <TableCell>
                      <div className="flex flex-wrap gap-1">
                        {w.tags.map((tag) => (
                          <Badge
                            key={tag}
                            variant="outline"
                            className="text-xs"
                          >
                            {tag}
                          </Badge>
                        ))}
                      </div>
                    </TableCell>
                    <TableCell className="font-mono text-xs text-muted-foreground">
                      {formatRelativeTime(w.last_heartbeat)}
                    </TableCell>
                    <TableCell className="font-mono text-xs text-muted-foreground">
                      {formatTime(w.registered_at)}
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
