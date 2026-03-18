import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import {
  ApiError,
  setAccessToken,
  getAccessToken,
  tryRestoreSession,
  login,
  logout,
  getServerConfig,
} from "../api";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/**
 * Build a mock fetch response that is re-readable (unlike a real Response
 * whose body stream is consumed on the first `.json()` call).
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

/**
 * Stub global fetch with a single response. Useful for simple cases where
 * no token is in flight (e.g. login, logout, getServerConfig).
 */
function stubFetch(status: number, body: unknown, extraHeaders: Record<string, string> = {}) {
  vi.stubGlobal(
    "fetch",
    vi.fn().mockResolvedValue(makeMockResponse(status, body, extraHeaders)),
  );
}

// ---------------------------------------------------------------------------
// Top-level reset so every suite starts clean
// ---------------------------------------------------------------------------

beforeEach(() => {
  setAccessToken(null);
});

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  setAccessToken(null);
});

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
// TokenManager — refresh deduplication
// ---------------------------------------------------------------------------

describe("TokenManager refresh deduplication", () => {
  it("deduplicates concurrent refresh calls into a single fetch", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      makeMockResponse(200, { access_token: "new-token", token_type: "bearer" }),
    );
    vi.stubGlobal("fetch", fetchMock);

    // Fire 3 concurrent refresh attempts while no token is set
    const [r1, r2, r3] = await Promise.all([
      tryRestoreSession(),
      tryRestoreSession(),
      tryRestoreSession(),
    ]);

    expect(r1).toBe(true);
    expect(r2).toBe(true);
    expect(r3).toBe(true);
    // Only one actual HTTP request should have been made
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(getAccessToken()).toBe("new-token");
  });

  it("allows a new refresh after a failed one completes", async () => {
    const fetchMock = vi
      .fn()
      // First refresh fails
      .mockResolvedValueOnce(makeMockResponse(401, {}))
      // Second refresh succeeds
      .mockResolvedValueOnce(
        makeMockResponse(200, { access_token: "recovered", token_type: "bearer" }),
      );
    vi.stubGlobal("fetch", fetchMock);

    const first = await tryRestoreSession();
    expect(first).toBe(false);

    const second = await tryRestoreSession();
    expect(second).toBe(true);
    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(getAccessToken()).toBe("recovered");
  });
});

// ---------------------------------------------------------------------------
// apiFetch — tested via exported wrappers that delegate to it
// ---------------------------------------------------------------------------

describe("apiFetch", () => {
  it("returns parsed JSON on a 200 response", async () => {
    const { getStats } = await import("../api");

    const payload = { pending: 1, running: 2, completed: 10, failed: 0, cancelled: 0, skipped: 0 };
    setAccessToken("valid-token");
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(makeMockResponse(200, payload)));

    const result = await getStats();
    expect(result).toEqual(payload);
  });

  it("attaches Authorization header when an access token is set", async () => {
    const { getStats } = await import("../api");

    setAccessToken("test-bearer-token");
    const fetchMock = vi.fn().mockResolvedValue(
      makeMockResponse(200, { pending: 0, running: 0, completed: 0, failed: 0, cancelled: 0, skipped: 0 }),
    );
    vi.stubGlobal("fetch", fetchMock);

    await getStats();

    const lastCall = fetchMock.mock.calls.at(-1)!;
    const requestHeaders = lastCall[1]?.headers as Record<string, string>;
    expect(requestHeaders["Authorization"]).toBe("Bearer test-bearer-token");
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
  });

  it("attempts token refresh on 401 and retries the request", async () => {
    const { getStats } = await import("../api");

    const statsPayload = { pending: 3, running: 1, completed: 5, failed: 0, cancelled: 0, skipped: 0 };

    const fetchMock = vi
      .fn()
      // 1. Preemptive refresh — no token so apiFetch calls refresh first
      .mockResolvedValueOnce(makeMockResponse(200, { access_token: "refreshed-token" }))
      // 2. First /api/stats attempt — 401 (token expired mid-flight)
      .mockResolvedValueOnce(makeMockResponse(401, { error: "token expired" }))
      // 3. Reactive refresh after 401
      .mockResolvedValueOnce(makeMockResponse(200, { access_token: "refreshed-token-2" }))
      // 4. Retried /api/stats — 200
      .mockResolvedValueOnce(makeMockResponse(200, statsPayload));

    vi.stubGlobal("fetch", fetchMock);

    const result = await getStats();
    expect(result).toEqual(statsPayload);
    expect(fetchMock).toHaveBeenCalledTimes(4);
  });

  it("throws ApiError when 401 refresh also fails and retry fails", async () => {
    const { getStats } = await import("../api");

    setAccessToken("stale-token");
    const fetchMock = vi
      .fn()
      // First /api/stats attempt — 401
      .mockResolvedValueOnce(makeMockResponse(401, { error: "token expired" }))
      // Reactive refresh — fails
      .mockResolvedValueOnce(makeMockResponse(401, { error: "refresh invalid" }))
      // Retry after failed refresh — still 401
      .mockResolvedValueOnce(makeMockResponse(401, { error: "still unauthorized" }));

    vi.stubGlobal("fetch", fetchMock);

    await expect(getStats()).rejects.toBeInstanceOf(ApiError);
  });

  it("sends Content-Type: application/json when a request body is present", async () => {
    const { executeTask } = await import("../api");

    setAccessToken("valid-token");
    const fetchMock = vi.fn().mockResolvedValue(makeMockResponse(200, { job_id: "abc" }));
    vi.stubGlobal("fetch", fetchMock);

    await executeTask("default", "my-task", { key: "value" });

    const postCall = fetchMock.mock.calls.find(
      (c) => (c[1]?.method as string) === "POST",
    );
    expect(postCall).toBeDefined();
    const headers = postCall![1]?.headers as Record<string, string>;
    expect(headers["Content-Type"]).toBe("application/json");
  });
});

