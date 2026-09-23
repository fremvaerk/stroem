import { describe, it, expect } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { LogViewer } from "../log-viewer";
import { LOG_GAP_MARKER } from "@/lib/log-lines";
import { useVirtualizerLayout } from "@/test/virtualizer-layout";

// Not a React hook — a `beforeAll`/`afterAll` registrar named for readability.
// eslint-disable-next-line react-hooks/rules-of-hooks
useVirtualizerLayout();

const jsonl = (i: number, stream = "stdout") =>
  JSON.stringify({ ts: "2026-09-22T06:01:04.123Z", stream, step: "s", line: `line ${i}` });

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
});
