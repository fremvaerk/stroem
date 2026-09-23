import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { downloadStepLog, getStepLogs, getStepLogsFull, type LogTail } from "@/lib/api";
import { appendTail } from "@/lib/log-tail";
import { formatBytesIEC, splitLogLines } from "@/lib/log-lines";
import type { FullLogState } from "@/components/log-tail-banner";

/** Above this the browser would hold a very large string: ask first. */
export const FULL_LOAD_CONFIRM_BYTES = 64 * 1024 * 1024;

export interface StepLog {
  /** Tail lines, or the full log (with gap markers) once loaded. */
  lines: readonly string[];
  truncated: boolean;
  returnedBytes: number;
  totalBytes: number;
  loading: boolean;
  fullState: FullLogState;
  progressBytes: number;
  loadFull: () => void;
  download: () => void;
  downloadError: boolean;
}

interface Options {
  enabled: boolean;
  /** Poll interval while the step is live; `null` fetches once. */
  pollMs: number | null;
}

/**
 * The log panel's data: polls the step's tail, keeps the last non-empty
 * body when a poll lands on a replica without this job's chunks, and — once
 * the user loads the full log — stitches every later tail onto it with
 * `appendTail`.
 */
export function useStepLog(jobId: string, stepName: string, { enabled, pollMs }: Options): StepLog {
  const [tail, setTail] = useState<LogTail | null>(null);
  const [fullLines, setFullLines] = useState<string[] | null>(null);
  const [fullState, setFullState] = useState<FullLogState>("idle");
  const [progressBytes, setProgressBytes] = useState(0);
  const [loading, setLoading] = useState(enabled);
  const [downloadError, setDownloadError] = useState(false);
  const hasLogsRef = useRef(false);
  const fullRef = useRef<string[] | null>(null);
  const generationRef = useRef(0);

  // A different step (or job) starts from scratch.
  useEffect(() => {
    generationRef.current += 1;
    hasLogsRef.current = false;
    fullRef.current = null;
    setTail(null);
    setFullLines(null);
    setFullState("idle");
    setProgressBytes(0);
    setLoading(enabled);
    setDownloadError(false);
  }, [jobId, stepName, enabled]);

  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    // A slow poll must not resolve after a later one and overwrite newer
    // data (or, once the full view is loaded, appendTail a stale tail after
    // a newer one — a spurious gap marker). Each effect run gets its own
    // flag, so the fetch a `pollMs` change makes when the step goes
    // terminal is never blocked by the previous run's in-flight request
    // (that one's result is dropped by `cancelled` instead).
    let inFlight = false;
    async function fetchTail() {
      if (inFlight) return;
      inFlight = true;
      try {
        const data = await getStepLogs(jobId, stepName);
        if (cancelled) return;
        // With multi-replica servers a poll can land on a replica that has
        // not received this job's chunks and return "". Keep what we have.
        if (data.logs) {
          hasLogsRef.current = true;
          setTail(data);
          if (fullRef.current) {
            const { lines } = appendTail(fullRef.current, splitLogLines(data.logs));
            fullRef.current = lines;
            setFullLines(lines);
          }
        } else if (!hasLogsRef.current) {
          setTail(data);
        }
      } catch {
        // Logs may not exist yet.
      } finally {
        inFlight = false;
        if (!cancelled) setLoading(false);
      }
    }
    void fetchTail();
    if (pollMs == null) {
      return () => {
        cancelled = true;
      };
    }
    const id = setInterval(fetchTail, pollMs);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, [jobId, stepName, enabled, pollMs]);

  const loadFull = useCallback(async () => {
    if (
      tail &&
      tail.total_bytes > FULL_LOAD_CONFIRM_BYTES &&
      !window.confirm(
        `This log is up to ${formatBytesIEC(tail.total_bytes)}. Loading all of it may slow this tab down; Download is lighter. Load it anyway?`,
      )
    ) {
      return;
    }
    const generation = generationRef.current;
    setFullState("loading");
    setProgressBytes(0);
    try {
      const text = await getStepLogsFull(jobId, stepName, (n) => {
        if (generationRef.current === generation) setProgressBytes(n);
      });
      if (generationRef.current !== generation) return;
      const lines = splitLogLines(text);
      fullRef.current = lines;
      setFullLines(lines);
      setFullState("loaded");
    } catch {
      if (generationRef.current === generation) setFullState("error");
    }
  }, [jobId, stepName, tail]);

  const download = useCallback(() => {
    const generation = generationRef.current;
    setDownloadError(false);
    void downloadStepLog(jobId, stepName).catch(() => {
      if (generationRef.current === generation) setDownloadError(true);
    });
  }, [jobId, stepName]);

  const tailLines = useMemo(() => splitLogLines(tail?.logs ?? ""), [tail]);

  return {
    lines: fullLines ?? tailLines,
    truncated: fullLines ? false : (tail?.truncated ?? false),
    returnedBytes: tail?.returned_bytes ?? 0,
    totalBytes: tail?.total_bytes ?? 0,
    loading,
    fullState,
    progressBytes,
    loadFull: () => void loadFull(),
    download,
    downloadError,
  };
}