// ---------------------------------------------------------------------------
// login
// ---------------------------------------------------------------------------

describe("login", () => {
  it("sets the access token on success and returns the token response", async () => {
    stubFetch(200, { access_token: "access-abc", token_type: "bearer" });

    const result = await login("user@example.com", "secret");
    expect(result.access_token).toBe("access-abc");
    expect(getAccessToken()).toBe("access-abc");
  });

  it("throws ApiError on invalid credentials", async () => {
    stubFetch(401, { error: "invalid credentials" });

    await expect(login("x@y.com", "wrong")).rejects.toBeInstanceOf(ApiError);
    await expect(login("x@y.com", "wrong")).rejects.toMatchObject({ status: 401 });
  });
});

// ---------------------------------------------------------------------------
// logout
// ---------------------------------------------------------------------------

describe("logout", () => {
  it("clears the access token on successful logout", async () => {
    setAccessToken("some-token");
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: true, status: 200 }));

    await logout();
    expect(getAccessToken()).toBeNull();
  });

  it("clears the access token even when the server returns an error", async () => {
    setAccessToken("some-token");
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("network")));

    await logout();
    expect(getAccessToken()).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// getServerConfig
// ---------------------------------------------------------------------------

describe("getServerConfig", () => {
  it("returns safe defaults when the server returns a non-ok response", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: false, status: 500 }));

    const cfg = await getServerConfig();
    expect(cfg).toEqual({
      authRequired: false,
      hasInternalAuth: false,
      oidcProviders: [],
      version: null,
      aclEnabled: false,
    });
  });

  it("returns safe defaults when fetch throws (network error)", async () => {
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("offline")));

    const cfg = await getServerConfig();
    expect(cfg).toEqual({
      authRequired: false,
      hasInternalAuth: false,
      oidcProviders: [],
      version: null,
      aclEnabled: false,
    });
  });

  it("parses a full server config response correctly", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        status: 200,
        json: () =>
          Promise.resolve({
            auth_required: true,
            has_internal_auth: true,
            oidc_providers: [{ id: "google", display_name: "Google" }],
          }),
      }),
    );

    const cfg = await getServerConfig();
    expect(cfg.authRequired).toBe(true);
    expect(cfg.hasInternalAuth).toBe(true);
    expect(cfg.oidcProviders).toEqual([{ id: "google", display_name: "Google" }]);
  });

  it("treats missing oidc_providers as empty array", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        status: 200,
        json: () =>
          Promise.resolve({ auth_required: false, has_internal_auth: false }),
      }),
    );

    const cfg = await getServerConfig();
    expect(cfg.oidcProviders).toEqual([]);
  });
});
