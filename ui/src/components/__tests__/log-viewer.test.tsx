import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { LogViewer } from "../log-viewer";
import { LOG_GAP_MARKER } from "@/lib/log-lines";
import { installVirtualizerLayout } from "@/test/virtualizer-layout";

installVirtualizerLayout();

const TS = "2026-09-22T06:01:04.123Z";
/** TS in Europe/Oslo (CEST, UTC+2), the zone the timestamp tests pin. */
const TS_OSLO = "08:01:04";
const STARTS = [Date.parse("2026-09-22T06:01:00Z")];

const jsonl = (i: number, stream = "stdout") =>
  JSON.stringify({ ts: TS, stream, step: "s", line: `line ${i}` });

beforeEach(() => localStorage.clear());

describe("LogViewer", () => {
  it("shows a placeholder for an empty log", () => {
    render(<LogViewer logs="" isStreaming={false} />);
    expect(screen.getByText("Waiting for logs...")).toBeInTheDocument();
  });

  it("renders only a window of a long log", () => {
    const lines = Array.from({ length: 10_000 }, (_, i) => jsonl(i));
    const { container } = render(<LogViewer logs={lines} isStreaming={false} />);
    const rows = container.querySelectorAll("[data-index]");
    expect(rows.length).toBeGreaterThan(0);
    expect(rows.length).toBeLessThan(200);
  });

  it("parses JSONL rows from a raw body, with stderr styling", () => {
    render(<LogViewer logs={`${jsonl(1)}\n${jsonl(2, "stderr")}\n`} isStreaming={false} />);
    expect(screen.getByText("line 1")).toBeInTheDocument();
    expect(screen.getByText("line 2")).toHaveAttribute("data-stream", "stderr");
  });

  it("renders legacy plain-text lines as they are", () => {
    render(<LogViewer logs={"plain legacy line\n"} isStreaming={false} />);
    expect(screen.getByText("plain legacy line")).toBeInTheDocument();
  });

  it("renders the gap marker as a separator", () => {
    render(<LogViewer logs={[jsonl(1), LOG_GAP_MARKER, jsonl(2)]} isStreaming={false} />);
    expect(screen.getByTestId("log-gap")).toBeInTheDocument();
  });

  it("renders a header and the live badge", () => {
    render(<LogViewer logs="" isStreaming header={<div>banner here</div>} />);
    expect(screen.getByText("banner here")).toBeInTheDocument();
    expect(screen.getByText("Log streaming is active")).toBeInTheDocument();
  });

  it("mutes the live region while the user has scrolled away from the end", () => {
    const lines = Array.from({ length: 50 }, (_, i) => jsonl(i));
    render(<LogViewer logs={lines} isStreaming={false} />);
    const el = screen.getByRole("log");
    expect(el).toHaveAttribute("aria-live", "polite");

    Object.defineProperty(el, "scrollHeight", { configurable: true, value: 2000 });
    Object.defineProperty(el, "clientHeight", { configurable: true, value: 500 });
    Object.defineProperty(el, "scrollTop", { configurable: true, value: 0 });
    fireEvent.scroll(el);
    expect(el).toHaveAttribute("aria-live", "off");

    Object.defineProperty(el, "scrollTop", { configurable: true, value: 1500 });
    fireEvent.scroll(el);
    expect(el).toHaveAttribute("aria-live", "polite");
  });

  describe("timestamps", () => {
    // A non-UTC zone, so a UTC rendering fails here too (CI runs in UTC).
    beforeEach(() => vi.stubEnv("TZ", "Europe/Oslo"));
    afterEach(() => vi.unstubAllEnvs());

    it("shows local clock time without milliseconds", () => {
      render(<LogViewer logs={[jsonl(1)]} isStreaming={false} />);
      expect(screen.getByText(TS_OSLO)).toBeInTheDocument();
      expect(screen.queryByText(/\.123$/)).toBeNull();
    });

    it("offers no elapsed mode without attempt starts", () => {
      render(<LogViewer logs={[jsonl(1)]} isStreaming={false} />);
      expect(screen.queryByRole("button", { name: "Elapsed" })).toBeNull();
    });

    it("ignores a saved elapsed mode when there is nothing to count from", () => {
      localStorage.setItem("stroem_log_time_mode", "elapsed");
      render(<LogViewer logs={[jsonl(1)]} isStreaming={false} />);
      expect(screen.getByText(TS_OSLO)).toBeInTheDocument();
    });

    it("switches to time since the attempt started", () => {
      render(<LogViewer logs={[jsonl(1)]} isStreaming={false} attemptStarts={STARTS} />);
      expect(screen.getByRole("button", { name: "Clock" })).toHaveAttribute("aria-pressed", "true");

      fireEvent.click(screen.getByRole("button", { name: "Elapsed" }));

      expect(screen.getByText("+00:04")).toBeInTheDocument();
      expect(screen.queryByText(TS_OSLO)).toBeNull();
      expect(screen.getByRole("button", { name: "Elapsed" })).toHaveAttribute("aria-pressed", "true");
    });

    it("remembers the chosen mode across mounts", () => {
      const { unmount } = render(
        <LogViewer logs={[jsonl(1)]} isStreaming={false} attemptStarts={STARTS} />,
      );
      fireEvent.click(screen.getByRole("button", { name: "Elapsed" }));
      unmount();

      render(<LogViewer logs={[jsonl(1)]} isStreaming={false} attemptStarts={STARTS} />);
      expect(screen.getByText("+00:04")).toBeInTheDocument();
    });

    it("shows the other format on hover", () => {
      render(<LogViewer logs={[jsonl(1)]} isStreaming={false} attemptStarts={STARTS} />);
      expect(screen.getByText(TS_OSLO)).toHaveAttribute("title", "+00:04");

      fireEvent.click(screen.getByRole("button", { name: "Elapsed" }));
      expect(screen.getByText("+00:04")).toHaveAttribute("title", TS_OSLO);
    });
  });
});
