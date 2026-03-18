import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import { StatusBadge } from "../status-badge";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Render the badge and return the root element. */
function renderBadge(status: string) {
  const { container } = render(<StatusBadge status={status} />);
  return container.firstElementChild as HTMLElement;
}

// ---------------------------------------------------------------------------
// Text content
// ---------------------------------------------------------------------------
describe("StatusBadge", () => {
  const statuses = [
    "pending",
    "ready",
    "running",
    "completed",
    "failed",
    "cancelled",
    "skipped",
  ] as const;

  statuses.forEach((status) => {
    it(`renders the status text "${status}"`, () => {
      renderBadge(status);
      // The badge renders the raw status string as visible text.
      expect(screen.getByText(status, { exact: false })).toBeInTheDocument();
    });
  });

  it("renders unknown status text as-is", () => {
    renderBadge("unknown-status");
    expect(screen.getByText("unknown-status", { exact: false })).toBeInTheDocument();
  });

  // ---------------------------------------------------------------------------
  // Styling — we check that the rendered element has the right CSS classes,
  // which drive the colour scheme in the UI.
  // ---------------------------------------------------------------------------
  it("applies the 'capitalize' class so status text is capitalised visually", () => {
    const el = renderBadge("pending");
    // The outermost Badge element should carry 'capitalize' among its classes.
    expect(el.className).toMatch(/capitalize/);
  });

  it("applies the 'font-mono' class for monospace rendering", () => {
    const el = renderBadge("running");
    expect(el.className).toMatch(/font-mono/);
  });

  it("applies yellow styling for 'pending'", () => {
    const el = renderBadge("pending");
    expect(el.className).toMatch(/yellow/);
  });

  it("applies blue styling for 'running'", () => {
    const el = renderBadge("running");
    expect(el.className).toMatch(/blue/);
  });

  it("applies blue styling for 'ready'", () => {
    const el = renderBadge("ready");
    expect(el.className).toMatch(/blue/);
  });

  it("applies green styling for 'completed'", () => {
    const el = renderBadge("completed");
    expect(el.className).toMatch(/green/);
  });

  it("applies red styling for 'failed'", () => {
    const el = renderBadge("failed");
    expect(el.className).toMatch(/red/);
  });

  it("applies gray styling for 'cancelled'", () => {
    const el = renderBadge("cancelled");
    expect(el.className).toMatch(/gray/);
  });

  it("applies slate styling for 'skipped'", () => {
    const el = renderBadge("skipped");
    expect(el.className).toMatch(/slate/);
  });

  // ---------------------------------------------------------------------------
  // Running indicator
  // ---------------------------------------------------------------------------
  it("renders an animated pulse indicator only for 'running'", () => {
    const { container } = render(<StatusBadge status="running" />);
    const pulseEl = container.querySelector(".animate-pulse");
    expect(pulseEl).toBeInTheDocument();
  });

  it("does NOT render an animated pulse for 'completed'", () => {
    const { container } = render(<StatusBadge status="completed" />);
    const pulseEl = container.querySelector(".animate-pulse");
    expect(pulseEl).not.toBeInTheDocument();
  });

  it("does NOT render an animated pulse for 'failed'", () => {
    const { container } = render(<StatusBadge status="failed" />);
    const pulseEl = container.querySelector(".animate-pulse");
    expect(pulseEl).not.toBeInTheDocument();
  });

  it("does NOT render an animated pulse for 'pending'", () => {
    const { container } = render(<StatusBadge status="pending" />);
    const pulseEl = container.querySelector(".animate-pulse");
    expect(pulseEl).not.toBeInTheDocument();
  });
});
