import { useState, useEffect, useCallback } from "react";

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
 * @example
 * const fetcher = useCallback(() => listWorkers(PAGE_SIZE, offset), [offset]);
 * const { data, loading } = useAsyncData(fetcher, { pollInterval: 5000 });
 */
export function useAsyncData<T>(
  fetcher: () => Promise<T>,
  options: UseAsyncDataOptions = {},
): { data: T | null; loading: boolean; refresh: () => void } {
  const [data, setData] = useState<T | null>(null);
  const [loading, setLoading] = useState(true);

  const load = useCallback(async () => {
    try {
      const result = await fetcher();
      setData(result);
    } catch {
      // Silently ignore — matches existing behavior
    } finally {
      setLoading(false);
    }
  }, [fetcher]);

  useEffect(() => {
    setLoading(true);
    load();
    if (options.pollInterval && options.pollInterval > 0) {
      const interval = setInterval(load, options.pollInterval);
      return () => clearInterval(interval);
    }
  }, [load, options.pollInterval]);

  return { data, loading, refresh: load };
}
