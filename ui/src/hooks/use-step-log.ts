import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { downloadStepLog, getStepLogs, getStepLogsFull } from "@/lib/api";
import { formatBytesIEC, splitLogLines } from "@/lib/log-lines";
import { fullStateOf, initialState, reduce, type StepLogEvent } from "@/lib/step-log-state";
import type { FullLogState } from "@/components/log-tail-banner";

/** Above this the browser would hold a very large string: ask first. */
export const FULL_LOAD_CONFIRM_BYTES = 64 * 1024 * 1024;
/** A tail poll that has not answered by then is given up on. */
export const POLL_TIMEOUT_MS = 30_000;
/** A full load with no progress for this long is given up on; a slow but
 * moving download is never cut (the timer restarts on every chunk). */
export const FULL_LOAD_STALL_MS = 60_000;

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

interface TrackedRequest {
  controller: AbortController;
  timer: ReturnType<typeof setTimeout> | null;
}

/**
 * The log panel's data: polls the step's tail, keeps the last non-empty
 * body when a poll lands on a replica without this job's chunks, and — once
 * the user loads the full log — stitches every later tail onto it.
 *
 * All ordering lives in the pure model (`lib/step-log-state.ts`): every
 * request gets a seq from ONE counter that is never reset, and the model
 * orders responses by it. The hook owns what is not state: no overlapping
 * polls within one effect run, dropping responses of an earlier step/job
 * (generation), one `AbortController` per request with the time bounds
 * above, the 64 MiB confirm, download errors and the load progress. A
 * request the model stops counting as in flight — settled, retired by a
 * newer load, evicted by the cap, or reset away — is aborted.
 */
