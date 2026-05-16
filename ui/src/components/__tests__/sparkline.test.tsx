import { describe, it, expect } from "vitest";
import { render } from "@testing-library/react";
import { Sparkline } from "../sparkline";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function renderSparkline(props: Parameters<typeof Sparkline>[0]) {
  return render(<Sparkline {...props} />);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("Sparkline", () => {
  it("renders nothing (null) for an empty values array", () => {
    const { container } = renderSparkline({ values: [] });
    expect(container.firstChild).toBeNull();
  });

  it("renders a single circle and degenerate polyline for a single value", () => {
    const { container } = renderSparkline({ values: [1000] });
    const svg = container.querySelector("svg");
    expect(svg).toBeInTheDocument();
    const circles = container.querySelectorAll("circle");
    expect(circles).toHaveLength(1);
    // Degenerate case: xStep is 0, so cx should be 0
    expect(circles[0].getAttribute("cx")).toBe("0");
    // Polyline is still rendered (with a single point)
    const polyline = container.querySelector("polyline");
    expect(polyline).toBeInTheDocument();
  });

  it("renders all circles at the top when all values are identical (t=1.0)", () => {
    // min is anchored at 0, so t = (v - 0) / (v - 0) = 1.0 for all values.
    // y = height - 1.0 * (height - 4) - 2 = 2 (2px top padding).
    const height = 48;
    const { container } = renderSparkline({
      values: [500, 500, 500],
      height,
    });
    const circles = container.querySelectorAll("circle");
    expect(circles.length).toBeGreaterThan(0);
    for (const circle of circles) {
      const cy = parseFloat(circle.getAttribute("cy") ?? "NaN");
      // Should be 2px top padding (within the SVG viewport)
      expect(cy).toBe(2);
      expect(cy).toBeGreaterThanOrEqual(0);
      expect(cy).toBeLessThanOrEqual(height);
    }
  });

  it("p95 reference line appears inside the SVG viewport when p95 > all data points", () => {
    const height = 48;
    const width = 240;
    // Data well below p95 so p95 pushes the y-range up.
    const { container } = renderSparkline({
      values: [100, 200, 150],
      p95: 10_000,
      height,
      width,
    });
    // p95 line should exist
    const lines = container.querySelectorAll("line");
    expect(lines.length).toBeGreaterThan(0);
    // The p95 line is the first one rendered (before p50)
    const p95Line = lines[0];
    const y = parseFloat(p95Line.getAttribute("y1") ?? "NaN");
    // Must be within the SVG viewport
    expect(y).toBeGreaterThanOrEqual(0);
    expect(y).toBeLessThanOrEqual(height);
  });

  it("aria-label includes sample count and latest value formatted", () => {
    const { container } = renderSparkline({
      values: [1000, 2000, 3000],
      ariaLabel: "Duration history for my-task",
    });
    const svg = container.querySelector("svg");
    const label = svg?.getAttribute("aria-label") ?? "";
    // Should include count (3 runs)
    expect(label).toMatch(/3 runs/);
    // Should include formatted latest value — values are newest-first so latest in display is 1000ms
    expect(label).toMatch(/1000ms|1\.0s/);
    // Should include the provided ariaLabel prefix
    expect(label).toContain("Duration history for my-task");
  });
});
