import type { Page } from "@playwright/test";

export async function login(page: Page) {
  await page.goto("/login");
  await page.fill('input[id="email"]', "admin@stroem.local");
  await page.fill('input[id="password"]', "admin");
  await page.click('button[type="submit"]');
  await page.waitForURL("/");
}

export async function triggerJob(
  baseURL: string,
  taskName: string,
  input: Record<string, unknown> = {},
): Promise<string> {
  // Login to get token
  const loginRes = await fetch(`${baseURL}/api/auth/login`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      email: "admin@stroem.local",
      password: "admin",
    }),
  });

  let headers: Record<string, string> = {
    "Content-Type": "application/json",
  };

  if (loginRes.ok) {
    const { access_token } = await loginRes.json();
    headers["Authorization"] = `Bearer ${access_token}`;
  }

  const res = await fetch(
    `${baseURL}/api/tasks/${encodeURIComponent(taskName)}/execute`,
    {
      method: "POST",
      headers,
      body: JSON.stringify({ input }),
    },
  );

  if (!res.ok) {
    throw new Error(`Failed to trigger job: ${res.status} ${await res.text()}`);
  }

  const data = await res.json();
  return data.job_id;
}
