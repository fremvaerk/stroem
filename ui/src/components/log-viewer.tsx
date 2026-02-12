import { useEffect, useMemo, useRef } from "react";

interface ParsedLogLine {
  ts: string;
  stream: string;
  step: string;
  line: string;
}

interface LogViewerProps {
  logs: string;
  isStreaming: boolean;
}

function parseLogLine(raw: string): ParsedLogLine | null {
  try {
    const parsed = JSON.parse(raw);
    if (parsed && typeof parsed.line === "string") {
      return {
        ts: parsed.ts || "",
        stream: parsed.stream || "stdout",
        step: parsed.step || "",
        line: parsed.line,
      };
    }
  } catch {
    // Not JSON — legacy plain text line
  }
  return null;
}

function formatTimestamp(ts: string): string {
  if (!ts) return "";
  try {
    const d = new Date(ts);
    return d.toISOString().substring(11, 23); // HH:MM:SS.mmm
  } catch {
    return "";
  }
}

export function LogViewer({ logs, isStreaming }: LogViewerProps) {
  const containerRef = useRef<HTMLDivElement>(null);
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

  const lines = useMemo(() => {
    if (!logs) return [];
    return logs.split("\n").filter((l) => l.length > 0);
  }, [logs]);

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
      <div
        ref={containerRef}
        onScroll={handleScroll}
        className="max-h-[500px] min-h-[200px] overflow-auto rounded-lg bg-zinc-950 p-4 font-mono text-xs leading-relaxed"
      >
        {lines.length === 0 ? (
          <span className="text-zinc-600">Waiting for logs...</span>
        ) : (
          lines.map((raw, i) => {
            const parsed = parseLogLine(raw);
            if (parsed) {
              const ts = formatTimestamp(parsed.ts);
              const isStderr = parsed.stream === "stderr";
              return (
                <div key={i} className="flex">
                  {ts && (
                    <span className="mr-3 shrink-0 select-none text-zinc-600">
                      {ts}
                    </span>
                  )}
                  <span
                    className={isStderr ? "text-red-400" : "text-zinc-300"}
                    data-stream={parsed.stream}
                  >
                    {parsed.line}
                  </span>
                </div>
              );
            }
            // Legacy plain text line
            return (
              <div key={i} className="text-zinc-300">
                {raw}
              </div>
            );
          })
        )}
      </div>
    </div>
  );
}
