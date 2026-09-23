import { useCallback, useEffect, useMemo, useReducer, useRef, useState } from "react";
import { downloadStepLog, getStepLogs, getStepLogsFull } from "@/lib/api";
import { formatBytesIEC, splitLogLines } from "@/lib/log-lines";
import { fullStateOf, initialState, reduce } from "@/lib/step-log-state";
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
 * the user loads the full log — stitches every later tail onto it.
 *
 * All ordering lives in the pure model (`lib/step-log-state.ts`): every
 * request gets a seq from ONE counter that is never reset, and the model
 * orders responses by it. The hook only owns what is not state: no
 * overlapping polls within one effect run, dropping responses of an
 * earlier step/job (generation), the 64 MiB confirm, download errors and
 * the load progress.
 */
export function useStepLog(jobId: string, stepName: string, { enabled, pollMs }: Options): StepLog {
  const [state, dispatch] = useReducer(reduce, undefined, initialState);
  const [progressBytes, setProgressBytes] = useState(0);
  const [loading, setLoading] = useState(enabled);
  const [downloadError, setDownloadError] = useState(false);
  // Monotonic over every request for the component's lifetime — across
  // step changes too, so a seq from an old generation can never be
  // mistaken for a new one.
  const seqRef = useRef(0);
  const generationRef = useRef(0);
  // Only the most recently started load drives the progress indicator.
  const latestLoadSeqRef = useRef(0);

  // A different step (or job) starts from scratch.
  useEffect(() => {
    generationRef.current += 1;
    latestLoadSeqRef.current = 0;
    dispatch({ type: "reset" });
    setProgressBytes(0);
    setLoading(enabled);
    setDownloadError(false);
  }, [jobId, stepName, enabled]);

  useEffect(() => {
    if (!enabled) return;
    // The reset effect above runs first in the same commit, so this is the
    // generation the step now has.
    const generation = generationRef.current;
    // No overlapping polls. Each effect run gets its own flag, so the fetch
    // a `pollMs` change makes when the step goes terminal is never blocked
    // by the previous run's in-flight request; that one's response still
    // reaches the model, which orders it by seq.
    let inFlight = false;
    async function fetchTail() {
      if (inFlight) return;
      inFlight = true;
      const seq = ++seqRef.current;
      dispatch({ type: "pollStarted", seq });
      try {
        const data = await getStepLogs(jobId, stepName);
        if (generationRef.current !== generation) return;
        dispatch({ type: "pollDone", seq, tail: data });
      } catch {
        // Logs may not exist yet.
        if (generationRef.current === generation) dispatch({ type: "pollFailed", seq });
      } finally {
        inFlight = false;
        if (generationRef.current === generation) setLoading(false);
      }
    }
    void fetchTail();
    if (pollMs == null) return;
    const id = setInterval(fetchTail, pollMs);
    return () => clearInterval(id);
  }, [jobId, stepName, enabled, pollMs]);

  const tail = state.tail;
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
    const seq = ++seqRef.current;
    latestLoadSeqRef.current = seq;
    dispatch({ type: "loadStarted", seq });
    setProgressBytes(0);
    try {
      const text = await getStepLogsFull(jobId, stepName, (n) => {
        if (generationRef.current === generation && latestLoadSeqRef.current === seq) setProgressBytes(n);
      });
      if (generationRef.current !== generation) return;
      dispatch({ type: "loadDone", seq, text });
    } catch {
      if (generationRef.current === generation) dispatch({ type: "loadFailed", seq });
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
    lines: state.full ?? tailLines,
    truncated: state.full ? false : (tail?.truncated ?? false),
    returnedBytes: tail?.returned_bytes ?? 0,
    totalBytes: tail?.total_bytes ?? 0,
    loading,
    fullState: fullStateOf(state),
    progressBytes,
    loadFull: () => void loadFull(),
    download,
    downloadError,
  };
}
