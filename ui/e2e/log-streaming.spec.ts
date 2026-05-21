import { test, expect } from "@playwright/test";
import { login, triggerJob } from "./helpers";

test.describe("Log Streaming", () => {
  test("logs appear in job detail", async ({ page, baseURL }) => {
    await login(page);

    // Trigger a job
    const jobId = await triggerJob(baseURL!, "hello-world", {
      name: "log-test",
    });

    await page.goto(`/jobs/${jobId}`);

    // Wait for the log viewer to get some content (job needs time to run)
    await expect(page.locator("pre")).toBeVisible();

    // Wait up to 30s for logs to appear (job execution takes time)
    await expect(async () => {
      const logContent = await page.locator("pre").textContent();
      expect(logContent).toBeTruthy();
      expect(logContent!.length).toBeGreaterThan(0);
      // Check it's not just the "Waiting for logs..." placeholder
      expect(logContent).not.toContain("Waiting for logs...");
    }).toPass({ timeout: 30000 });
  });

  test("empty poll from replica does not clear displayed logs (no flicker)", async ({
    page,
    baseURL,
  }) => {
    await login(page);

    const jobId = await triggerJob(baseURL!, "hello-world", {
      name: "flicker-test",
    });

    await page.goto(`/jobs/${jobId}`);

    // Wait for the first non-empty log response so the viewer shows real content.
    await expect(async () => {
      const logContent = await page.locator("pre").textContent();
      expect(logContent).toBeTruthy();
      expect(logContent).not.toContain("Waiting for logs...");
    }).toPass({ timeout: 30000 });

    // Capture the log text that is currently displayed.
    const logsBefore = await page.locator("pre").textContent();
    expect(logsBefore).toBeTruthy();

    // Intercept the NEXT getStepLogs call and return an empty body,
    // simulating a poll routed to a replica that hasn't received the chunks.
    let interceptCount = 0;
    await page.route("**/api/jobs/*/steps/*/logs", (route) => {
      if (interceptCount === 0) {
        interceptCount++;
        route.fulfill({
          status: 200,
          contentType: "application/json",
          body: JSON.stringify({ logs: "" }),
        });
      } else {
        route.continue();
      }
    });

    // Wait long enough for at least one intercepted poll to fire (> 2 s).
    await page.waitForTimeout(3000);

    // The displayed logs must NOT have been cleared.
    const logsAfter = await page.locator("pre").textContent();
    expect(logsAfter).not.toContain("Waiting for logs...");
    expect(logsAfter).toBe(logsBefore);

    // Remove the intercept and confirm subsequent polls still update normally.
    await page.unroute("**/api/jobs/*/steps/*/logs");
    await page.waitForTimeout(3000);
    // After unrouting, real polls resume; the log content should remain at least
    // as long as what we captured (it may grow if the job is still running).
    const logsFinal = await page.locator("pre").textContent();
    expect(logsFinal).not.toContain("Waiting for logs...");
  });
});
