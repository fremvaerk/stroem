import { Download } from "lucide-react";
import { Button } from "@/components/ui/button";
import { formatBytesIEC } from "@/lib/log-lines";

export type FullLogState = "idle" | "loading" | "loaded" | "error";

interface LogTailBannerProps {
  lineCount: number;
  returnedBytes: number;
  totalBytes: number;
  fullState: FullLogState;
  progressBytes: number;
  onLoadFull: () => void;
  onDownload: () => void;
  downloadError?: boolean;
}

export function LogTailBanner({
  lineCount,
  returnedBytes,
  totalBytes,
  fullState,
  progressBytes,
  onLoadFull,
  onDownload,
  downloadError = false,
}: LogTailBannerProps) {
  const loading = fullState === "loading";
  const loaded = fullState === "loaded";
  return (
    <div
      data-testid="log-tail-banner"
      role="status"
      className="mb-2 flex flex-wrap items-center gap-2 rounded-md border border-amber-300 bg-amber-50 px-3 py-1.5 text-xs text-amber-900 dark:border-amber-700 dark:bg-amber-950 dark:text-amber-100"
    >
      <span className="grow">
        {loaded ? (
          <>Showing the full log (~{lineCount.toLocaleString("en-US")} lines).</>
        ) : (
          <>
            Showing the last ~{lineCount.toLocaleString("en-US")} lines ({formatBytesIEC(returnedBytes)} of up to{" "}
            {formatBytesIEC(totalBytes)}).
          </>
        )}
      </span>
      {fullState === "error" && (
        <span className="text-red-600 dark:text-red-400">Could not load the full log.</span>
      )}
      {downloadError && (
        <span className="text-red-600 dark:text-red-400">Could not download the log.</span>
      )}
      <Button type="button" size="sm" variant="outline" disabled={loading} onClick={onLoadFull}>
        {loading ? `Loading… ${formatBytesIEC(progressBytes)}` : loaded ? "Reload full log" : "Load full log"}
      </Button>
      <Button type="button" size="sm" variant="ghost" onClick={onDownload}>
        <Download className="mr-1 h-3.5 w-3.5" aria-hidden="true" />
        Download
      </Button>
    </div>
  );
}
