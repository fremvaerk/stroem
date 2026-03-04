import { useState, useEffect, useCallback, useRef } from "react";

interface UseAsyncDataOptions {
  /** Polling interval in ms. Pass 0 or undefined to disable polling. */
  pollInterval?: number;
}

/**
 * Hook for async data fetching with optional polling.
 *
 * Pass a stable (memoized) fetcher function. When the fetcher reference
 * changes the hook treats it as a new fetch trigger, matching the behavior
 * of putting the original deps in a useCallback dependency array.
 *
 * Uses a request-ID ref to discard responses from superseded fetches,
 * preventing stale data from overwriting fresher results (race condition
 * guard). Resets data and loading state when the fetcher changes.
 *
 * Background polls (interval ticks) skip `setLoading(true)` to avoid
 * spinner flash and skip `setData()` entirely when the response is
 * identical to the last one (deep equality via JSON.stringify), preventing
 * unnecessary re-renders / table redraws.
 *
 * @example
 * const fetcher = useCallback(() => listWorkers(PAGE_SIZE, offset), [offset]);
 * const { data, loading, error } = useAsyncData(fetcher, { pollInterval: 5000 });
 */
export function useAsyncData<T>(
  fetcher: () => Promise<T>,
  options: UseAsyncDataOptions = {},
): { data: T | null; loading: boolean; error: string | null; refresh: () => void } {
  const [data, setData] = useState<T | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  // Monotonically increasing ID: each new fetch increments this value.
  // A response is only applied when its captured ID still matches the ref,
  // discarding responses from superseded (stale) fetches.
  const requestIdRef = useRef(0);

  // Last serialized response — used to skip redundant setData on polls.
  const lastJsonRef = useRef("");

  const load = useCallback(
    async (isBackground = false) => {
      const currentId = ++requestIdRef.current;
      if (!isBackground) {
        // Foreground loads show a spinner and immediately clear any previous
        // error so the UI does not show stale error text alongside the spinner.
        setLoading(true);
        setError(null);
      }
      try {
        const result = await fetcher();
        if (currentId === requestIdRef.current) {
          // Clear any previous error on success regardless of foreground/background.
          // A successful background poll means the transient error has resolved.
          setError(null);
          const json = JSON.stringify(result);
          if (json !== lastJsonRef.current) {
            lastJsonRef.current = json;
            setData(result);
          }
        }
      } catch (err) {
        if (currentId === requestIdRef.current) {
          setError(err instanceof Error ? err.message : "Failed to load");
        }
      } finally {
        if (currentId === requestIdRef.current) {
          setLoading(false);
        }
      }
    },
    [fetcher],
  );

  useEffect(() => {
    // Reset visible state immediately when the fetcher changes so consumers
    // never see data from a previous fetcher while the new one is in flight.
    setData(null);
    setError(null);
    lastJsonRef.current = "";

    load();

    if (options.pollInterval && options.pollInterval > 0) {
      const interval = setInterval(() => load(true), options.pollInterval);
      return () => {
        requestIdRef.current++;
        clearInterval(interval);
      };
    }

    return () => {
      // Invalidate any in-flight request so stale responses are discarded.
      // requestIdRef is a plain counter (not a DOM node) — reading .current
      // at cleanup time is intentional: we want the latest value, not the
      // one captured when the effect ran.
      // eslint-disable-next-line react-hooks/exhaustive-deps
      requestIdRef.current++;
    };
  }, [load, options.pollInterval]);

  return { data, loading, error, refresh: load };
}
