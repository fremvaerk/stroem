import { useMemo } from "react";
import { formatDurationMs } from "@/lib/formatting";

interface SparklineProps {
  /** Datapoints ordered newest-first; rendered left-to-right oldest-first. */
  values: number[];
  /** Optional p50 reference line (same unit as values). */
  p50?: number | null;
  /** Optional p95 reference line. */
  p95?: number | null;
  /** Pixel width. Default 240. */
  width?: number;
  /** Pixel height. Default 48. */
  height?: number;
  /** Accessible label describing what the chart shows. */
  ariaLabel?: string;
}

/**
 * Tiny SVG line+bar chart for duration history. Reference lines for p50/p95
 * give the reader instant context for whether the latest run was typical.
 *
 * Renders nothing when there are no values — caller is expected to gate
 * display on sample size.
 */
export function Sparkline({
  values,
  p50,
  p95,
  width = 240,
  height = 48,
  ariaLabel = "Duration history",
}: SparklineProps) {
  const { points, yForValue, ordered, xStep } = useMemo(() => {
    const ordered = [...values].reverse(); // oldest-first for L→R reading
    if (ordered.length === 0) {
      return { points: "", yForValue: () => 0, ordered, xStep: 0 };
    }
    // Include reference lines in the y-range so they don't get clipped.
    const candidates = ordered.slice();
    if (p50 != null) candidates.push(p50);
    if (p95 != null) candidates.push(p95);
    const max = Math.max(...candidates, 1);
    const min = 0; // anchored at 0 to make magnitude differences readable
    const xStep = ordered.length > 1 ? width / (ordered.length - 1) : 0;
    const yForValue = (v: number) => {
      const t = (v - min) / (max - min || 1);
      return height - t * (height - 4) - 2; // 2px top/bottom padding
    };
    const points = ordered
      .map((v, i) => `${i * xStep},${yForValue(v)}`)
      .join(" ");
    return { points, yForValue, ordered, xStep };
  }, [values, p50, p95, width, height]);

  if (ordered.length === 0) return null;

  const latest = ordered[ordered.length - 1];

  return (
    <svg
      width={width}
      height={height}
      viewBox={`0 0 ${width} ${height}`}
      role="img"
      aria-label={`${ariaLabel}: ${ordered.length} runs, latest ${formatDurationMs(latest)}`}
      className="overflow-visible"
    >
      {p95 != null && (
        <line
          x1={0}
          x2={width}
          y1={yForValue(p95)}
          y2={yForValue(p95)}
          className="stroke-amber-400/60 dark:stroke-amber-500/60"
          strokeWidth={1}
          strokeDasharray="3 3"
        >
          <title>p95: {formatDurationMs(p95)}</title>
        </line>
      )}
      {p50 != null && (
        <line
          x1={0}
          x2={width}
          y1={yForValue(p50)}
          y2={yForValue(p50)}
          className="stroke-emerald-500/60 dark:stroke-emerald-400/60"
          strokeWidth={1}
          strokeDasharray="3 3"
        >
          <title>p50: {formatDurationMs(p50)}</title>
        </line>
      )}
      <polyline
        points={points}
        fill="none"
        className="stroke-primary"
        strokeWidth={1.5}
        strokeLinejoin="round"
        strokeLinecap="round"
      />
      {ordered.map((v, i) => {
        return (
          <circle
            key={i}
            cx={i * xStep}
            cy={yForValue(v)}
            r={1.5}
            className="fill-primary"
          >
            <title>{formatDurationMs(v)}</title>
          </circle>
        );
      })}
    </svg>
  );
}
