import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { LOG_GAP_MARKER, estimateLineRows, splitLogLines } from "@/lib/log-lines";

interface ParsedLogLine {
  ts: string;
  stream: string;
  step: string;
  line: string;
}

interface LogViewerProps {
  /** A raw JSONL body, or lines already split (may contain LOG_GAP_MARKER). */
  logs: string | readonly string[];
  isStreaming: boolean;
  /** Rendered above the log, e.g. the tail banner. */
  header?: ReactNode;
}

/** text-xs (12px) × leading-relaxed (1.625). */
const LINE_HEIGHT_PX = 20;
const DEFAULT_CHARS_PER_ROW = 120;
const PROBE_CHARS = 20;
/** Timestamp column and horizontal padding, subtracted before chars/row. */
const RESERVED_PX = 140;

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
    return new Date(ts).toISOString().substring(11, 23); // HH:MM:SS.mmm
  } catch {
    return "";
  }
}

/** One row; parsed on render, so only visible rows cost a JSON.parse. */
function LogLine({ raw }: { raw: string }) {
  if (raw === LOG_GAP_MARKER) {
    return (
      <div
        role="separator"
        data-testid="log-gap"
        className="my-1 border-t border-dashed border-amber-500/60 pt-0.5 text-center text-[10px] uppercase tracking-wider text-amber-500"
      >
        Lines missing here — reload the full log to fill the gap
      </div>
    );
  }
  const parsed = parseLogLine(raw);
  if (!parsed) return <div className="text-zinc-300">{raw}</div>;
  const ts = formatTimestamp(parsed.ts);
  return (
    <div className="flex">
      {ts && <span className="mr-3 shrink-0 select-none text-zinc-600">{ts}</span>}
      <span
        className={parsed.stream === "stderr" ? "text-red-400" : "text-zinc-300"}
        data-stream={parsed.stream}
      >
        {parsed.line}
      </span>
    </div>
  );
}

export function LogViewer({ logs, isStreaming, header }: LogViewerProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const probeRef = useRef<HTMLSpanElement>(null);
  const autoScrollRef = useRef(true);
  const [following, setFollowing] = useState(true);
  const [charsPerRow, setCharsPerRow] = useState(DEFAULT_CHARS_PER_ROW);
  const lines = useMemo(() => (typeof logs === "string" ? splitLogLines(logs) : logs), [logs]);

  const virtualizer = useVirtualizer({
    count: lines.length,
    getScrollElement: () => containerRef.current,
    estimateSize: (i) => estimateLineRows(lines[i] ?? "", charsPerRow) * LINE_HEIGHT_PX,
    overscan: 20,
    // No layout before the first paint (and none in jsdom); the observed
    // size replaces this.
    initialRect: { width: 800, height: 500 },
  });

  // Characters per row from one measured character and the container width.
  useEffect(() => {
    const el = containerRef.current;
    const probe = probeRef.current;
    if (!el || !probe) return;
    const update = () => {
      const charWidth = probe.getBoundingClientRect().width / PROBE_CHARS;
      if (charWidth > 0 && el.clientWidth > 0) {
        setCharsPerRow(Math.max(20, Math.floor((el.clientWidth - RESERVED_PX) / charWidth)));
      }
    };
    update();
    const observer = new ResizeObserver(update);
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

  // Follow the end while the user is at the bottom.
  useEffect(() => {
    if (autoScrollRef.current && lines.length > 0) {
      virtualizer.scrollToIndex(lines.length - 1, { align: "end" });
    }
  }, [lines, virtualizer]);

  function handleScroll() {
    const el = containerRef.current;
    if (!el) return;
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
    autoScrollRef.current = atBottom;
    setFollowing((prev) => (prev === atBottom ? prev : atBottom));
  }

  return (
    <div>
      {header}
      <div className="relative">
        {isStreaming && (
          <div className="absolute right-3 top-3 z-10 flex items-center gap-1.5">
            <span className="h-2 w-2 animate-pulse rounded-full bg-green-500" aria-hidden="true" />
            <span
              className="text-[10px] font-medium uppercase tracking-wider text-green-600 dark:text-green-400"
              aria-hidden="true"
            >
              Live
            </span>
            <span className="sr-only">Log streaming is active</span>
          </div>
        )}
        <div
          ref={containerRef}
          onScroll={handleScroll}
          role="log"
          aria-label="Job execution logs"
          aria-live={following ? "polite" : "off"}
          className="relative max-h-[500px] min-h-[200px] overflow-auto rounded-lg bg-zinc-950 p-4 font-mono text-xs leading-relaxed"
        >
          <span ref={probeRef} aria-hidden="true" className="invisible absolute whitespace-pre">
            {"0".repeat(PROBE_CHARS)}
          </span>
          {lines.length === 0 ? (
            <span className="text-zinc-600">Waiting for logs...</span>
          ) : (
            <div className="relative w-full" style={{ height: virtualizer.getTotalSize() }}>
              {virtualizer.getVirtualItems().map((item) => (
                <div
                  key={item.key}
                  data-index={item.index}
                  ref={virtualizer.measureElement}
                  className="absolute left-0 top-0 w-full"
                  style={{ transform: `translateY(${item.start}px)` }}
                >
                  <LogLine raw={lines[item.index]} />
                </div>
              ))}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
