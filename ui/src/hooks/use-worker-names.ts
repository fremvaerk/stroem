import { useEffect, useState } from "react";
import { listWorkers } from "@/lib/api";

const EMPTY_MAP = new Map<string, string>();

// Module-level singleton cache so all components share a single fetch.
// Multiple calls to useWorkerNames() on the same render cycle will all
// attach to the same in-flight promise rather than issuing duplicate requests.
let cachedNames: Map<string, string> | null = null;
let fetchPromise: Promise<Map<string, string>> | null = null;

function fetchWorkerNames(): Promise<Map<string, string>> {
  // Return immediately if we have a warm cache.
  if (cachedNames !== null) return Promise.resolve(cachedNames);
  // Coalesce concurrent callers onto the same in-flight request.
  if (fetchPromise !== null) return fetchPromise;

  fetchPromise = listWorkers(200, 0)
    .then((data) => {
      const map = new Map<string, string>();
      for (const w of data.items) {
        map.set(w.worker_id, w.name);
      }
      cachedNames = map;
      // Expire the cache after 60 s so the data stays reasonably fresh
      // without hammering the server on every render.
      setTimeout(() => {
        cachedNames = null;
        fetchPromise = null;
      }, 60_000);
      return map;
    })
    .catch(() => {
      // Clear the promise so the next caller can retry.
      fetchPromise = null;
      return new Map<string, string>();
    });

  return fetchPromise;
}

export function useWorkerNames(): Map<string, string> {
  // Initialise from the cache when it is already warm (avoids a render
  // with an empty map on components that mount after the first fetch).
  const [names, setNames] = useState<Map<string, string>>(
    cachedNames ?? EMPTY_MAP,
  );

  useEffect(() => {
    let cancelled = false;
    fetchWorkerNames().then((map) => {
      if (!cancelled) setNames(map);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  return names;
}