export function useStepLog(jobId: string, stepName: string, { enabled, pollMs }: Options): StepLog {
  const [state, setState] = useState(initialState);
  const [progressBytes, setProgressBytes] = useState(0);
  const [loading, setLoading] = useState(enabled);
  const [downloadError, setDownloadError] = useState(false);
  // The model, readable synchronously by the request handlers; `state`
  // mirrors it for rendering.
  const modelRef = useRef(state);
  const requestsRef = useRef(new Map<number, TrackedRequest>());
  const mountedRef = useRef(true);
  // Monotonic over every request for the component's lifetime — across
  // step changes too, so a seq from an old generation can never be
  // mistaken for a new one.
  const seqRef = useRef(0);
  const generationRef = useRef(0);
  // Only the most recently started load drives the progress indicator.
  const latestLoadSeqRef = useRef(0);

  const send = useCallback((event: StepLogEvent) => {
    const next = reduce(modelRef.current, event);
    if (next === modelRef.current) return;
    modelRef.current = next;
    // A request still tracked here but no longer in flight in the model is
    // one the model gave up on while it was running — retired by a newer
    // load, evicted by the cap, or reset away: abort it. (A request that
    // settled on its own released its entry before dispatching.)
    for (const [seq, request] of requestsRef.current) {
      if (next.inFlightPolls.has(seq) || next.inFlightLoads.has(seq)) continue;
      abandon(request);
      requestsRef.current.delete(seq);
    }
    if (mountedRef.current) setState(next);
  }, []);

  // Unmount aborts whatever is still in flight.
  useEffect(() => {
    mountedRef.current = true;
    const requests = requestsRef.current;
    return () => {
      mountedRef.current = false;
      for (const request of requests.values()) abandon(request);
      requests.clear();
    };
  }, []);

  // A different step (or job) starts from scratch; `reset` empties the
  // in-flight sets, which aborts the old generation's requests.
  useEffect(() => {
    generationRef.current += 1;
    latestLoadSeqRef.current = 0;
    send({ type: "reset" });
    setProgressBytes(0);
    setLoading(enabled);
    setDownloadError(false);
  }, [jobId, stepName, enabled, send]);

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
      const controller = new AbortController();
      requestsRef.current.set(seq, {
        controller,
        timer: setTimeout(() => controller.abort(), POLL_TIMEOUT_MS),
      });
      send({ type: "pollStarted", seq });
      try {
        const data = await settleWith(getStepLogs(jobId, stepName, controller.signal), controller.signal);
        release(requestsRef.current, seq);
        if (generationRef.current !== generation) return;
        send({ type: "pollDone", seq, tail: data });
      } catch {
        // Logs may not exist yet, or the poll timed out / was aborted.
        release(requestsRef.current, seq);
        if (generationRef.current === generation) send({ type: "pollFailed", seq });
      } finally {
        inFlight = false;
        if (generationRef.current === generation) setLoading(false);
      }
    }
    void fetchTail();
    if (pollMs == null) return;
    const id = setInterval(fetchTail, pollMs);
    return () => clearInterval(id);
  }, [jobId, stepName, enabled, pollMs, send]);

  const totalBytes = state.meta?.total_bytes ?? 0;
  const loadFull = useCallback(async () => {
    if (
      totalBytes > FULL_LOAD_CONFIRM_BYTES &&
      !window.confirm(
        `This log is up to ${formatBytesIEC(totalBytes)}. Loading all of it may slow this tab down; Download is lighter. Load it anyway?`,
      )
    ) {
      return;
    }
    const generation = generationRef.current;
    const seq = ++seqRef.current;
    latestLoadSeqRef.current = seq;
    const controller = new AbortController();
    const request: TrackedRequest = { controller, timer: null };
    const armStall = () => {
      if (request.timer !== null) clearTimeout(request.timer);
      request.timer = setTimeout(() => controller.abort(), FULL_LOAD_STALL_MS);
    };
    armStall();
    requestsRef.current.set(seq, request);
    send({ type: "loadStarted", seq });
    setProgressBytes(0);
    try {
      const text = await settleWith(
        getStepLogsFull(
          jobId,
          stepName,
          (n) => {
            if (controller.signal.aborted) return;
            armStall();
            if (generationRef.current === generation && latestLoadSeqRef.current === seq) setProgressBytes(n);
          },
          controller.signal,
        ),
        controller.signal,
      );
      release(requestsRef.current, seq);
      if (generationRef.current !== generation) return;
      send({ type: "loadDone", seq, text });
    } catch {
      // A stall timeout lands here too; a retired load's abort is ignored
      // by the model (its seq is no longer in flight).
      release(requestsRef.current, seq);
      if (generationRef.current === generation) send({ type: "loadFailed", seq });
    }
  }, [jobId, stepName, totalBytes, send]);

  const download = useCallback(() => {
    const generation = generationRef.current;
    setDownloadError(false);
    void downloadStepLog(jobId, stepName).catch(() => {
      if (generationRef.current === generation) setDownloadError(true);
    });
  }, [jobId, stepName]);

  const body = state.body;
  const tailLines = useMemo(() => splitLogLines(body?.logs ?? ""), [body]);

  return {
    lines: state.full ?? tailLines,
    truncated: state.full ? false : (state.meta?.truncated ?? false),
    returnedBytes: body?.returned_bytes ?? 0,
    totalBytes,
    loading,
    fullState: fullStateOf(state),
    progressBytes,
    loadFull: () => void loadFull(),
    download,
    downloadError,
  };
}

/** The request settled by itself: stop tracking it, leave its signal alone. */
function release(requests: Map<number, TrackedRequest>, seq: number) {
  const request = requests.get(seq);
  if (!request) return;
  if (request.timer !== null) clearTimeout(request.timer);
  requests.delete(seq);
}

/** The request is given up on while still running. */
function abandon(request: TrackedRequest) {
  if (request.timer !== null) clearTimeout(request.timer);
  request.controller.abort();
}

/** Settle as soon as `signal` aborts, whether or not `promise` ever does —
 * the model must not depend on a hung transport honouring the abort. */
function settleWith<T>(promise: Promise<T>, signal: AbortSignal): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const onAbort = () => reject(signal.reason ?? new Error("aborted"));
    if (signal.aborted) {
      onAbort();
      return;
    }
    signal.addEventListener("abort", onAbort, { once: true });
    promise.then(
      (value) => {
        signal.removeEventListener("abort", onAbort);
        resolve(value);
      },
      (err: unknown) => {
        signal.removeEventListener("abort", onAbort);
        reject(err);
      },
    );
  });
}
