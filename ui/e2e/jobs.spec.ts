import { test, expect } from "@playwright/test";
import { login, triggerJob } from "./helpers";

test.describe("Jobs", () => {
  test.beforeEach(async ({ page }) => {
    await login(page);
  });

  test("jobs list shows status badges", async ({ page, baseURL }) => {
    // Trigger a job so there's at least one
    await triggerJob(baseURL!, "hello-world", { name: "e2e-test" });

    await page.click("text=Jobs");
    await expect(page).toHaveURL("/jobs");
    await expect(page.locator("h1")).toHaveText("Jobs");

    // Wait for at least one job to appear
    await expect(page.locator("table tbody tr").first()).toBeVisible();
    // Should have status badges
    await expect(
      page.locator("table tbody tr").first().locator('[class*="badge"]'),
    ).toBeVisible();
  });

  test("job detail shows steps", async ({ page, baseURL }) => {
    const jobId = await triggerJob(baseURL!, "hello-world", {
      name: "e2e-steps-test",
    });

    await page.goto(`/jobs/${jobId}`);
    // Should show the task name
    await expect(page.locator("h1")).toHaveText("hello-world");
    // Should show step timeline
    await expect(page.locator("text=Steps")).toBeVisible();
    // Should show logs section
    await expect(page.locator("text=Logs")).toBeVisible();
  });
});
