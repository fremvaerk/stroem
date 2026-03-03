import { describe, it, expect, vi, afterEach } from "vitest";
import { renderHook, waitFor, act } from "@testing-library/react";
import { useAsyncData } from "../use-async-data";

// Restore real timers after every test so tests that use fake timers don't
// bleed into the next test (waitFor relies on real timers).
afterEach(() => {
  vi.useRealTimers();
});

// ---------------------------------------------------------------------------
// Basic fetch behaviour (real timers — waitFor works normally)
// ---------------------------------------------------------------------------
describe("useAsyncData", () => {
  it("starts with loading=true and data=null", () => {
    const fetcher = vi.fn(() => new Promise<string[]>(() => {}));
    const { result } = renderHook(() => useAsyncData(fetcher));

    expect(result.current.loading).toBe(true);
    expect(result.current.data).toBeNull();
  });

  it("sets data and loading=false after the fetcher resolves", async () => {
    const fetcher = vi.fn().mockResolvedValue(["item-a", "item-b"]);
    const { result } = renderHook(() => useAsyncData(fetcher));

    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.data).toEqual(["item-a", "item-b"]);
  });

  it("calls the fetcher exactly once on mount", async () => {
    const fetcher = vi.fn().mockResolvedValue(42);
    const { result } = renderHook(() => useAsyncData(fetcher));

    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(fetcher).toHaveBeenCalledTimes(1);
  });

  it("exposes error state when the fetcher throws", async () => {
    const fetcher = vi.fn().mockRejectedValue(new Error("network error"));
    const { result } = renderHook(() => useAsyncData(fetcher));

    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.data).toBeNull();
    expect(result.current.error).toBe("network error");
  });

  it("sets error to null on a successful fetch", async () => {
    const fetcher = vi.fn().mockResolvedValue("ok");
    const { result } = renderHook(() => useAsyncData(fetcher));

    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.error).toBeNull();
  });

  // ---------------------------------------------------------------------------
  // refresh()
  // ---------------------------------------------------------------------------
  describe("refresh()", () => {
    it("re-calls the fetcher and updates data", async () => {
      let callCount = 0;
      const fetcher = vi.fn().mockImplementation(() => {
        callCount += 1;
        return Promise.resolve(`call-${callCount}`);
      });

      const { result } = renderHook(() => useAsyncData(fetcher));
      await waitFor(() => expect(result.current.loading).toBe(false));
      expect(result.current.data).toBe("call-1");

      await act(async () => {
        result.current.refresh();
      });

      await waitFor(() => expect(result.current.data).toBe("call-2"));
      expect(fetcher).toHaveBeenCalledTimes(2);
    });

    it("triggers a second fetch call when refresh() is invoked", async () => {
      const fetcher = vi.fn().mockResolvedValue("result");

      const { result } = renderHook(() => useAsyncData(fetcher));
      await waitFor(() => expect(result.current.loading).toBe(false));

      await act(async () => {
        result.current.refresh();
      });

      await waitFor(() => expect(fetcher).toHaveBeenCalledTimes(2));
    });

    it("clears a previous error when refresh succeeds", async () => {
      let shouldFail = true;
      const fetcher = vi.fn().mockImplementation(() =>
        shouldFail
          ? Promise.reject(new Error("fail"))
          : Promise.resolve("ok"),
      );

      const { result } = renderHook(() => useAsyncData(fetcher));
      await waitFor(() => expect(result.current.loading).toBe(false));
      expect(result.current.error).toBe("fail");

      shouldFail = false;
      await act(async () => {
        result.current.refresh();
      });
      await waitFor(() => expect(result.current.loading).toBe(false));
      expect(result.current.error).toBeNull();
      expect(result.current.data).toBe("ok");
    });
  });

  // ---------------------------------------------------------------------------
  // Polling (fake timers — only active within each test, restored in afterEach)
  // ---------------------------------------------------------------------------
  describe("polling", () => {
    it("does not register a polling interval when pollInterval is 0", async () => {
      vi.useFakeTimers();

      const fetcher = vi.fn().mockResolvedValue("data");
      const setIntervalSpy = vi.spyOn(globalThis, "setInterval");

      renderHook(() => useAsyncData(fetcher, { pollInterval: 0 }));

      // Advance a small amount to let the initial promise settle without
      // triggering any repeating timer.
      await act(async () => {
        await vi.advanceTimersByTimeAsync(1);
      });

      // Any setInterval calls with a delay >= 1 s would be from the hook.
      const hookIntervals = setIntervalSpy.mock.calls.filter(
        ([, delay]) => typeof delay === "number" && delay >= 1_000,
      );
      expect(hookIntervals).toHaveLength(0);

      setIntervalSpy.mockRestore();
    });

    it("polls at the specified interval", async () => {
      vi.useFakeTimers();

      let callCount = 0;
      const fetcher = vi.fn().mockImplementation(() => {
        callCount += 1;
        return Promise.resolve(`call-${callCount}`);
      });

      const { result } = renderHook(() =>
        useAsyncData(fetcher, { pollInterval: 5_000 }),
      );

      // Flush the initial load — the fetcher promise is already resolved so
      // advancing by 1 ms is enough for its microtask to complete.
      await act(async () => {
        await vi.advanceTimersByTimeAsync(1);
      });
      expect(result.current.data).toBe("call-1");

      // Fire the first polling interval.
      await act(async () => {
        await vi.advanceTimersByTimeAsync(5_000);
      });
      expect(result.current.data).toBe("call-2");

      // Fire the second polling interval.
      await act(async () => {
        await vi.advanceTimersByTimeAsync(5_000);
      });
      expect(result.current.data).toBe("call-3");
    });

    it("stops polling after unmount", async () => {
      vi.useFakeTimers();

      const fetcher = vi.fn().mockResolvedValue("data");

      const { unmount } = renderHook(() =>
        useAsyncData(fetcher, { pollInterval: 5_000 }),
      );

      // Complete the initial load.
      await act(async () => {
        await vi.advanceTimersByTimeAsync(1);
      });
      expect(fetcher).toHaveBeenCalledTimes(1);

      // Unmount — the cleanup function calls clearInterval.
      unmount();

      // Advancing time must NOT trigger any further fetcher calls.
      await act(async () => {
        await vi.advanceTimersByTimeAsync(15_000);
      });
      expect(fetcher).toHaveBeenCalledTimes(1);
    });
  });

  // ---------------------------------------------------------------------------
  // Stale response handling (requestIdRef guard, real timers)
  // ---------------------------------------------------------------------------
  describe("stale response handling", () => {
    it("discards a stale response when the fetcher reference changes mid-flight", async () => {
      let resolveFirst!: (v: string) => void;
      const firstFetcher = vi.fn(
        () => new Promise<string>((res) => { resolveFirst = res; }),
      );
      const secondFetcher = vi.fn().mockResolvedValue("new-data");

      const { result, rerender } = renderHook(
        ({ fetcher }: { fetcher: () => Promise<string> }) =>
          useAsyncData(fetcher),
        { initialProps: { fetcher: firstFetcher } },
      );

      // Switch fetcher while first is still pending.
      rerender({ fetcher: secondFetcher });

      // Second fetch settles immediately; hook shows "new-data".
      await waitFor(() => expect(result.current.data).toBe("new-data"));

      // Resolve the stale first fetch — requestIdRef guard should discard it.
      act(() => resolveFirst("stale-data"));
      await act(async () => {});

      // Data must remain "new-data".
      expect(result.current.data).toBe("new-data");
    });

    it("resets data to null immediately when the fetcher reference changes", async () => {
      const firstFetcher = vi.fn().mockResolvedValue("first-result");
      // Second fetcher never resolves — lets us observe the reset state.
      const secondFetcher = vi.fn(() => new Promise<string>(() => {}));

      const { result, rerender } = renderHook(
        ({ fetcher }: { fetcher: () => Promise<string> }) =>
          useAsyncData(fetcher),
        { initialProps: { fetcher: firstFetcher } },
      );

      await waitFor(() => expect(result.current.data).toBe("first-result"));

      // Change fetcher — hook resets data to null immediately in the effect.
      rerender({ fetcher: secondFetcher });
      await waitFor(() => expect(result.current.data).toBeNull());
    });
  });
});
