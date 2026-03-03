import {
  describe,
  it,
  expect,
  vi,
  beforeEach,
  afterEach,
} from "vitest";
import {
  ApiError,
  setAccessToken,
  getAccessToken,
} from "../api";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/**
 * Build a mock fetch response object that can be read multiple times (unlike a
 * real Response whose body stream is consumed on first `.json()` call).
 */
function makeMockResponse(
  status: number,
  body: unknown,
  extraHeaders: Record<string, string> = {},
) {
  return {
    ok: status >= 200 && status < 300,
    status,
    statusText: String(status),
    headers: {
      get: (name: string) => extraHeaders[name.toLowerCase()] ?? null,
    },
    json: vi.fn().mockResolvedValue(body),
  };
}

// ---------------------------------------------------------------------------
// ApiError
// ---------------------------------------------------------------------------
describe("ApiError", () => {
  it("sets the name to 'ApiError'", () => {
    const err = new ApiError(404, "Not Found");
    expect(err.name).toBe("ApiError");
  });

  it("stores the HTTP status code", () => {
    const err = new ApiError(500, "Internal Server Error");
    expect(err.status).toBe(500);
  });

  it("stores the message on the Error instance", () => {
    const err = new ApiError(403, "Forbidden");
    expect(err.message).toBe("Forbidden");
  });

  it("is an instance of Error", () => {
    const err = new ApiError(400, "Bad Request");
    expect(err).toBeInstanceOf(Error);
  });

  it("is an instance of ApiError", () => {
    const err = new ApiError(401, "Unauthorized");
    expect(err).toBeInstanceOf(ApiError);
  });
});

// ---------------------------------------------------------------------------
// setAccessToken / getAccessToken
// ---------------------------------------------------------------------------
describe("setAccessToken / getAccessToken", () => {
  afterEach(() => {
    setAccessToken(null);
  });

  it("returns null after being explicitly cleared", () => {
    setAccessToken(null);
    expect(getAccessToken()).toBeNull();
  });

  it("stores a token set via setAccessToken", () => {
    setAccessToken("my-token-abc");
    expect(getAccessToken()).toBe("my-token-abc");
  });

  it("clears the token when set to null", () => {
    setAccessToken("some-token");
    setAccessToken(null);
    expect(getAccessToken()).toBeNull();
  });

  it("overwrites a previous token", () => {
    setAccessToken("token-1");
    setAccessToken("token-2");
    expect(getAccessToken()).toBe("token-2");
  });
});

// ---------------------------------------------------------------------------
// apiFetch — tested via the exported wrappers that delegate to it.
// We use object-mock responses (not real Response) so the body is re-readable.
// ---------------------------------------------------------------------------

beforeEach(() => {
  setAccessToken(null);
});

afterEach(() => {
  vi.restoreAllMocks();
  setAccessToken(null);
});

describe("apiFetch (via getStats)", () => {
  it("returns parsed JSON on a 200 response", async () => {
    const { getStats } = await import("../api");

    const payload = { pending: 1, running: 2, completed: 10, failed: 0, cancelled: 0 };

    // Provide an access token so the preemptive refresh is skipped.
    setAccessToken("valid-token");
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(makeMockResponse(200, payload)));

    const result = await getStats();
    expect(result).toEqual(payload);

    vi.unstubAllGlobals();
  });

  it("attaches Authorization header when an access token is set", async () => {
    const { getStats } = await import("../api");

    setAccessToken("test-bearer-token");
    const fetchMock = vi.fn().mockResolvedValue(
      makeMockResponse(200, { pending: 0, running: 0, completed: 0, failed: 0, cancelled: 0 }),
    );
    vi.stubGlobal("fetch", fetchMock);

    await getStats();

    const lastCall = fetchMock.mock.calls.at(-1)!;
    const requestHeaders = lastCall[1]?.headers as Record<string, string>;
    expect(requestHeaders["Authorization"]).toBe("Bearer test-bearer-token");

    vi.unstubAllGlobals();
  });

  it("throws ApiError with correct status and message on a non-ok response", async () => {
    const { getStats } = await import("../api");

    setAccessToken("valid-token");
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(makeMockResponse(500, { error: "server exploded" })),
    );

    let caught: unknown;
    try {
      await getStats();
    } catch (err) {
      caught = err;
    }

    expect(caught).toBeInstanceOf(ApiError);
    expect((caught as ApiError).status).toBe(500);
    expect((caught as ApiError).message).toBe("server exploded");

    vi.unstubAllGlobals();
  });

  it("returns null for a 204 No Content response", async () => {
    const { cancelJob } = await import("../api");

    setAccessToken("valid-token");
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        status: 204,
        statusText: "No Content",
        headers: { get: () => null },
        json: vi.fn().mockResolvedValue(null),
      }),
    );

    const result = await cancelJob("job-id-123");
    expect(result).toBeNull();

    vi.unstubAllGlobals();
  });

  it("attempts token refresh on 401 and retries the request", async () => {
    const { getStats } = await import("../api");

    const statsPayload = { pending: 3, running: 1, completed: 5, failed: 0, cancelled: 0 };

    const fetchMock = vi
      .fn()
      // 1. Preemptive refresh — no token so apiFetch calls refresh first.
      .mockResolvedValueOnce(makeMockResponse(200, { access_token: "refreshed-token" }))
      // 2. First /api/stats attempt — 401.
      .mockResolvedValueOnce(makeMockResponse(401, { error: "token expired" }))
      // 3. Reactive refresh after 401.
      .mockResolvedValueOnce(makeMockResponse(200, { access_token: "refreshed-token-2" }))
      // 4. Retried /api/stats — 200.
      .mockResolvedValueOnce(makeMockResponse(200, statsPayload));

    vi.stubGlobal("fetch", fetchMock);

    const result = await getStats();
    expect(result).toEqual(statsPayload);
    expect(fetchMock).toHaveBeenCalledTimes(4);

    vi.unstubAllGlobals();
  });

  it("throws ApiError when 401 refresh also fails and retry fails", async () => {
    const { getStats } = await import("../api");

    setAccessToken("stale-token");
    const fetchMock = vi
      .fn()
      // First /api/stats attempt — 401.
      .mockResolvedValueOnce(makeMockResponse(401, { error: "token expired" }))
      // Reactive refresh — fails (401 on refresh).
      .mockResolvedValueOnce(makeMockResponse(401, { error: "refresh invalid" }))
      // Retry after failed refresh — still 401.
      .mockResolvedValueOnce(makeMockResponse(401, { error: "still unauthorized" }));

    vi.stubGlobal("fetch", fetchMock);

    await expect(getStats()).rejects.toBeInstanceOf(ApiError);

    vi.unstubAllGlobals();
  });

  it("sends Content-Type: application/json when a request body is present", async () => {
    const { executeTask } = await import("../api");

    setAccessToken("valid-token");
    const fetchMock = vi
      .fn()
      .mockResolvedValue(makeMockResponse(200, { job_id: "abc" }));
    vi.stubGlobal("fetch", fetchMock);

    await executeTask("default", "my-task", { key: "value" });

    const postCall = fetchMock.mock.calls.find(
      (c) => (c[1]?.method as string) === "POST",
    );
    expect(postCall).toBeDefined();
    const headers = postCall![1]?.headers as Record<string, string>;
    expect(headers["Content-Type"]).toBe("application/json");

    vi.unstubAllGlobals();
  });
});
