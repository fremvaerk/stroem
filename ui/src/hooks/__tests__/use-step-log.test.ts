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

  it("stitches a tail that arrives from a poll while a full load is in flight", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    getStepLogs
      .mockResolvedValueOnce(tail("c\nd\n", { truncated: true }))
      .mockResolvedValue(tail("d\ne\n", { truncated: true }));
    let resolveFull!: (text: string) => void;
    const full = new Promise<string>((resolve) => {
      resolveFull = resolve;
    });
    getStepLogsFull.mockReturnValueOnce(full);
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: 2000 }));
    await waitFor(() => expect(result.current.truncated).toBe(true));

    act(() => {
      result.current.loadFull();
    });
    expect(result.current.fullState).toBe("loading");

    // A poll resolves ("d\ne\n") while the full-log request is still pending.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2100);
    });

    await act(async () => {
      resolveFull("a\nb\nc\nd\n");
      await full;
    });
    await waitFor(() => expect(result.current.fullState).toBe("loaded"));
    // The poll's newer line ("e") is stitched onto the snapshot, not lost.
    expect(result.current.lines).toEqual(["a", "b", "c", "d", "e"]);
  });

  it("stitches a tail from the pollMs -> null final fetch that arrives during a pending full load", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    getStepLogs.mockResolvedValue(tail("c\nd\n", { truncated: true }));
    let resolveFull!: (text: string) => void;
    const full = new Promise<string>((resolve) => {
      resolveFull = resolve;
    });
    getStepLogsFull.mockReturnValueOnce(full);
    const { result, rerender } = renderHook(
      ({ pollMs }: { pollMs: number | null }) => useStepLog("j", "build", { enabled: true, pollMs }),
      { initialProps: { pollMs: 2000 as number | null } },
    );
    await waitFor(() => expect(result.current.truncated).toBe(true));

    act(() => {
      result.current.loadFull();
    });
    expect(result.current.fullState).toBe("loading");

    // The step goes terminal mid-load: the caller switches pollMs to null,
    // which fires exactly one more fetch with newer content.
    const callsBeforeTransition = getStepLogs.mock.calls.length;
    getStepLogs.mockResolvedValue(tail("d\ne\n", { truncated: true }));
    rerender({ pollMs: null });
    await waitFor(() => expect(getStepLogs.mock.calls.length).toBe(callsBeforeTransition + 1));

    await act(async () => {
      resolveFull("a\nb\nc\nd\n");
      await full;
    });
    await waitFor(() => expect(result.current.fullState).toBe("loaded"));
    expect(result.current.lines).toEqual(["a", "b", "c", "d", "e"]);
  });

  it("a tail that arrives during the load and is already covered by the snapshot's end is a no-op", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    getStepLogs
      .mockResolvedValueOnce(tail("a\nb\n", { truncated: true }))
      // Arrives mid-load, but its content ("c","d") sits exactly at the
      // end of what the full load is about to return, so nothing new.
      .mockResolvedValue(tail("c\nd\n", { truncated: true }));
    let resolveFull!: (text: string) => void;
    const full = new Promise<string>((resolve) => {
      resolveFull = resolve;
    });
    getStepLogsFull.mockReturnValueOnce(full);
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: 2000 }));
    await waitFor(() => expect(result.current.truncated).toBe(true));

    act(() => {
      result.current.loadFull();
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2100);
    });

    await act(async () => {
      resolveFull("a\nb\nc\nd\n");
      await full;
    });
    await waitFor(() => expect(result.current.fullState).toBe("loaded"));
    expect(result.current.lines).toEqual(["a", "b", "c", "d"]);
  });

  it("stitches every tail fetched during a load, in request order, across two polls", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    getStepLogs
      .mockResolvedValueOnce(tail("c\nd\n", { truncated: true }))
      .mockResolvedValueOnce(tail("d\ne\n", { truncated: true }))
      .mockResolvedValue(tail("f\ng\n", { truncated: true }));
    let resolveFull!: (text: string) => void;
    const full = new Promise<string>((resolve) => {
      resolveFull = resolve;
    });
    getStepLogsFull.mockReturnValueOnce(full);
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: 2000 }));
    await waitFor(() => expect(result.current.truncated).toBe(true));

    act(() => {
      result.current.loadFull();
    });
    expect(result.current.fullState).toBe("loading");

    // Two polls land, back to back, while the full-log request is pending.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(4100);
    });
    expect(getStepLogs.mock.calls.length).toBeGreaterThanOrEqual(3);

    await act(async () => {
      resolveFull("a\nb\nc\nd\n");
      await full;
    });
    await waitFor(() => expect(result.current.fullState).toBe("loaded"));
    // Both polls are kept (not just the last one) and in request order; they
    // share no line with each other, so a gap marker separates them.
    expect(result.current.lines).toEqual(["a", "b", "c", "d", "e", LOG_GAP_MARKER, "f", "g"]);
  });

  it("stitches every tail fetched during a RELOAD the same way, without touching the stale full view mid-load", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    getStepLogs.mockResolvedValue(tail("c\nd\n", { truncated: true }));
    getStepLogsFull.mockResolvedValueOnce("a\nb\nc\nd\n");
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: 2000 }));
    await waitFor(() => expect(result.current.truncated).toBe(true));

    // First load completes normally.
    await act(async () => {
      result.current.loadFull();
    });
    await waitFor(() => expect(result.current.fullState).toBe("loaded"));
    expect(result.current.lines).toEqual(["a", "b", "c", "d"]);

    // Reload: a new deferred full-log request, with two polls landing while
    // it's pending — same shape as the first-load case above.
    getStepLogs
      .mockResolvedValueOnce(tail("d\ne\n", { truncated: true }))
      .mockResolvedValue(tail("f\ng\n", { truncated: true }));
    let resolveFull!: (text: string) => void;
    const full = new Promise<string>((resolve) => {
      resolveFull = resolve;
    });
    getStepLogsFull.mockReturnValueOnce(full);

    act(() => {
      result.current.loadFull();
    });
    expect(result.current.fullState).toBe("loading");
    // The stale full view from the first load stays on screen untouched
    // while the reload is in flight — the polls below must not append to it.
    expect(result.current.lines).toEqual(["a", "b", "c", "d"]);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(4100);
    });

    await act(async () => {
      resolveFull("a\nb\nc\nd\n");
      await full;
    });
    await waitFor(() => expect(result.current.fullState).toBe("loaded"));
    expect(result.current.lines).toEqual(["a", "b", "c", "d", "e", LOG_GAP_MARKER, "f", "g"]);
  });

  it("does not reorder the snapshot when a poll that started before the load resolves, mid-load, with content already inside it", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    let resolvePoll!: (value: ReturnType<typeof tail>) => void;
    const pending = new Promise<ReturnType<typeof tail>>((resolve) => {
      resolvePoll = resolve;
    });
    // The initial mount fetch stays pending — its request started BEFORE
    // `loadFull` is ever called.
    getStepLogs.mockReturnValueOnce(pending);
    let resolveFull!: (text: string) => void;
    const full = new Promise<string>((resolve) => {
      resolveFull = resolve;
    });
    getStepLogsFull.mockReturnValueOnce(full);
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: 2000 }));

    act(() => {
      result.current.loadFull();
    });
    expect(result.current.fullState).toBe("loading");

    // The pre-load poll resolves DURING the load, with lines that sit in
    // the MIDDLE of the eventual snapshot, not at its end.
    await act(async () => {
      resolvePoll(tail("b\nc\n"));
      await pending;
    });

    await act(async () => {
      resolveFull("a\nb\nc\nd\n");
      await full;
    });
    await waitFor(() => expect(result.current.fullState).toBe("loaded"));
    expect(result.current.lines).toEqual(["a", "b", "c", "d"]);
  });

  it("adopts truncated and the larger bound from an empty poll once a body was seen", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    getStepLogs
      .mockResolvedValueOnce(tail("a\n"))
      .mockResolvedValue(tail("", { truncated: true, total_bytes: 999 }));
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: 2000 }));
    await waitFor(() => expect(result.current.lines).toEqual(["a"]));

    await act(async () => {
      await vi.advanceTimersByTimeAsync(2100);
    });

    expect(result.current.lines).toEqual(["a"]);
    expect(result.current.truncated).toBe(true);
    expect(result.current.totalBytes).toBe(999);
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

  it("reloads the full log from the loaded state, replacing the view", async () => {
    getStepLogs.mockResolvedValue(tail("c\nd\n", { truncated: true }));
    getStepLogsFull
      .mockResolvedValueOnce("a\nb\nc\nd\n")
      .mockResolvedValueOnce("x\ny\nz\n");
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: null }));
    await waitFor(() => expect(result.current.truncated).toBe(true));

    await act(async () => {
      result.current.loadFull();
    });
    await waitFor(() => expect(result.current.fullState).toBe("loaded"));
    expect(result.current.lines).toEqual(["a", "b", "c", "d"]);

    await act(async () => {
      result.current.loadFull();
    });
    await waitFor(() => expect(result.current.fullState).toBe("loaded"));
    expect(getStepLogsFull).toHaveBeenCalledTimes(2);
    expect(result.current.lines).toEqual(["x", "y", "z"]);
  });

  it("keeps the previously loaded full view on screen after a reload fails", async () => {
    getStepLogs.mockResolvedValue(tail("c\nd\n", { truncated: true }));
    getStepLogsFull.mockResolvedValueOnce("a\nb\nc\nd\n").mockRejectedValueOnce(new Error("boom"));
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: null }));
    await waitFor(() => expect(result.current.truncated).toBe(true));

    await act(async () => {
      result.current.loadFull();
    });
    await waitFor(() => expect(result.current.fullState).toBe("loaded"));
    expect(result.current.lines).toEqual(["a", "b", "c", "d"]);

    await act(async () => {
      result.current.loadFull();
    });
    await waitFor(() => expect(result.current.fullState).toBe("error"));
    expect(result.current.lines).toEqual(["a", "b", "c", "d"]);
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
