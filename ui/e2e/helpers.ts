import type { Page } from "@playwright/test";

export async function login(page: Page) {
  await page.goto("/login");
  await page.fill('input[id="email"]', "admin@stroem.local");
  await page.fill('input[id="password"]', "admin");
  await page.click('button[type="submit"]');
  await page.waitForURL("/");
}

/** Get a Bearer token for server-side API calls. */
export async function getAuthToken(baseURL: string): Promise<string> {
  const res = await fetch(`${baseURL}/api/auth/login`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      email: "admin@stroem.local",
      password: "admin",
    }),
  });
  if (!res.ok) {
    throw new Error(`Login failed: ${res.status} ${await res.text()}`);
  }
  const { access_token } = await res.json();
  return access_token;
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
 * Uses authenticated fetch.
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
