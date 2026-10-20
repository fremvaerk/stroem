import { useEffect, useState } from "react";
import { Link } from "react-router";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { statusStyles } from "@/lib/status-styles";
import { LastRunCell, MissedRunsPill } from "@/components/trigger-run-status";
import { listAttentionTriggers } from "@/lib/api";
import type { AttentionTrigger } from "@/lib/types";
import { cn } from "@/lib/utils";

const REFRESH_MS = 30_000;

export function TriggersAttentionCard() {
  const [triggers, setTriggers] = useState<AttentionTrigger[]>([]);

  useEffect(() => {
    let cancelled = false;

    async function load() {
      try {
        const data = await listAttentionTriggers();
        if (!cancelled) setTriggers(data);
      } catch {
        if (!cancelled) setTriggers([]);
      }
    }

    load();
    const interval = setInterval(load, REFRESH_MS);
    return () => {
      cancelled = true;
      clearInterval(interval);
    };
  }, []);

  if (triggers.length === 0) return null;

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">Triggers that need attention</CardTitle>
      </CardHeader>
      <CardContent>
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Workspace</TableHead>
              <TableHead>Trigger</TableHead>
              <TableHead>Problem</TableHead>
              <TableHead>Last run</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {triggers.map((t) => {
              const href = `/workspaces/${encodeURIComponent(t.workspace)}`;
              return (
                <TableRow key={`${t.workspace}/${t.name}`}>
                  <TableCell>
                    <Link to={href} className="hover:underline">
                      {t.workspace}
                    </Link>
                  </TableCell>
                  <TableCell>
                    <Link to={href} className="font-medium hover:underline">
                      {t.name}
                    </Link>
                  </TableCell>
                  <TableCell>
                    <span className="flex flex-wrap items-center gap-1.5">
                      {t.missed && <MissedRunsPill missed={t.missed} />}
                      {t.last_run_failed && (
                        <Badge
                          variant="secondary"
                          className={cn("text-xs", statusStyles.failed)}
                        >
                          last run failed
                        </Badge>
                      )}
                    </span>
                  </TableCell>
                  <TableCell>
                    <LastRunCell trigger={t} />
                  </TableCell>
                </TableRow>
              );
            })}
          </TableBody>
        </Table>
      </CardContent>
    </Card>
  );
}
