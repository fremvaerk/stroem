import { test, expect } from "@playwright/test";
import { login } from "./helpers";

test.describe("Tasks", () => {
  // login() already lands on "/". Do not reload it here: the reload's session
  // restore (POST /api/auth/refresh) is still in flight when a test navigates
  // on, the navigation aborts it after the server has rotated the refresh
  // token, and the next page's restore then presents the consumed token (401)
  // and lands on /login.
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
    // Should show Run Task submit button (not the card title)
    await expect(page.getByRole("button", { name: "Run Task" })).toBeVisible();
  });

  test("task detail shows flow steps for multi-step task", async ({ page }) => {
    // Navigate to data-pipeline task (multi-step: transform -> summarize)
    await page.goto("/workspaces/default/tasks/data-pipeline");
    await page.waitForLoadState("networkidle");
    await expect(page.locator("main h1")).toContainText("data-pipeline");

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

  test("multi-select input submits an array (verified via network)", async ({
    page,
  }) => {
    await page.goto("/workspaces/default/tasks/multi-select-demo");
    await page.waitForLoadState("networkidle");

    // Open the multi-select popover for `environments` (Label "environments *")
    const trigger = page.getByLabel(/environments/i);
    await expect(trigger).toBeVisible();
    await trigger.click();

    await page.getByRole("option", { name: "dev" }).click();
    await page.getByRole("option", { name: "staging" }).click();

    // Popover stays open across selections
    await expect(
      page.getByPlaceholder("Search or type a value..."),
    ).toBeVisible();

    // Close popover and verify both chips are visible in the trigger
    await page.keyboard.press("Escape");
    await expect(trigger).toContainText("dev");
    await expect(trigger).toContainText("staging");

    // Intercept the execute request and verify input.environments is an actual array
    const executePromise = page.waitForRequest(
      (req) =>
        req.url().includes("/api/workspaces/") &&
        req.url().includes("/tasks/") &&
        req.url().includes("/execute") &&
        req.method() === "POST",
    );
    await page.getByRole("button", { name: "Run Task" }).click();
    const executeReq = await executePromise;
    const body = executeReq.postDataJSON() as {
      input?: { environments?: unknown };
    };
    expect(Array.isArray(body.input?.environments)).toBe(true);
    expect(body.input?.environments).toEqual(["dev", "staging"]);

    await page.waitForURL(/\/jobs\/.+/);

    // Job detail's Job Input card should contain the array values
    const inputCard = page.getByTestId("job-input");
    await expect(inputCard).toContainText(/dev/);
    await expect(inputCard).toContainText(/staging/);
  });

  test("multi-select: deselect-all returns trigger to placeholder", async ({
    page,
  }) => {
    await page.goto("/workspaces/default/tasks/multi-select-demo");
    await page.waitForLoadState("networkidle");

    const trigger = page.getByLabel(/environments/i);
    await trigger.click();
    await page.getByRole("option", { name: "dev" }).click();
    await page.getByRole("option", { name: "staging" }).click();
    await expect(
      page.getByPlaceholder("Search or type a value..."),
    ).toBeVisible();

    // Deselect both
    await page.getByRole("option", { name: "dev" }).click();
    await page.getByRole("option", { name: "staging" }).click();
    await page.keyboard.press("Escape");

    // Placeholder falls back to "Select <label>" (fb26a0d: descriptions are helper text only)
    await expect(trigger).toContainText(/select environments/i);
  });

  test("multi-select: allow_custom lets users add a value outside options", async ({
    page,
  }) => {
    await page.goto("/workspaces/default/tasks/multi-select-demo");
    await page.waitForLoadState("networkidle");

    // The `tags` field has allow_custom: true
    const tagsTrigger = page.getByLabel(/tags/i);
    await tagsTrigger.click();
    await page.getByPlaceholder("Search or type a value...").fill("hotfix");
    // Component renders curly quotes (&ldquo;/&rdquo;); tolerate either curly or straight.
    await page
      .getByRole("option", { name: /Add ["“]hotfix["”]/ })
      .click();
    await page.keyboard.press("Escape");
    await expect(tagsTrigger).toContainText("hotfix");

    // Submit and verify the network body has the custom tag in the array
    const executePromise = page.waitForRequest(
      (req) =>
        req.url().includes("/execute") && req.method() === "POST",
    );
    // environments is required — pick something so the form is valid
    const envTrigger = page.getByLabel(/environments/i);
    await envTrigger.click();
    await page.getByRole("option", { name: "dev" }).click();
    await page.keyboard.press("Escape");

    await page.getByRole("button", { name: "Run Task" }).click();
    const executeReq = await executePromise;
    const body = executeReq.postDataJSON() as {
      input?: { tags?: unknown };
    };
    expect(Array.isArray(body.input?.tags)).toBe(true);
    expect(body.input?.tags).toContain("hotfix");
  });

  test("json input submits a parsed object and re-runs it", async ({ page }) => {
    await page.goto("/workspaces/default/tasks/json-demo");
    await page.waitForLoadState("networkidle");

    const editor = page.getByLabel("payload");
    await expect(editor).toHaveValue(/from-task-default/);
    await editor.fill('{"key": "from-ui"}');

    const isExecute = (req: import("@playwright/test").Request) =>
      req.url().includes("/tasks/json-demo/execute") && req.method() === "POST";
    const first = page.waitForRequest(isExecute);
    await page.getByRole("button", { name: "Run Task" }).click();
    const body = (await first).postDataJSON() as { input?: { payload?: unknown } };
    expect(body.input?.payload).toEqual({ key: "from-ui" });

    await page.waitForURL(/\/jobs\/.+/);
    await page.getByRole("link", { name: "Re-run" }).click();
    await page.waitForURL(/\/tasks\/json-demo/);
    await expect(page.getByLabel("payload")).toHaveValue(/from-ui/);

    const second = page.waitForRequest(isExecute);
    await page.getByRole("button", { name: "Run Task" }).click();
    const rerun = (await second).postDataJSON() as {
      input?: { payload?: unknown };
      source_job_id?: string;
    };
    expect(rerun.input?.payload).toEqual({ key: "from-ui" });
    expect(rerun.source_job_id).toBeTruthy();
  });
});
