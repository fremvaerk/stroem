import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import { WorkerStatusBadge } from "./worker-status-badge";

describe("WorkerStatusBadge", () => {
  it("renders the status text", () => {
    render(<WorkerStatusBadge status="active" />);
    expect(screen.getByText("active")).toBeInTheDocument();
  });

  it("renders 'inactive' status text", () => {
    render(<WorkerStatusBadge status="inactive" />);
    expect(screen.getByText("inactive")).toBeInTheDocument();
  });

  it("renders 'stale' status text", () => {
    render(<WorkerStatusBadge status="stale" />);
    expect(screen.getByText("stale")).toBeInTheDocument();
  });

  it("applies the green colour class for 'active' status", () => {
    const { container } = render(<WorkerStatusBadge status="active" />);
    const badge = container.firstChild as HTMLElement;
    expect(badge.className).toContain("bg-green-100");
    expect(badge.className).toContain("text-green-800");
  });

  it("applies the gray colour class for 'inactive' status", () => {
    const { container } = render(<WorkerStatusBadge status="inactive" />);
    const badge = container.firstChild as HTMLElement;
    expect(badge.className).toContain("bg-gray-100");
    expect(badge.className).toContain("text-gray-800");
  });

  it("applies the gray colour class for 'stale' status", () => {
    const { container } = render(<WorkerStatusBadge status="stale" />);
    const badge = container.firstChild as HTMLElement;
    expect(badge.className).toContain("bg-gray-100");
    expect(badge.className).toContain("text-gray-800");
  });

  it("applies a fallback gray class for an unknown status", () => {
    const { container } = render(<WorkerStatusBadge status="unknown" />);
    const badge = container.firstChild as HTMLElement;
    expect(badge.className).toContain("bg-gray-100");
  });

  it("renders a pulsing dot indicator for 'active' status", () => {
    const { container } = render(<WorkerStatusBadge status="active" />);
    const dot = container.querySelector(".animate-pulse");
    expect(dot).toBeInTheDocument();
  });

  it("does not render a pulsing dot for 'inactive' status", () => {
    const { container } = render(<WorkerStatusBadge status="inactive" />);
    const dot = container.querySelector(".animate-pulse");
    expect(dot).not.toBeInTheDocument();
  });

  it("does not render a pulsing dot for 'stale' status", () => {
    const { container } = render(<WorkerStatusBadge status="stale" />);
    const dot = container.querySelector(".animate-pulse");
    expect(dot).not.toBeInTheDocument();
  });
});
