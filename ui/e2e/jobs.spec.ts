import { test, expect } from "@playwright/test";
import { triggerJob, waitForJob, getAuthToken, apiFetch } from "./helpers";

test.describe("Jobs", () => {

  test("jobs list shows status badges", async ({ page, baseURL }) => {
    // Trigger a job so there's at least one
    await triggerJob(baseURL!, "hello-world", { name: "e2e-test" });

    await page.click("text=Jobs");
    await expect(page).toHaveURL("/jobs");
    await expect(page.getByRole("heading", { name: "Jobs" })).toBeVisible();

    // Wait for at least one job row to appear with a status text
    const firstRow = page.locator("table tbody tr").first();
    await expect(firstRow).toBeVisible();
    // Status cell (3rd column: Task, Workspace, Status)
    await expect(
      firstRow.locator("td").nth(2),
    ).toContainText(/(pending|running|completed|failed)/);
  });

  test("job detail shows steps and expandable step detail", async ({
    page,
    baseURL,
  }) => {
    const jobId = await triggerJob(baseURL!, "hello-world", {
      name: "e2e-steps-test",
    });

    // Wait for job to complete so steps have data
    await waitForJob(baseURL!, jobId);

    await page.goto(`/jobs/${jobId}`);
    // Should show the task name
    await expect(page.getByRole("heading", { name: "hello-world" })).toBeVisible();
    // Should show step list heading
    await expect(page.getByText("Steps", { exact: true })).toBeVisible();

    // Steps use div[role="button"], not native <button> elements
    const stepButton = page
      .locator('[role="button"]')
      .filter({ hasText: /say.hello/ })
      .first();
    await expect(stepButton).toBeVisible();
    await stepButton.click();
    // Should show Logs/Input/Output tabs
    await expect(page.getByRole("tab", { name: "Logs" })).toBeVisible();
    await expect(page.getByRole("tab", { name: "Input" })).toBeVisible();
    await expect(page.getByRole("tab", { name: "Output" })).toBeVisible();
  });

  test("job detail shows started time and duration for completed job", async ({
    page,
    baseURL,
  }) => {
    const jobId = await triggerJob(baseURL!, "hello-world", {
      name: "timing-test",
    });

    // Wait for job to complete
    await waitForJob(baseURL!, jobId);

    // Verify the API response has started_at set
    const token = await getAuthToken(baseURL!);
    const apiRes = await apiFetch(baseURL!, `/api/jobs/${jobId}`, token);
    const apiData = await apiRes.json();
    expect(apiData.started_at).not.toBeNull();
    expect(apiData.completed_at).not.toBeNull();

    await page.goto(`/jobs/${jobId}`);
    await expect(page.getByRole("heading", { name: "hello-world" })).toBeVisible();

    // InfoGrid renders each item as <div class="rounded-lg border ...">
    //   <p class="text-xs ...">Label</p>
    //   <div>value</div>
    // </div>
    // Locate the label <p> elements and check their sibling value is not "-"
    const startedLabel = page.locator("p").filter({ hasText: /^Started$/ });
    await expect(startedLabel).toBeVisible();
    // The parent div contains both the label and value — value should not be "-"
    const startedCard = startedLabel.locator("..");
    await expect(startedCard).not.toContainText(/^—$|^-$/);

    const durationLabel = page.locator("p").filter({ hasText: /^Duration$/ });
    await expect(durationLabel).toBeVisible();
    const durationCard = durationLabel.locator("..");
    await expect(durationCard).not.toContainText(/^—$|^-$/);
  });

  test("job detail shows job output for data-pipeline task", async ({
    page,
    baseURL,
  }) => {
    const jobId = await triggerJob(baseURL!, "data-pipeline", {
      data: "hello",
    });

    // Wait for job to complete
    await waitForJob(baseURL!, jobId);

    // Verify job completed successfully with output
    const token = await getAuthToken(baseURL!);
    const apiRes = await apiFetch(baseURL!, `/api/jobs/${jobId}`, token);
    const apiData = await apiRes.json();
    expect(apiData.status).toBe("completed");
    expect(apiData.output).not.toBeNull();

    await page.goto(`/jobs/${jobId}`);
    await page.waitForLoadState("networkidle");
    // "Job Output" is a CardTitle inside a Card
    await expect(page.getByText("Job Output").first()).toBeVisible();
    // The job output JSON should contain the processed result
    await expect(page.getByText("processed-hello done")).toBeVisible();
  });

  test("job detail hides job output for hello-world task", async ({
    page,
    baseURL,
  }) => {
    const jobId = await triggerJob(baseURL!, "hello-world", {
      name: "output-test",
    });

    // Wait for job to complete
    await waitForJob(baseURL!, jobId);

    await page.goto(`/jobs/${jobId}`);
    // Should NOT show "Job Output" (terminal step `shout-it` has no OUTPUT:)
    await expect(page.getByText("Job Output")).not.toBeVisible();
  });

  test("failing job shows failed status and error message", async ({
    page,
    baseURL,
  }) => {
    const jobId = await triggerJob(baseURL!, "always-failing");

    // Wait for job to complete (fail)
    await waitForJob(baseURL!, jobId);

    // Verify it failed via API
    const token = await getAuthToken(baseURL!);
    const apiRes = await apiFetch(baseURL!, `/api/jobs/${jobId}`, token);
    const apiData = await apiRes.json();
    expect(apiData.status).toBe("failed");

    await page.goto(`/jobs/${jobId}`);

    // Job status badge should show "failed"
    await expect(page.getByText("failed").first()).toBeVisible();

    // Steps use div[role="button"], not native <button> elements
    const stepButton = page
      .locator('[role="button"]')
      .filter({ hasText: /doom/ })
      .first();
    await expect(stepButton).toBeVisible();

    // Error message is in a <pre> inside a red container below the step row
    // error_message format: "Exit code: N\nStderr: ..."
    await expect(
      page.locator("pre").filter({ hasText: /Exit code|Stderr/ }),
    ).toBeVisible();

    // Expand step to see logs
    await stepButton.click();
    await expect(page.getByRole("tab", { name: "Logs" })).toBeVisible();
  });

  test("failed step in chain skips dependent step", async ({
    page,
    baseURL,
  }) => {
    const jobId = await triggerJob(baseURL!, "fail-in-chain", {
      name: "chain-test",
    });

    // Wait for job to complete (fail)
    await waitForJob(baseURL!, jobId);

    const token = await getAuthToken(baseURL!);
    const apiRes = await apiFetch(baseURL!, `/api/jobs/${jobId}`, token);
    const apiData = await apiRes.json();
    expect(apiData.status).toBe("failed");

    // Verify step statuses via API: step-fail=failed, step-after=skipped
    const stepFail = apiData.steps.find(
      (s: { step_name: string }) => s.step_name === "step-fail",
    );
    const stepAfter = apiData.steps.find(
      (s: { step_name: string }) => s.step_name === "step-after",
    );
    expect(stepFail.status).toBe("failed");
    expect(stepAfter.status).toBe("skipped");

    await page.goto(`/jobs/${jobId}`);

    // Both steps should be visible (use first() since step name may appear in multiple places)
    await expect(page.getByText("step-fail").first()).toBeVisible();
    await expect(page.getByText("step-after").first()).toBeVisible();

    // Error message from the failed step should be visible
    await expect(
      page.locator("pre").filter({ hasText: /Exit code|Stderr/ }),
    ).toBeVisible();
  });

  test("continue_on_failure allows step to run after failed dependency", async ({
    page,
    baseURL,
  }) => {
    const jobId = await triggerJob(baseURL!, "fail-continue", {
      name: "continue-test",
    });

    // Wait for job to complete (fail)
    await waitForJob(baseURL!, jobId);

    const token = await getAuthToken(baseURL!);
    const apiRes = await apiFetch(baseURL!, `/api/jobs/${jobId}`, token);
    const apiData = await apiRes.json();
    // Job should be failed (step-fail failed), but step-after should have run
    expect(apiData.status).toBe("failed");

    const stepFail = apiData.steps.find(
      (s: { step_name: string }) => s.step_name === "step-fail",
    );
    const stepAfter = apiData.steps.find(
      (s: { step_name: string }) => s.step_name === "step-after",
    );
    expect(stepFail.status).toBe("failed");
    expect(stepAfter.status).toBe("completed");

    await page.goto(`/jobs/${jobId}`);
    await expect(page.getByText("step-fail").first()).toBeVisible();
    await expect(page.getByText("step-after").first()).toBeVisible();
  });

  test("step logs show timestamps and stderr coloring", async ({
    page,
    baseURL,
  }) => {
    const jobId = await triggerJob(baseURL!, "hello-with-warning", {
      name: "stderr-test",
    });

    // Wait for job to complete
    await waitForJob(baseURL!, jobId);

    await page.goto(`/jobs/${jobId}`);

    // Steps use div[role="button"], not native <button> elements
    const stepButton = page
      .locator('[role="button"]')
      .filter({ hasText: /warn.hello/ })
      .first();
    await expect(stepButton).toBeVisible();
    await stepButton.click();

    // Wait for logs to load
    await expect(page.getByRole("tab", { name: "Logs" })).toBeVisible();

    // Timestamps are in a <span> with class "mr-3 shrink-0 select-none text-zinc-600"
    // Use attribute selector since Tailwind composes multiple classes
    await expect(
      page.locator("span[class*='text-zinc-600']").first(),
    ).toBeVisible();

    // Stderr lines have data-stream="stderr" and class "text-red-400"
    const stderrSpan = page.locator("[data-stream='stderr']").first();
    await expect(stderrSpan).toBeVisible();
    await expect(stderrSpan).toHaveClass(/text-red-400/);
  });

  test("job API response includes depends_on for multi-step task", async ({
    baseURL,
  }) => {
    const jobId = await triggerJob(baseURL!, "data-pipeline", {
      data: "dep-test",
    });

    // Wait for job to complete
    await waitForJob(baseURL!, jobId);

    const token = await getAuthToken(baseURL!);
    const apiRes = await apiFetch(baseURL!, `/api/jobs/${jobId}`, token);
    const apiData = await apiRes.json();

    // The transform step should have empty depends_on
    const transform = apiData.steps.find(
      (s: { step_name: string }) => s.step_name === "transform",
    );
    expect(transform.depends_on).toEqual([]);

    // The summarize step should depend on transform
    const summarize = apiData.steps.find(
      (s: { step_name: string }) => s.step_name === "summarize",
    );
    expect(summarize.depends_on).toEqual(["transform"]);
  });

  test("job detail shows graph toggle for multi-step job", async ({
    page,
    baseURL,
  }) => {
    const jobId = await triggerJob(baseURL!, "data-pipeline", {
      data: "dag-test",
    });

    // Wait for job to complete
    await waitForJob(baseURL!, jobId);

    await page.goto(`/jobs/${jobId}`);
    await expect(
      page.getByRole("heading", { name: "data-pipeline" }),
    ).toBeVisible();

    // There is a single "Graph" toggle button (with Network icon + chevron).
    // The graph is open by default (graphOpen starts true).
    const graphBtn = page.getByRole("button", { name: /Graph/ });
    await expect(graphBtn).toBeVisible();

    // React Flow canvas should already be visible (graph open by default)
    await expect(page.locator(".react-flow")).toBeVisible();

    // Clicking the button collapses the graph
    await graphBtn.click();
    await expect(page.locator(".react-flow")).not.toBeVisible();

    // Clicking again re-opens it
    await graphBtn.click();
    await expect(page.locator(".react-flow")).toBeVisible();
  });
});
