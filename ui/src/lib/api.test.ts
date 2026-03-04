import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import {
  setAccessToken,
  getAccessToken,
  tryRestoreSession,
  ApiError,
  login,
  logout,
  getServerConfig,
} from "./api";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function mockFetch(
  status: number,
  body: unknown,
  headers: Record<string, string> = {},
): void {
  const responseHeaders = new Headers({
    "Content-Type": "application/json",
    ...headers,
  });
  vi.stubGlobal(
    "fetch",
    vi.fn().mockResolvedValue({
      ok: status >= 200 && status < 300,
      status,
      statusText: "OK",
      headers: responseHeaders,
      json: () => Promise.resolve(body),
    }),
  );
}

// ---------------------------------------------------------------------------
// Module-level token state
// ---------------------------------------------------------------------------

describe("token helpers", () => {
  afterEach(() => {
    setAccessToken(null);
  });

  it("getAccessToken returns null by default", () => {
    setAccessToken(null);
    expect(getAccessToken()).toBeNull();
  });

  it("setAccessToken and getAccessToken round-trip", () => {
    setAccessToken("my-token");
    expect(getAccessToken()).toBe("my-token");
  });
});

// ---------------------------------------------------------------------------
// TokenManager refresh deduplication
// ---------------------------------------------------------------------------

describe("TokenManager refresh deduplication", () => {
  beforeEach(() => {
    setAccessToken(null);
    vi.restoreAllMocks();
  });

  afterEach(() => {
    setAccessToken(null);
    vi.unstubAllGlobals();
  });

  it("deduplicates concurrent refresh calls into a single fetch", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      headers: new Headers({ "Content-Type": "application/json" }),
      json: () => Promise.resolve({ access_token: "new-token", token_type: "bearer" }),
    });
    vi.stubGlobal("fetch", fetchMock);

    // Fire 3 concurrent refresh attempts
    const [r1, r2, r3] = await Promise.all([
      tryRestoreSession(),
      tryRestoreSession(),
      tryRestoreSession(),
    ]);

    expect(r1).toBe(true);
    expect(r2).toBe(true);
    expect(r3).toBe(true);
    // Only one actual fetch should have been made
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(getAccessToken()).toBe("new-token");
  });

  it("allows a new refresh after a failed one completes", async () => {
    const fetchMock = vi
      .fn()
      // First refresh fails
      .mockResolvedValueOnce({
        ok: false,
        status: 401,
        headers: new Headers(),
        json: () => Promise.resolve({}),
      })
      // Second refresh succeeds
      .mockResolvedValueOnce({
        ok: true,
        status: 200,
        headers: new Headers({ "Content-Type": "application/json" }),
        json: () => Promise.resolve({ access_token: "recovered", token_type: "bearer" }),
      });
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
// apiFetch — tested indirectly via exported API functions that call it
// ---------------------------------------------------------------------------

describe("apiFetch behaviour", () => {
  beforeEach(() => {
    // Ensure no leftover token between tests
    setAccessToken(null);
    vi.restoreAllMocks();
  });

  afterEach(() => {
    setAccessToken(null);
    vi.unstubAllGlobals();
  });

  it("returns null for 204 No Content", async () => {
    // We need a route that calls apiFetch and returns its result directly.
    // deleteApiKey wraps the result but discards it, so use listWorkspaces
    // shape instead.  The simplest approach: stub fetch for the refresh call
    // (POST /api/auth/refresh) and then for the actual GET.
    vi.stubGlobal(
      "fetch",
      vi
        .fn()
        // First call: refresh (triggered because accessToken is null)
        .mockResolvedValueOnce({
          ok: false,
          status: 401,
          headers: new Headers(),
          json: () => Promise.resolve({}),
        })
        // Second call: the actual API request
        .mockResolvedValueOnce({
          ok: true,
          status: 204,
          statusText: "No Content",
          headers: new Headers({ "content-length": "0" }),
          json: () => Promise.resolve(null),
        }),
    );

    // deleteApiKey calls apiFetch<{status}> and discards the return, but we
    // just need it to not throw. The test proves 204 doesn't throw.
    const { deleteApiKey } = await import("./api");
    await expect(deleteApiKey("abc123")).resolves.toBeUndefined();
  });

  it("throws ApiError on non-ok responses", async () => {
    vi.stubGlobal(
      "fetch",
      vi
        .fn()
        // refresh attempt
        .mockResolvedValueOnce({
          ok: false,
          status: 401,
          headers: new Headers(),
          json: () => Promise.resolve({}),
        })
        // actual request — 404
        .mockResolvedValueOnce({
          ok: false,
          status: 404,
          statusText: "Not Found",
          headers: new Headers({ "Content-Type": "application/json" }),
          json: () => Promise.resolve({ error: "not found" }),
        }),
    );

    const { getJob } = await import("./api");
    await expect(getJob("missing-id")).rejects.toBeInstanceOf(ApiError);
  });

  it("ApiError carries the HTTP status code", async () => {
    vi.stubGlobal(
      "fetch",
      vi
        .fn()
        .mockResolvedValueOnce({
          ok: false,
          status: 401,
          headers: new Headers(),
          json: () => Promise.resolve({}),
        })
        .mockResolvedValueOnce({
          ok: false,
          status: 403,
          statusText: "Forbidden",
          headers: new Headers({ "Content-Type": "application/json" }),
          json: () => Promise.resolve({ error: "forbidden" }),
        }),
    );

    const { listUsers } = await import("./api");
    try {
      await listUsers();
      expect.fail("should have thrown");
    } catch (err) {
      expect(err).toBeInstanceOf(ApiError);
      expect((err as ApiError).status).toBe(403);
      expect((err as ApiError).message).toBe("forbidden");
    }
  });

  it("parses JSON for successful responses", async () => {
    // Provide a valid access token so the preemptive refresh is skipped
    setAccessToken("valid-token");
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        status: 200,
        statusText: "OK",
        headers: new Headers({ "Content-Type": "application/json" }),
        json: () => Promise.resolve([{ name: "default", revision: "abc" }]),
      }),
    );

    const { listWorkspaces } = await import("./api");
    const result = await listWorkspaces();
    expect(result).toEqual([{ name: "default", revision: "abc" }]);
  });
});

