import { useEffect, useState } from "react";
import { AlertCircle } from "lucide-react";
import { LogViewer } from "@/components/log-viewer";
import { getStepLogs } from "@/lib/api";

interface ServerEventsProps {
  jobId: string;
  jobStatus: string;
}

export function ServerEvents({ jobId, jobStatus }: ServerEventsProps) {
  const [logs, setLogs] = useState("");

  useEffect(() => {
    let cancelled = false;

    async function fetchLogs() {
      try {
        const data = await getStepLogs(jobId, "_server");
        if (!cancelled) setLogs(data.logs);
      } catch {
        // Silently ignore — no server events is the common case
      }
    }

    // Initial fetch
    fetchLogs();

    // Poll while job is active; also do one final fetch when it becomes
    // terminal (hook errors are written after a job completes/fails).
    const isActive = jobStatus === "pending" || jobStatus === "running";
    if (isActive) {
      const interval = setInterval(fetchLogs, 3000);
      return () => {
        cancelled = true;
        clearInterval(interval);
      };
    }

    return () => {
      cancelled = true;
    };
  }, [jobId, jobStatus]);

  if (!logs) return null;

  return (
    <div className="rounded-lg border border-amber-300 bg-amber-50 dark:border-amber-700 dark:bg-amber-950">
      <div className="flex items-center gap-2 border-b border-amber-200 px-4 py-2.5 dark:border-amber-800">
        <AlertCircle className="h-4 w-4 text-amber-600 dark:text-amber-400" />
        <span className="text-sm font-medium text-amber-800 dark:text-amber-200">
          Server Events
        </span>
      </div>
      <div className="p-2">
        <LogViewer logs={logs} isStreaming={false} />
      </div>
    </div>
  );
}
