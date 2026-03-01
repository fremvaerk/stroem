import { useEffect, useRef, useState } from "react";
import { getAccessToken, getJobLogs } from "@/lib/api";

export function useJobLogs(
  jobId: string | undefined,
  jobStatus: string | null,
) {
  const [logs, setLogs] = useState("");
  const [isStreaming, setIsStreaming] = useState(false);
  const logsRef = useRef("");
  const wsRef = useRef<WebSocket | null>(null);
  const isActive = jobStatus === "pending" || jobStatus === "running";

  useEffect(() => {
    if (!jobId) return;

    let cancelled = false;

    async function init() {
      // 1. Fetch existing logs via REST first
      try {
        const data = await getJobLogs(jobId!);
        if (cancelled) return;
        if (data.logs) {
          logsRef.current = data.logs;
          setLogs(data.logs);
        }
      } catch {
        // ignore — logs may not exist yet
      }

      // 2. Only open WebSocket for active jobs, after REST completes
      if (!isActive || cancelled) return;

      const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
      let url = `${protocol}//${window.location.host}/api/jobs/${jobId}/logs/stream?skip_backfill=true`;

      const token = getAccessToken();
      if (token) {
        url += `&token=${encodeURIComponent(token)}`;
      }

      const ws = new WebSocket(url);
      wsRef.current = ws;

      ws.onopen = () => {
        if (!cancelled) setIsStreaming(true);
      };

      ws.onmessage = (event) => {
        if (cancelled) return;
        logsRef.current += event.data;
        setLogs(logsRef.current);
      };

      ws.onclose = () => {
        if (!cancelled) setIsStreaming(false);
      };

      ws.onerror = () => {
        if (!cancelled) setIsStreaming(false);
      };
    }

    init();

    return () => {
      cancelled = true;
      logsRef.current = "";
      wsRef.current?.close();
      wsRef.current = null;
    };
  }, [jobId, isActive]);

  return { logs, isStreaming };
}
