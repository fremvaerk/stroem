import { AlertCircle } from "lucide-react";
import { LogViewer } from "@/components/log-viewer";
import { LogTailBanner } from "@/components/log-tail-banner";
import { useStepLog } from "@/hooks/use-step-log";

interface ServerEventsProps {
  jobId: string;
  jobStatus: string;
}

export function ServerEvents({ jobId, jobStatus }: ServerEventsProps) {
  // Poll while the job is active. The switch to terminal changes `pollMs`,
  // which runs one final fetch: hook errors are written after completion.
  const isActive = jobStatus === "pending" || jobStatus === "running";
  const log = useStepLog(jobId, "_server", { enabled: true, pollMs: isActive ? 3000 : null });
  const showBanner = log.truncated || log.fullState === "loading" || log.fullState === "error";

  // An empty but truncated tail (its only record was torn) still shows the
  // banner, so the full log stays reachable.
  if (log.lines.length === 0 && !showBanner) return null;

  return (
    <div className="rounded-lg border border-amber-300 bg-amber-50 dark:border-amber-700 dark:bg-amber-950">
      <div className="flex items-center gap-2 border-b border-amber-200 px-4 py-2.5 dark:border-amber-800">
        <AlertCircle className="h-4 w-4 text-amber-600 dark:text-amber-400" />
        <span className="text-sm font-medium text-amber-800 dark:text-amber-200">Server Events</span>
      </div>
      <div className="p-2">
        <LogViewer
          logs={log.lines}
          isStreaming={false}
          header={
            showBanner ? (
              <LogTailBanner
                lineCount={log.lines.length}
                returnedBytes={log.returnedBytes}
                totalBytes={log.totalBytes}
                fullState={log.fullState}
                progressBytes={log.progressBytes}
                onLoadFull={log.loadFull}
                onDownload={log.download}
                downloadError={log.downloadError}
              />
            ) : null
          }
        />
      </div>
    </div>
  );
}
