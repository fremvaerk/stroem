import { test, expect } from "@playwright/test";
import { login } from "./helpers";

test.describe("Tasks", () => {
  test.beforeEach(async ({ page }) => {
    await login(page);
  });
  test.beforeEach(async ({ page }) => {
    await page.goto("/");
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
    // Should show Run Task submit button (not the card title)
    await expect(page.getByRole("button", { name: "Run Task" })).toBeVisible();
  });

  test("task detail shows flow steps for multi-step task", async ({ page }) => {
    // Navigate to data-pipeline task (multi-step: transform -> summarize)
    await page.goto("/workspaces/default/tasks/data-pipeline");
    await page.waitForLoadState("networkidle");
    await expect(page.locator("h1")).toContainText("data-pipeline");

    // Flow Steps section should be visible
    await expect(page.locator("main").getByText("Flow Steps").first()).toBeVisible();

    // Step names should be visible in the step list
    await expect(page.getByText("transform").first()).toBeVisible();
    await expect(page.getByText("summarize").first()).toBeVisible();

    // DAG canvas may or may not render in headless Docker (WebGL-dependent).
    // If it renders, it should have the .react-flow class.
    const dag = page.locator(".react-flow");
    const dagFallback = page.getByText("DAG visualization failed to render");
    await expect(dag.or(dagFallback)).toBeVisible();
  });

  test("run task creates job", async ({ page }) => {
    await page.click("text=Tasks");
    await page.locator("table tbody tr td a").first().click();
    await page.waitForLoadState("networkidle");

    // The "Run Task" form is inline on the task detail page (not a dialog).
    // Click the submit button.
    await page.getByRole("button", { name: "Run Task" }).click();

    // Should redirect to job detail
    await page.waitForURL(/\/jobs\/.+/);
    await expect(page.locator("h1")).toBeVisible();
  });
});
