import { test, expect } from "@playwright/test";
import { login, triggerJob } from "./helpers";

test.describe("Console and network errors audit", () => {
  test("no console errors or failed network requests across all pages", async ({
    page,
    baseURL,
  }) => {
    const consoleErrors: string[] = [];
    const networkErrors: string[] = [];

    // Collect console errors and uncaught exceptions
    page.on("console", (msg) => {
      if (msg.type() === "error") {
        consoleErrors.push(`[console.error] ${msg.text()}`);
      }
    });

    page.on("pageerror", (err) => {
      consoleErrors.push(`[pageerror] ${err.message}`);
    });

    // Collect failed network requests (status >= 400, excluding expected 401s during auth probe)
    page.on("response", (response) => {
      const status = response.status();
      const url = response.url();
      if (status >= 400) {
        // Ignore auth probe (config endpoint may 401 before login, refresh may fail)
        const isAuthProbe =
          url.includes("/api/config") ||
          url.includes("/api/auth/refresh");
        if (!isAuthProbe) {
          networkErrors.push(`[${status}] ${response.request().method()} ${url}`);
        }
      }
    });

    // Trigger a job first so we have data to render
    const jobId = await triggerJob(baseURL!, "hello-world", {
      name: "console-audit",
    });

    // Wait for job to complete
    await expect(async () => {
      const res = await fetch(`${baseURL}/api/jobs/${jobId}`);
      const data = await res.json();
      expect(["completed", "failed"]).toContain(data.status);
    }).toPass({ timeout: 30000 });

    // 1. Login
    await login(page);

    // 2. Dashboard
    await page.goto("/");
    await page.waitForLoadState("networkidle");

    // 3. Tasks page
    await page.goto("/tasks");
    await page.waitForLoadState("networkidle");

    // 4. Task detail
    await page.goto("/tasks/hello-world");
    await page.waitForLoadState("networkidle");

    // 5. Jobs page
    await page.goto("/jobs");
    await page.waitForLoadState("networkidle");

    // 6. Job detail page — exercises step timeline, job input/output cards
    await page.goto(`/jobs/${jobId}`);
    await page.waitForLoadState("networkidle");
    // Wait for steps to render
    await expect(page.getByText("Steps", { exact: true })).toBeVisible();

    // 7. Expand a step — exercises StepDetail (per-step logs, input, output tabs)
    const stepButton = page.locator("button").filter({ hasText: /say.hello/ }).first();
    if (await stepButton.isVisible()) {
      await stepButton.click();
      // Wait for step detail tabs to appear
      await expect(page.getByRole("tab", { name: "Logs" })).toBeVisible();
      await expect(page.getByRole("tab", { name: "Input" })).toBeVisible();
      await expect(page.getByRole("tab", { name: "Output" })).toBeVisible();
      // Wait for step log fetch
      await page.waitForLoadState("networkidle");

      // Click Input tab
      await page.getByRole("tab", { name: "Input" }).click();
      await page.waitForTimeout(500);

      // Click Output tab
      await page.getByRole("tab", { name: "Output" }).click();
      await page.waitForTimeout(500);

      // Click Logs tab back
      await page.getByRole("tab", { name: "Logs" }).click();
      await page.waitForTimeout(500);
    }

    // Assert zero errors
    if (consoleErrors.length > 0) {
      console.log("Console errors found:");
      for (const err of consoleErrors) console.log("  ", err);
    }
    if (networkErrors.length > 0) {
      console.log("Network errors found:");
      for (const err of networkErrors) console.log("  ", err);
    }

    expect(consoleErrors, "Expected no console errors").toHaveLength(0);
    expect(networkErrors, "Expected no failed network requests").toHaveLength(0);
  });
});
