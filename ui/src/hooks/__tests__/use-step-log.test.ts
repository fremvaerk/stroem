import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { renderHook, act, waitFor } from "@testing-library/react";
import { useStepLog } from "../use-step-log";
import { LOG_GAP_MARKER } from "@/lib/log-lines";

const getStepLogs = vi.fn();
const getStepLogsFull = vi.fn();
const downloadStepLog = vi.fn();
vi.mock("@/lib/api", () => ({
  getStepLogs: (...a: unknown[]) => getStepLogs(...a),
  getStepLogsFull: (...a: unknown[]) => getStepLogsFull(...a),
  downloadStepLog: (...a: unknown[]) => downloadStepLog(...a),
}));

const tail = (logs: string, over: Partial<{ truncated: boolean; total_bytes: number }> = {}) => ({
  logs,
  truncated: false,
  total_bytes: logs.length,
  returned_bytes: logs.length,
  ...over,
});

beforeEach(() => {
  getStepLogs.mockReset();
  getStepLogsFull.mockReset();
  downloadStepLog.mockReset().mockResolvedValue(undefined);
});
afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe("useStepLog", () => {
  it("fetches once without polling and exposes the tail metadata", async () => {
    getStepLogs.mockResolvedValue(tail("a\nb\n", { truncated: true, total_bytes: 999 }));
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: null }));
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.lines).toEqual(["a", "b"]);
    expect(result.current.truncated).toBe(true);
    expect(result.current.totalBytes).toBe(999);
    expect(getStepLogs).toHaveBeenCalledTimes(1);
  });

  it("does nothing when disabled", async () => {
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: false, pollMs: 2000 }));
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(getStepLogs).not.toHaveBeenCalled();
  });

  it("keeps the last non-empty body when a poll comes back empty", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    getStepLogs.mockResolvedValueOnce(tail("a\n")).mockResolvedValue(tail(""));
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: 2000 }));
    await waitFor(() => expect(result.current.lines).toEqual(["a"]));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2100);
    });
    expect(getStepLogs.mock.calls.length).toBeGreaterThanOrEqual(2);
    expect(result.current.lines).toEqual(["a"]);
  });

  it("loads the full log, then stitches later polls onto it", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    getStepLogs
      .mockResolvedValueOnce(tail("c\nd\n", { truncated: true }))
      .mockResolvedValue(tail("d\ne\n", { truncated: true }));
    getStepLogsFull.mockResolvedValue("a\nb\nc\nd\n");
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: 2000 }));
    await waitFor(() => expect(result.current.truncated).toBe(true));
    await act(async () => {
      result.current.loadFull();
    });
    await waitFor(() => expect(result.current.fullState).toBe("loaded"));
    expect(result.current.lines).toEqual(["a", "b", "c", "d"]);
    expect(result.current.truncated).toBe(false);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2100);
    });
    await waitFor(() => expect(result.current.lines).toEqual(["a", "b", "c", "d", "e"]));
  });

  it("marks a gap when a poll shares no line with the full view", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    getStepLogs.mockResolvedValueOnce(tail("b\n", { truncated: true })).mockResolvedValue(tail("y\nz\n", { truncated: true }));
    getStepLogsFull.mockResolvedValue("a\nb\n");
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: 2000 }));
    await waitFor(() => expect(result.current.truncated).toBe(true));
    await act(async () => {
      result.current.loadFull();
    });
    await waitFor(() => expect(result.current.fullState).toBe("loaded"));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2100);
    });
    await waitFor(() => expect(result.current.lines).toEqual(["a", "b", LOG_GAP_MARKER, "y", "z"]));
  });

  it("asks before loading more than 64 MiB", async () => {
    getStepLogs.mockResolvedValue(tail("a\n", { truncated: true, total_bytes: 65 * 1024 * 1024 }));
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: null }));
    await waitFor(() => expect(result.current.truncated).toBe(true));
    await act(async () => {
      result.current.loadFull();
    });
    expect(confirm).toHaveBeenCalled();
    expect(getStepLogsFull).not.toHaveBeenCalled();
  });

  it("reports a failed full load", async () => {
    getStepLogs.mockResolvedValue(tail("a\n", { truncated: true }));
    getStepLogsFull.mockRejectedValue(new Error("boom"));
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: null }));
    await waitFor(() => expect(result.current.truncated).toBe(true));
    await act(async () => {
      result.current.loadFull();
    });
    await waitFor(() => expect(result.current.fullState).toBe("error"));
    expect(result.current.lines).toEqual(["a"]);
  });

  it("starts over when the step changes", async () => {
    getStepLogs.mockImplementation((_job: string, step: string) => Promise.resolve(tail(`${step}-line\n`)));
    const { result, rerender } = renderHook(
      ({ step }) => useStepLog("j", step, { enabled: true, pollMs: null }),
      { initialProps: { step: "a" } },
    );
    await waitFor(() => expect(result.current.lines).toEqual(["a-line"]));
    rerender({ step: "b" });
    await waitFor(() => expect(result.current.lines).toEqual(["b-line"]));
  });

  it("never runs two tail fetches concurrently", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    let resolveFirst!: (value: ReturnType<typeof tail>) => void;
    const first = new Promise<ReturnType<typeof tail>>((resolve) => {
      resolveFirst = resolve;
    });
    // The first call stays pending; later calls (if any got through) resolve normally.
    getStepLogs.mockReturnValueOnce(first).mockResolvedValue(tail("b\n"));
    renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: 2000 }));

    // Two intervals elapse while the first fetch is still in flight: a
    // second call here would mean the poll overlapped itself.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(4100);
    });
    expect(getStepLogs).toHaveBeenCalledTimes(1);

    // Resolving the first fetch clears `inFlight`, letting the poll resume.
    await act(async () => {
      resolveFirst(tail("a\n"));
      await vi.advanceTimersByTimeAsync(0);
    });

    await act(async () => {
      await vi.advanceTimersByTimeAsync(2100);
    });
    expect(getStepLogs).toHaveBeenCalledTimes(2);
  });

  it("keeps the loaded full view across a pollMs change, with exactly one final fetch", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    getStepLogs.mockResolvedValue(tail("c\nd\n", { truncated: true }));
    getStepLogsFull.mockResolvedValue("a\nb\nc\nd\n");
    const { result, rerender } = renderHook(
      ({ pollMs }: { pollMs: number | null }) => useStepLog("j", "build", { enabled: true, pollMs }),
      { initialProps: { pollMs: 2000 as number | null } },
    );
    await waitFor(() => expect(result.current.truncated).toBe(true));
    await act(async () => {
      result.current.loadFull();
    });
    await waitFor(() => expect(result.current.fullState).toBe("loaded"));
    expect(result.current.lines).toEqual(["a", "b", "c", "d"]);

    // Step goes terminal: the caller switches pollMs to null.
    const callsBeforeTransition = getStepLogs.mock.calls.length;
    rerender({ pollMs: null });
    await waitFor(() => expect(getStepLogs.mock.calls.length).toBe(callsBeforeTransition + 1));
    await waitFor(() => expect(result.current.lines).toEqual(["a", "b", "c", "d"]));

    // No more polling after that final fetch.
    const callsAfterFinalFetch = getStepLogs.mock.calls.length;
    await act(async () => {
      await vi.advanceTimersByTimeAsync(10_000);
    });
    expect(getStepLogs.mock.calls.length).toBe(callsAfterFinalFetch);
  });

  it("surfaces a failed download and clears it on the next attempt", async () => {
    getStepLogs.mockResolvedValue(tail("a\n"));
    downloadStepLog.mockRejectedValueOnce(new Error("boom"));
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: null }));
    await waitFor(() => expect(result.current.loading).toBe(false));

    act(() => {
      result.current.download();
    });
    await waitFor(() => expect(result.current.downloadError).toBe(true));

    // The next attempt clears the error before its own result is known.
    act(() => {
      result.current.download();
    });
    expect(result.current.downloadError).toBe(false);
  });
});
