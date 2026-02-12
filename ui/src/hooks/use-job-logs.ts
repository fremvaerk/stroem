import { useEffect, useState } from "react";
import { getAccessToken, getJobLogs } from "@/lib/api";

export function useJobLogs(
  jobId: string | undefined,
  jobStatus: string | null,
) {
  const [logs, setLogs] = useState("");
  const [isStreaming, setIsStreaming] = useState(false);
  const isActive = jobStatus === "pending" || jobStatus === "running";

  useEffect(() => {
    if (!jobId) return;

    let cancelled = false;

    // Fetch existing logs via REST
    getJobLogs(jobId)
      .then((data) => {
        if (!cancelled && data.logs) {
          setLogs(data.logs);
        }
      })
      .catch(() => {});

    // Only open WebSocket for active jobs
    if (!isActive) return;

    const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
    let url = `${protocol}//${window.location.host}/api/jobs/${jobId}/logs/stream`;

    const token = getAccessToken();
    if (token) {
      url += `?token=${encodeURIComponent(token)}`;
    }

    let wsLogs = "";
    const ws = new WebSocket(url);

    ws.onopen = () => {
      if (!cancelled) setIsStreaming(true);
    };

    ws.onmessage = (event) => {
      if (cancelled) return;
      wsLogs += event.data;
      setLogs(wsLogs);
    };

    ws.onclose = () => {
      if (!cancelled) setIsStreaming(false);
    };

    ws.onerror = () => {
      if (!cancelled) setIsStreaming(false);
    };

    return () => {
      cancelled = true;
      ws.close();
    };
  }, [jobId, isActive]);

  return { logs, isStreaming };
}
