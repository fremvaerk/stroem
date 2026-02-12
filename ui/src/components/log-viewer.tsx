import { useEffect, useRef } from "react";

interface LogViewerProps {
  logs: string;
  isStreaming: boolean;
}

export function LogViewer({ logs, isStreaming }: LogViewerProps) {
  const containerRef = useRef<HTMLPreElement>(null);
  const autoScrollRef = useRef(true);

  useEffect(() => {
    const el = containerRef.current;
    if (!el || !autoScrollRef.current) return;
    el.scrollTop = el.scrollHeight;
  }, [logs]);

  function handleScroll() {
    const el = containerRef.current;
    if (!el) return;
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
    autoScrollRef.current = atBottom;
  }

  return (
    <div className="relative">
      {isStreaming && (
        <div className="absolute right-3 top-3 flex items-center gap-1.5">
          <span className="h-2 w-2 animate-pulse rounded-full bg-green-500" />
          <span className="text-[10px] font-medium uppercase tracking-wider text-green-600 dark:text-green-400">
            Live
          </span>
        </div>
      )}
      <pre
        ref={containerRef}
        onScroll={handleScroll}
        className="max-h-[500px] min-h-[200px] overflow-auto rounded-lg bg-zinc-950 p-4 font-mono text-xs leading-relaxed text-zinc-300"
      >
        {logs || (
          <span className="text-zinc-600">Waiting for logs...</span>
        )}
      </pre>
    </div>
  );
}
