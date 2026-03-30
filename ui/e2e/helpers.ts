import type { Page } from "@playwright/test";

export async function login(page: Page) {
  // Navigate to root — if we're already authenticated (cookie present),
  // the app won't redirect to /login and we can skip the login flow.
  await page.goto("/");
  const url = page.url();
  if (!url.includes("/login")) {
    // Already authenticated
    await page.waitForLoadState("networkidle");
    return;
  }

  // Not authenticated — perform login with rate-limit retry
  for (let attempt = 0; attempt < 5; attempt++) {
    await page.goto("/login");
    await page.fill('input[id="email"]', "admin@stroem.local");
    await page.fill('input[id="password"]', "admin");

    const responsePromise = page.waitForResponse(
      (r) => r.url().includes("/api/auth/login"),
      { timeout: 10_000 },
    );
    await page.click('button[type="submit"]');

    const response = await responsePromise.catch(() => null);
    if (response && response.status() === 429) {
      await page.waitForTimeout(3000 * (attempt + 1));
      continue;
    }

    await page.waitForURL("/", { timeout: 10_000 });
    return;
  }
  throw new Error("Login failed after 5 rate-limit retries");
}

// Cached token to avoid rate limiting on /api/auth/login.
// JWT has 15min TTL — more than enough for a test run.
let cachedToken: string | null = null;
let tokenBaseURL: string | null = null;

/** Get a Bearer token for server-side API calls (cached, with rate-limit retry). */
export async function getAuthToken(baseURL: string): Promise<string> {
  if (cachedToken && tokenBaseURL === baseURL) {
    return cachedToken;
  }
  // Retry loop to handle 429 rate limiting
  for (let attempt = 0; attempt < 5; attempt++) {
    const res = await fetch(`${baseURL}/api/auth/login`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        email: "admin@stroem.local",
        password: "admin",
      }),
    });
    if (res.ok) {
      const { access_token } = await res.json();
      cachedToken = access_token;
      tokenBaseURL = baseURL;
      return access_token;
    }
    if (res.status === 429) {
      const body = await res.json().catch(() => ({}));
      const wait = (body.retry_after_secs || 2) * 1000 + 500;
      await new Promise((r) => setTimeout(r, wait));
      continue;
    }
    throw new Error(`Login failed: ${res.status} ${await res.text()}`);
  }
  throw new Error("Login failed after 5 attempts (rate limited)");
}

/** Authenticated fetch wrapper for server-side API calls from tests. */
export async function apiFetch(
  baseURL: string,
  path: string,
  token: string,
  init?: RequestInit,
): Promise<Response> {
  return fetch(`${baseURL}${path}`, {
    ...init,
    headers: {
      ...((init?.headers as Record<string, string>) || {}),
      Authorization: `Bearer ${token}`,
    },
  });
}

export async function triggerJob(
  baseURL: string,
  taskName: string,
  input: Record<string, unknown> = {},
  workspace = "default",
): Promise<string> {
  const token = await getAuthToken(baseURL);

  const res = await apiFetch(
    baseURL,
    `/api/workspaces/${encodeURIComponent(workspace)}/tasks/${encodeURIComponent(taskName)}/execute`,
    token,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ input }),
    },
  );

  if (!res.ok) {
    throw new Error(`Failed to trigger job: ${res.status} ${await res.text()}`);
  }

  const data = await res.json();
  return data.job_id;
}

/**
 * Wait for a job to reach a terminal state (completed or failed).
 * Uses authenticated fetch with cached token.
 */
export async function waitForJob(
  baseURL: string,
  jobId: string,
  timeoutMs = 30000,
): Promise<{ status: string; [key: string]: unknown }> {
  const token = await getAuthToken(baseURL);
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    const res = await apiFetch(baseURL, `/api/jobs/${jobId}`, token);
    if (res.ok) {
      const data = await res.json();
      if (data.status === "completed" || data.status === "failed") {
        return data;
      }
    }
    await new Promise((r) => setTimeout(r, 1000));
  }
  throw new Error(`Job ${jobId} did not complete within ${timeoutMs}ms`);
}
