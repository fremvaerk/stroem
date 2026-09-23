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
  // Monotonic counter over every `fetchTail`/`loadFull` REQUEST (assigned
  // when it starts, not when it resolves), so completions can be ordered by
  // when they were issued instead of by arrival order.
  const requestSeqRef = useRef(0);
  // The sequence number of the in-flight full load, or null when none is
  // in flight. A poll whose request started after this (its own seq is
  // greater) is one that ran DURING the load; accumulated in `pendingRef`
  // instead of the ordinary "append to fullRef" path, because there may be
  // no full view yet (first load) or only a now-superseded one (reload).
  const loadSeqRef = useRef<number | null>(null);
  // Every non-empty poll fetched during the in-flight load, stitched onto
  // one another in request order as they arrive; `loadFull`'s completion
  // stitches the whole accumulator onto its snapshot in one go, so nothing
  // fetched during the load is lost. Reset whenever a load starts or ends.
  const pendingRef = useRef<string[] | null>(null);
  const generationRef = useRef(0);

  // A different step (or job) starts from scratch.
  useEffect(() => {
    generationRef.current += 1;
    hasLogsRef.current = false;
    fullRef.current = null;
    requestSeqRef.current = 0;
    loadSeqRef.current = null;
    pendingRef.current = null;
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
      const seq = ++requestSeqRef.current;
      try {
        const data = await getStepLogs(jobId, stepName);
        if (cancelled) return;
        // With multi-replica servers a poll can land on a replica that has
        // not received this job's chunks and return "". Keep what we have.
        if (data.logs) {
          hasLogsRef.current = true;
          setTail(data);
          const lines = splitLogLines(data.logs);
          if (loadSeqRef.current !== null && seq > loadSeqRef.current) {
            // Started after the in-flight load began: hold it rather than
            // touch fullRef (stale during a reload, absent on a first
            // load) — `loadFull` stitches every held tail on completion.
            pendingRef.current = pendingRef.current ? appendTail(pendingRef.current, lines).lines : lines;
          } else if (fullRef.current) {
            const { lines: stitched } = appendTail(fullRef.current, lines);
            fullRef.current = stitched;
            setFullLines(stitched);
          }
        } else if (!hasLogsRef.current) {
          setTail(data);
        } else if (data.truncated) {
          // A scan cap or an over-window newest line can come back empty
          // AND truncated once a body was seen; the cold-replica case this
          // guard exists for always answers `truncated: false`. Keep the
          // body but adopt the flag (and the larger bound) so the banner
          // doesn't hide behind stale metadata.
          setTail((prev) =>
            prev ? { ...prev, truncated: true, total_bytes: Math.max(prev.total_bytes, data.total_bytes) } : data,
          );
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
    // Requests (not responses) are what order this: a poll whose request
    // starts after `loadSeq` ran DURING this load, whenever it resolves.
    const loadSeq = ++requestSeqRef.current;
    loadSeqRef.current = loadSeq;
    pendingRef.current = null;
    setFullState("loading");
    setProgressBytes(0);
    try {
      const text = await getStepLogsFull(jobId, stepName, (n) => {
        if (generationRef.current === generation) setProgressBytes(n);
      });
      if (generationRef.current !== generation) return;
      const lines = splitLogLines(text);
      // Stitch every tail that arrived during the load onto the snapshot in
      // one go. appendTail is idempotent: a tail already reflected at the
      // snapshot's end is a no-op, a newer one appends exactly its new
      // lines, and a disjoint one adds a gap marker.
      const stitched = pendingRef.current ? appendTail(lines, pendingRef.current).lines : lines;
      fullRef.current = stitched;
      setFullLines(stitched);
      setFullState("loaded");
    } catch {
      if (generationRef.current === generation) setFullState("error");
    } finally {
      // Guard against a stale, already-superseded load's cleanup clearing
      // a newer load's (or a newer generation's) in-flight state.
      if (loadSeqRef.current === loadSeq) {
        loadSeqRef.current = null;
        pendingRef.current = null;
      }
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
