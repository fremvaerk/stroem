import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { LOG_GAP_MARKER, estimateLineRows, splitLogLines } from "@/lib/log-lines";
import { elapsedLabel, formatClock } from "@/lib/log-time";
import { cn } from "@/lib/utils";

type TimeMode = "clock" | "elapsed";

const TIME_MODE_KEY = "stroem_log_time_mode";

function readTimeMode(): TimeMode {
  try {
    return localStorage.getItem(TIME_MODE_KEY) === "elapsed" ? "elapsed" : "clock";
  } catch {
    return "clock";
  }
}

function writeTimeMode(mode: TimeMode) {
  try {
    localStorage.setItem(TIME_MODE_KEY, mode);
  } catch {
    // storage may be unavailable (private mode); the mode is a convenience only
  }
}

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
  /**
   * Start of each attempt of the step, ascending epoch ms (`attemptStarts`
   * in lib/log-time). Enables the elapsed-time mode; absent or empty = clock only.
   */
  attemptStarts?: readonly number[];
}

/** text-xs (12px) × leading-relaxed (1.625). */
const LINE_HEIGHT_PX = 20;
const DEFAULT_CHARS_PER_ROW = 120;
const PROBE_CHARS = 20;
/** Timestamp column and horizontal padding, subtracted before chars/row. */
const RESERVED_PX = 110;
const NO_STARTS: readonly number[] = [];

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

/** One row; parsed on render, so only visible rows cost a JSON.parse. */
function LogLine({
  raw,
  mode,
  starts,
}: {
  raw: string;
  mode: TimeMode;
  starts: readonly number[];
}) {
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
  const clock = formatClock(parsed.ts);
  const elapsed = elapsedLabel(parsed.ts, starts);
  const [shown, other] = mode === "elapsed" ? [elapsed, clock] : [clock, elapsed];
  return (
    <div className="flex">
      {shown && (
        <span
          className="mr-3 min-w-[8ch] shrink-0 select-none text-zinc-600"
          title={other || undefined}
        >
          {shown}
        </span>
      )}
      <span
        className={parsed.stream === "stderr" ? "text-red-400" : "text-zinc-300"}
        data-stream={parsed.stream}
      >
        {parsed.line}
      </span>
    </div>
  );
}

function TimeModeToggle({
  mode,
  onChange,
}: {
  mode: TimeMode;
  onChange: (mode: TimeMode) => void;
}) {
  const options: [TimeMode, string][] = [
    ["clock", "Clock"],
    ["elapsed", "Elapsed"],
  ];
  return (
    <div
      role="group"
      aria-label="Timestamp format"
      className="flex overflow-hidden rounded border border-zinc-700 bg-zinc-900/90 text-[10px] font-medium uppercase tracking-wider"
    >
      {options.map(([value, label]) => (
        <button
          key={value}
          type="button"
          aria-pressed={mode === value}
          onClick={() => onChange(value)}
          className={cn(
            "px-1.5 py-0.5",
            mode === value ? "bg-zinc-700 text-zinc-100" : "text-zinc-500 hover:text-zinc-300",
          )}
        >
          {label}
        </button>
      ))}
    </div>
  );
}

export function LogViewer({
  logs,
  isStreaming,
  header,
  attemptStarts = NO_STARTS,
}: LogViewerProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const probeRef = useRef<HTMLSpanElement>(null);
  const autoScrollRef = useRef(true);
  const [following, setFollowing] = useState(true);
  const [charsPerRow, setCharsPerRow] = useState(DEFAULT_CHARS_PER_ROW);
  const [savedMode, setSavedMode] = useState(readTimeMode);
  const canShowElapsed = attemptStarts.length > 0;
  const mode: TimeMode = canShowElapsed ? savedMode : "clock";

  function changeMode(next: TimeMode) {
    setSavedMode(next);
    writeTimeMode(next);
  }
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
      <div className="overflow-hidden rounded-lg bg-zinc-950">
        {(isStreaming || canShowElapsed) && (
          <div className="flex items-center justify-end gap-3 px-4 pt-3">
            {isStreaming && (
              <div className="flex items-center gap-1.5">
                <span
                  className="h-2 w-2 animate-pulse rounded-full bg-green-500"
                  aria-hidden="true"
                />
                <span
                  className="text-[10px] font-medium uppercase tracking-wider text-green-600 dark:text-green-400"
                  aria-hidden="true"
                >
                  Live
                </span>
                <span className="sr-only">Log streaming is active</span>
              </div>
            )}
            {canShowElapsed && <TimeModeToggle mode={mode} onChange={changeMode} />}
          </div>
        )}
        <div
          ref={containerRef}
          onScroll={handleScroll}
          role="log"
          aria-label="Job execution logs"
          aria-live={following ? "polite" : "off"}
          className="relative max-h-[500px] min-h-[200px] overflow-auto p-4 font-mono text-xs leading-relaxed"
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
                  <LogLine raw={lines[item.index]} mode={mode} starts={attemptStarts} />
                </div>
              ))}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
