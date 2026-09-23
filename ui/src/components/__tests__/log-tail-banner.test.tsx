import { describe, it, expect, vi } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { LogTailBanner } from "../log-tail-banner";

const base = {
  lineCount: 2100,
  returnedBytes: 262_144,
  totalBytes: 87_325_871,
  fullState: "idle" as const,
  progressBytes: 0,
  onLoadFull: vi.fn(),
  onDownload: vi.fn(),
};

describe("LogTailBanner", () => {
  it("says how much of the log is shown", () => {
    render(<LogTailBanner {...base} />);
    expect(screen.getByTestId("log-tail-banner")).toHaveTextContent(
      "Showing the last ~2,100 lines (256.0 KiB of up to 83.3 MiB).",
    );
  });

  it("offers load and download", () => {
    const onLoadFull = vi.fn();
    const onDownload = vi.fn();
    render(<LogTailBanner {...base} onLoadFull={onLoadFull} onDownload={onDownload} />);
    fireEvent.click(screen.getByRole("button", { name: "Load full log" }));
    fireEvent.click(screen.getByRole("button", { name: /Download/ }));
    expect(onLoadFull).toHaveBeenCalledOnce();
    expect(onDownload).toHaveBeenCalledOnce();
  });

  it("shows progress while loading and an error after a failure", () => {
    const { rerender } = render(<LogTailBanner {...base} fullState="loading" progressBytes={3 * 1024 * 1024} />);
    expect(screen.getByRole("button", { name: "Loading… 3.0 MiB" })).toBeDisabled();
    rerender(<LogTailBanner {...base} fullState="error" />);
    expect(screen.getByText("Could not load the full log.")).toBeInTheDocument();
  });

  it("shows a download error only when the prop is true", () => {
    const { rerender } = render(<LogTailBanner {...base} />);
    expect(screen.queryByText("Could not download the log.")).not.toBeInTheDocument();
    rerender(<LogTailBanner {...base} downloadError />);
    expect(screen.getByText("Could not download the log.")).toBeInTheDocument();
  });

  it("keeps the banner reachable once the full log is loaded", () => {
    const onLoadFull = vi.fn();
    render(<LogTailBanner {...base} fullState="loaded" onLoadFull={onLoadFull} />);
    expect(screen.getByTestId("log-tail-banner")).toHaveTextContent("Showing the full log (~2,100 lines).");
    fireEvent.click(screen.getByRole("button", { name: "Reload full log" }));
    expect(onLoadFull).toHaveBeenCalledOnce();
    // Download stays reachable too.
    expect(screen.getByRole("button", { name: /Download/ })).toBeInTheDocument();
  });
});
