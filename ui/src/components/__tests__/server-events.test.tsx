import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { ServerEvents } from "../server-events";
import { useVirtualizerLayout } from "@/test/virtualizer-layout";

// Not a React hook — a `beforeAll`/`afterAll` registrar named for readability.
// eslint-disable-next-line react-hooks/rules-of-hooks
useVirtualizerLayout();

const getStepLogs = vi.fn();
vi.mock("@/lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@/lib/api")>()),
  getStepLogs: (...a: unknown[]) => getStepLogs(...a),
}));

const serverLine = JSON.stringify({ ts: "2026-09-22T06:00:00Z", stream: "stderr", step: "_server", line: "hook failed" });

beforeEach(() => getStepLogs.mockReset());

describe("ServerEvents", () => {
  it("renders nothing without events", async () => {
    getStepLogs.mockResolvedValue({ logs: "", truncated: false, total_bytes: 0, returned_bytes: 0 });
    const { container } = render(<ServerEvents jobId="j" jobStatus="completed" />);
    await waitFor(() => expect(getStepLogs).toHaveBeenCalled());
    expect(container).toBeEmptyDOMElement();
  });

  it("renders the events", async () => {
    getStepLogs.mockResolvedValue({ logs: `${serverLine}\n`, truncated: false, total_bytes: 90, returned_bytes: 90 });
    render(<ServerEvents jobId="j" jobStatus="completed" />);
    expect(await screen.findByText("hook failed")).toBeInTheDocument();
    expect(screen.queryByTestId("log-tail-banner")).not.toBeInTheDocument();
  });

  it("shows the banner for an empty but truncated tail", async () => {
    getStepLogs.mockResolvedValue({ logs: "", truncated: true, total_bytes: 40, returned_bytes: 0 });
    render(<ServerEvents jobId="j" jobStatus="completed" />);
    expect(await screen.findByTestId("log-tail-banner")).toBeInTheDocument();
    expect(screen.getByText("Server Events")).toBeInTheDocument();
  });
});
