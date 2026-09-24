import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { renderHook, act, waitFor } from "@testing-library/react";
import { FULL_LOAD_STALL_MS, POLL_TIMEOUT_MS, useStepLog } from "../use-step-log";
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

  it("stitches every tail fetched during a RELOAD the same way, showing them on the current view as they land", async () => {
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
    expect(result.current.lines).toEqual(["a", "b", "c", "d"]);

    // The polls keep the view current while the reload is in flight (a
    // reload that then fails must not have hidden them — see below)...
    await act(async () => {
      await vi.advanceTimersByTimeAsync(4100);
    });
    expect(result.current.lines).toEqual(["a", "b", "c", "d", "e", LOG_GAP_MARKER, "f", "g"]);

    // ...and are replayed, in request order, onto the reload's snapshot.
    await act(async () => {
      resolveFull("a\nb\nc\nd\n");
      await full;
    });
    await waitFor(() => expect(result.current.fullState).toBe("loaded"));
    expect(result.current.lines).toEqual(["a", "b", "c", "d", "e", LOG_GAP_MARKER, "f", "g"]);
  });

  it("keeps the lines a poll added during a reload when the reload fails, and keeps stitching afterwards", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    getStepLogs.mockResolvedValue(tail("c\nd\n", { truncated: true }));
    getStepLogsFull.mockResolvedValueOnce("a\nb\nc\nd\n");
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: 2000 }));
    await waitFor(() => expect(result.current.truncated).toBe(true));
    await act(async () => {
      result.current.loadFull();
    });
    await waitFor(() => expect(result.current.fullState).toBe("loaded"));
    expect(result.current.lines).toEqual(["a", "b", "c", "d"]);

    // Reload, deferred; it will fail after a poll has landed.
    let rejectFull!: (err: Error) => void;
    const full = new Promise<string>((_, reject) => {
      rejectFull = reject;
    });
    getStepLogsFull.mockReturnValueOnce(full);
    getStepLogs.mockResolvedValue(tail("d\ne\n", { truncated: true }));
    act(() => {
      result.current.loadFull();
    });
    expect(result.current.fullState).toBe("loading");

    await act(async () => {
      await vi.advanceTimersByTimeAsync(2100);
    });
    expect(result.current.lines).toEqual(["a", "b", "c", "d", "e"]);

    await act(async () => {
      rejectFull(new Error("boom"));
      await full.catch(() => undefined);
    });
    await waitFor(() => expect(result.current.fullState).toBe("error"));
    expect(result.current.lines).toEqual(["a", "b", "c", "d", "e"]);

    // Polling goes on and the view keeps growing.
    getStepLogs.mockResolvedValue(tail("e\nf\n", { truncated: true }));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2100);
    });
    expect(result.current.lines).toEqual(["a", "b", "c", "d", "e", "f"]);
    expect(result.current.fullState).toBe("error");
  });

  it.each([
    ["resolves"],
    ["rejects"],
  ])("an old step's full load that %s after the step changed leaves the new step's load and its pending tails alone", async (mode) => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    getStepLogs.mockImplementation((_job: string, step: string) =>
      Promise.resolve(tail(step === "a" ? "a-1\n" : "c\nd\n", { truncated: true })),
    );
    let resolveOld!: (text: string) => void;
    let rejectOld!: (err: Error) => void;
    const oldFull = new Promise<string>((resolve, reject) => {
      resolveOld = resolve;
      rejectOld = reject;
    });
    let resolveNew!: (text: string) => void;
    const newFull = new Promise<string>((resolve) => {
      resolveNew = resolve;
    });
    getStepLogsFull.mockReturnValueOnce(oldFull).mockReturnValueOnce(newFull);
    const { result, rerender } = renderHook(
      ({ step }) => useStepLog("j", step, { enabled: true, pollMs: 2000 }),
      { initialProps: { step: "a" } },
    );
    await waitFor(() => expect(result.current.lines).toEqual(["a-1"]));

    // Step a: a full load that stays pending across the step change.
    act(() => {
      result.current.loadFull();
    });
    expect(result.current.fullState).toBe("loading");

    // Step b: starts over, then its own load, then a poll during that load.
    rerender({ step: "b" });
    await waitFor(() => expect(result.current.lines).toEqual(["c", "d"]));
    expect(result.current.fullState).toBe("idle");
    act(() => {
      result.current.loadFull();
    });
    expect(result.current.fullState).toBe("loading");
    getStepLogs.mockImplementation(() => Promise.resolve(tail("d\ne\n", { truncated: true })));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2100);
    });

    // The old step's load settles now: nothing about step b may change.
    await act(async () => {
      if (mode === "resolves") resolveOld("a-0\na-1\n");
      else rejectOld(new Error("boom"));
      await oldFull.catch(() => undefined);
    });
    expect(result.current.fullState).toBe("loading");
    expect(result.current.lines).toEqual(["d", "e"]);

    await act(async () => {
      resolveNew("a\nb\nc\nd\n");
      await newFull;
    });
    await waitFor(() => expect(result.current.fullState).toBe("loaded"));
    expect(result.current.lines).toEqual(["a", "b", "c", "d", "e"]);
  });

  it("ignores, for the view, a poll that started before the load and resolves after it completed", async () => {
    let resolvePoll!: (value: ReturnType<typeof tail>) => void;
    const pending = new Promise<ReturnType<typeof tail>>((resolve) => {
      resolvePoll = resolve;
    });
    // The mount fetch stays pending past the whole load.
    getStepLogs.mockReturnValueOnce(pending);
    getStepLogsFull.mockResolvedValueOnce("a\nb\nc\nd\n");
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: null }));

    await act(async () => {
      result.current.loadFull();
    });
    await waitFor(() => expect(result.current.fullState).toBe("loaded"));
    expect(result.current.lines).toEqual(["a", "b", "c", "d"]);

    await act(async () => {
      resolvePoll(tail("b\nc\n"));
      await pending;
    });
    expect(result.current.lines).toEqual(["a", "b", "c", "d"]);
  });

  it("starting a reload retires the previous load: its signal is aborted and its result is ignored", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    getStepLogs.mockResolvedValueOnce(tail("c\nd\n", { truncated: true })).mockResolvedValue(tail("e\nf\n", { truncated: true }));
    const signals: AbortSignal[] = [];
    let resolveL1!: (text: string) => void;
    const l1 = new Promise<string>((resolve) => {
      resolveL1 = resolve;
    });
    let resolveL2!: (text: string) => void;
    const l2 = new Promise<string>((resolve) => {
      resolveL2 = resolve;
    });
    getStepLogsFull
      .mockImplementationOnce((_j: string, _s: string, _p: unknown, signal: AbortSignal) => {
        signals.push(signal);
        return l1;
      })
      .mockImplementationOnce((_j: string, _s: string, _p: unknown, signal: AbortSignal) => {
        signals.push(signal);
        return l2;
      });
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: 2000 }));
    await waitFor(() => expect(result.current.truncated).toBe(true));

    act(() => {
      result.current.loadFull();
    });
    expect(signals[0].aborted).toBe(false);
    act(() => {
      result.current.loadFull();
    });
    expect(getStepLogsFull).toHaveBeenCalledTimes(2);
    expect(signals[0].aborted).toBe(true);
    expect(signals[1].aborted).toBe(false);
    expect(result.current.fullState).toBe("loading");

    // A poll newer than both loads lands while the second is in flight.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2100);
    });

    // The retired load's transport answers anyway: nothing changes.
    await act(async () => {
      resolveL1("x\ny\n");
      await l1;
    });
    expect(result.current.fullState).toBe("loading");
    expect(result.current.lines).toEqual(["e", "f"]);

    await act(async () => {
      resolveL2("a\nb\nc\nd\ne\n");
      await l2;
    });
    await waitFor(() => expect(result.current.fullState).toBe("loaded"));
    expect(result.current.lines).toEqual(["a", "b", "c", "d", "e", "f"]);
  });

  it("aborts a poll that never answers after 30 s and keeps polling", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    let signal!: AbortSignal;
    getStepLogs
      .mockImplementationOnce((_j: string, _s: string, s: AbortSignal) => {
        signal = s;
        return new Promise(() => {});
      })
      .mockResolvedValue(tail("a\n"));
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: 2000 }));
    expect(signal).toBeInstanceOf(AbortSignal);

    // Many intervals elapse while the first poll hangs: no overlap, no abort yet.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(POLL_TIMEOUT_MS - 1000);
    });
    expect(getStepLogs).toHaveBeenCalledTimes(1);
    expect(signal.aborted).toBe(false);
    expect(result.current.loading).toBe(true);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1100);
    });
    expect(signal.aborted).toBe(true);
    expect(result.current.loading).toBe(false);

    // Polling resumes on the next interval.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2100);
    });
    expect(getStepLogs.mock.calls.length).toBeGreaterThanOrEqual(2);
    await waitFor(() => expect(result.current.lines).toEqual(["a"]));
  });

  it("fails a full load that makes no progress for 60 s", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    getStepLogs.mockResolvedValue(tail("a\n", { truncated: true }));
    let signal!: AbortSignal;
    getStepLogsFull.mockImplementationOnce((_j: string, _s: string, _p: unknown, s: AbortSignal) => {
      signal = s;
      return new Promise(() => {});
    });
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: null }));
    await waitFor(() => expect(result.current.truncated).toBe(true));
    act(() => {
      result.current.loadFull();
    });
    expect(result.current.fullState).toBe("loading");

    await act(async () => {
      await vi.advanceTimersByTimeAsync(FULL_LOAD_STALL_MS - 1000);
    });
    expect(signal.aborted).toBe(false);
    expect(result.current.fullState).toBe("loading");

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1100);
    });
    expect(signal.aborted).toBe(true);
    await waitFor(() => expect(result.current.fullState).toBe("error"));
    expect(result.current.lines).toEqual(["a"]);
  });

  it("does not abort a load that keeps making progress for longer than the stall timeout", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    getStepLogs.mockResolvedValue(tail("a\n", { truncated: true }));
    let signal!: AbortSignal;
    let onProgress!: (n: number) => void;
    let resolveFull!: (text: string) => void;
    const full = new Promise<string>((resolve) => {
      resolveFull = resolve;
    });
    getStepLogsFull.mockImplementationOnce((_j: string, _s: string, p: (n: number) => void, s: AbortSignal) => {
      signal = s;
      onProgress = p;
      return full;
    });
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: null }));
    await waitFor(() => expect(result.current.truncated).toBe(true));
    act(() => {
      result.current.loadFull();
    });

    // A chunk every 10 s for 90 s: well past 60 s in total, never 60 s idle.
    for (let i = 1; i <= 9; i++) {
      await act(async () => {
        await vi.advanceTimersByTimeAsync(10_000);
      });
      act(() => {
        onProgress(i * 1000);
      });
    }
    expect(signal.aborted).toBe(false);
    expect(result.current.fullState).toBe("loading");
    expect(result.current.progressBytes).toBe(9000);

    await act(async () => {
      resolveFull("a\nb\n");
      await full;
    });
    await waitFor(() => expect(result.current.fullState).toBe("loaded"));
    expect(result.current.lines).toEqual(["a", "b"]);
  });

  it("aborts every in-flight request on unmount", async () => {
    let pollSignal!: AbortSignal;
    let loadSignal!: AbortSignal;
    getStepLogs.mockImplementation((_j: string, _s: string, s: AbortSignal) => {
      pollSignal = s;
      return new Promise(() => {});
    });
    getStepLogsFull.mockImplementation((_j: string, _s: string, _p: unknown, s: AbortSignal) => {
      loadSignal = s;
      return new Promise(() => {});
    });
    const { result, unmount } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: 2000 }));
    act(() => {
      result.current.loadFull();
    });
    expect(pollSignal.aborted).toBe(false);
    expect(loadSignal.aborted).toBe(false);
    unmount();
    expect(pollSignal.aborted).toBe(true);
    expect(loadSignal.aborted).toBe(true);
  });

  it("aborts the old step's requests when the step changes", async () => {
    const signals = new Map<string, AbortSignal>();
    getStepLogs.mockImplementation((_j: string, step: string, s: AbortSignal) => {
      signals.set(step, s);
      return step === "a" ? new Promise(() => {}) : Promise.resolve(tail("b-line\n"));
    });
    const { result, rerender } = renderHook(
      ({ step }) => useStepLog("j", step, { enabled: true, pollMs: null }),
      { initialProps: { step: "a" } },
    );
    expect(signals.get("a")?.aborted).toBe(false);
    rerender({ step: "b" });
    await waitFor(() => expect(result.current.lines).toEqual(["b-line"]));
    expect(signals.get("a")?.aborted).toBe(true);
    expect(signals.get("b")?.aborted).toBe(false);
  });

  it("accepts an older body that lands after an empty final poll from a cold replica", async () => {
    let resolveFirst!: (value: ReturnType<typeof tail>) => void;
    const first = new Promise<ReturnType<typeof tail>>((resolve) => {
      resolveFirst = resolve;
    });
    // The first poll is pending when the step goes terminal; the final
    // fetch lands on a replica without this job's chunks.
    getStepLogs.mockReturnValueOnce(first).mockResolvedValue(tail(""));
    const { result, rerender } = renderHook(
      ({ pollMs }: { pollMs: number | null }) => useStepLog("j", "_server", { enabled: true, pollMs }),
      { initialProps: { pollMs: 3000 as number | null } },
    );
    rerender({ pollMs: null });
    await waitFor(() => expect(getStepLogs).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.lines).toEqual([]);

    await act(async () => {
      resolveFirst(tail("known log\n"));
      await first;
    });
    expect(result.current.lines).toEqual(["known log"]);
    expect(result.current.truncated).toBe(false);
    expect(result.current.returnedBytes).toBe("known log\n".length);
  });

  it("shows nothing, and no truncation, for a truly empty log", async () => {
    getStepLogs.mockResolvedValue(tail(""));
    const { result } = renderHook(() => useStepLog("j", "build", { enabled: true, pollMs: null }));
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.lines).toEqual([]);
    expect(result.current.truncated).toBe(false);
    expect(result.current.totalBytes).toBe(0);
    expect(result.current.fullState).toBe("idle");
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
