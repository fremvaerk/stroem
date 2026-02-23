import { test, expect } from "@playwright/test";
import { login } from "./helpers";

test.describe("Tasks", () => {
  test.beforeEach(async ({ page }) => {
    await login(page);
  });

  test("tasks list renders", async ({ page }) => {
    await page.click("text=Tasks");
    await expect(page).toHaveURL("/tasks");
    await expect(page.locator("h1")).toHaveText("Tasks");
    // Should have at least one task from the workspace
    await expect(page.locator("table tbody tr").first()).toBeVisible();
  });

  test("task detail shows input schema", async ({ page }) => {
    await page.click("text=Tasks");
    // Click the first task link
    await page.locator("table tbody tr td a").first().click();
    await expect(page.locator("h1")).toBeVisible();
    // Should show Run Task button
    await expect(page.locator("text=Run Task")).toBeVisible();
  });

  test("task detail shows DAG for multi-step task", async ({ page }) => {
    // Navigate to data-pipeline task (multi-step: transform -> summarize)
    await page.goto("/workspaces/default/tasks/data-pipeline");
    await expect(
      page.getByRole("heading", { name: "data-pipeline" }),
    ).toBeVisible();

    // DAG should render for multi-step tasks
    await expect(page.locator(".react-flow")).toBeVisible();

    // Step list should still be visible below the DAG
    await expect(page.getByText("transform")).toBeVisible();
    await expect(page.getByText("summarize")).toBeVisible();
  });

  test("run task creates job", async ({ page }) => {
    await page.click("text=Tasks");
    await page.locator("table tbody tr td a").first().click();

    // Click Run Task
    await page.click("text=Run Task");
    // Dialog should open
    await expect(page.locator('[role="dialog"]')).toBeVisible();

    // Click Run in the dialog
    await page.locator('[role="dialog"] button:has-text("Run")').click();

    // Should redirect to job detail
    await page.waitForURL(/\/jobs\/.+/);
    await expect(page.locator("h1")).toBeVisible();
  });
});