// ---------------------------------------------------------------------------
// login
// ---------------------------------------------------------------------------

describe("login", () => {
  afterEach(() => {
    setAccessToken(null);
    vi.unstubAllGlobals();
  });

  it("sets the access token on success and returns token response", async () => {
    mockFetch(200, {
      access_token: "access-abc",
      token_type: "bearer",
    });

    const result = await login("user@example.com", "secret");
    expect(result.access_token).toBe("access-abc");
    expect(getAccessToken()).toBe("access-abc");
  });

  it("throws ApiError on invalid credentials", async () => {
    mockFetch(401, { error: "invalid credentials" });
    await expect(login("x@y.com", "wrong")).rejects.toBeInstanceOf(ApiError);
    await expect(login("x@y.com", "wrong")).rejects.toMatchObject({
      status: 401,
    });
  });
});

// ---------------------------------------------------------------------------
// logout
// ---------------------------------------------------------------------------

describe("logout", () => {
  afterEach(() => {
    setAccessToken(null);
    vi.unstubAllGlobals();
  });

  it("clears the access token", async () => {
    setAccessToken("some-token");
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({ ok: true, status: 200 }),
    );

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
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("returns defaults when the server returns a non-ok response", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({ ok: false, status: 500 }),
    );
    const cfg = await getServerConfig();
    expect(cfg).toEqual({
      authRequired: false,
      hasInternalAuth: false,
      oidcProviders: [],
      version: null,
    });
  });

  it("returns defaults when fetch throws (network error)", async () => {
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("offline")));
    const cfg = await getServerConfig();
    expect(cfg).toEqual({
      authRequired: false,
      hasInternalAuth: false,
      oidcProviders: [],
      version: null,
    });
  });

  it("parses server config correctly", async () => {
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

// ---------------------------------------------------------------------------
// ApiError
// ---------------------------------------------------------------------------

describe("ApiError", () => {
  it("has name 'ApiError'", () => {
    const err = new ApiError(500, "boom");
    expect(err.name).toBe("ApiError");
  });

  it("is an instance of Error", () => {
    const err = new ApiError(400, "bad");
    expect(err).toBeInstanceOf(Error);
  });

  it("exposes status and message", () => {
    const err = new ApiError(422, "unprocessable");
    expect(err.status).toBe(422);
    expect(err.message).toBe("unprocessable");
  });
});
