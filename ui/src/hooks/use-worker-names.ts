import { useEffect, useState } from "react";
import { listWorkers } from "@/lib/api";

const EMPTY_MAP = new Map<string, string>();

export function useWorkerNames(): Map<string, string> {
  const [names, setNames] = useState<Map<string, string>>(EMPTY_MAP);

  useEffect(() => {
    let cancelled = false;
    listWorkers(200, 0)
      .then((data) => {
        if (cancelled) return;
        const map = new Map<string, string>();
        for (const w of data.items) {
          map.set(w.worker_id, w.name);
        }
        setNames(map);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, []);

  return names;
}
