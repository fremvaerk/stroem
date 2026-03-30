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
});
