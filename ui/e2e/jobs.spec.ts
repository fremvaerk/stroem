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
    await expect(page.getByRole("heading", { name: "Jobs" })).toBeVisible();

    // Wait for at least one job row to appear with a status text
    const firstRow = page.locator("table tbody tr").first();
    await expect(firstRow).toBeVisible();
    // Status cell should contain a recognized status
    await expect(
      firstRow.locator("td").nth(1),
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
    await expect(async () => {
      const res = await fetch(`${baseURL}/api/jobs/${jobId}`);
      const data = await res.json();
      expect(["completed", "failed"]).toContain(data.status);
    }).toPass({ timeout: 30000 });

    await page.goto(`/jobs/${jobId}`);
    // Should show the task name
    await expect(page.getByRole("heading", { name: "hello-world" })).toBeVisible();
    // Should show step timeline
    await expect(page.getByText("Steps", { exact: true })).toBeVisible();

    // Click a step to expand it
    const stepButton = page
      .locator("button")
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
    await expect(async () => {
      const res = await fetch(`${baseURL}/api/jobs/${jobId}`);
      const data = await res.json();
      expect(["completed", "failed"]).toContain(data.status);
    }).toPass({ timeout: 30000 });

    // Verify the API response has started_at set
    const apiRes = await fetch(`${baseURL}/api/jobs/${jobId}`);
    const apiData = await apiRes.json();
    expect(apiData.started_at).not.toBeNull();
    expect(apiData.completed_at).not.toBeNull();

    await page.goto(`/jobs/${jobId}`);
    await expect(page.getByRole("heading", { name: "hello-world" })).toBeVisible();

    // "Started" card should show a real timestamp, not "-"
    const startedCard = page.locator("div").filter({ hasText: /^Started/ });
    await expect(startedCard).toBeVisible();
    await expect(startedCard).not.toContainText("-");

    // "Duration" card should show a real value, not "-"
    const durationCard = page.locator("div").filter({ hasText: /^Duration/ });
    await expect(durationCard).toBeVisible();
    await expect(durationCard).not.toContainText("-");
  });

  test("job detail shows job output for data-pipeline task", async ({
    page,
    baseURL,
  }) => {
    const jobId = await triggerJob(baseURL!, "data-pipeline", {
      data: "hello",
    });

    // Wait for job to complete
    await expect(async () => {
      const res = await fetch(`${baseURL}/api/jobs/${jobId}`);
      const data = await res.json();
      expect(["completed", "failed"]).toContain(data.status);
    }).toPass({ timeout: 30000 });

    // Verify job completed successfully with output
    const apiRes = await fetch(`${baseURL}/api/jobs/${jobId}`);
    const apiData = await apiRes.json();
    expect(apiData.status).toBe("completed");
    expect(apiData.output).not.toBeNull();

    await page.goto(`/jobs/${jobId}`);
    // Should show "Job Output" card with aggregated output
    await expect(page.getByText("Job Output")).toBeVisible();
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
    await expect(async () => {
      const res = await fetch(`${baseURL}/api/jobs/${jobId}`);
      const data = await res.json();
      expect(["completed", "failed"]).toContain(data.status);
    }).toPass({ timeout: 30000 });

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
    await expect(async () => {
      const res = await fetch(`${baseURL}/api/jobs/${jobId}`);
      const data = await res.json();
      expect(["completed", "failed"]).toContain(data.status);
    }).toPass({ timeout: 30000 });

    // Verify it failed via API
    const apiRes = await fetch(`${baseURL}/api/jobs/${jobId}`);
    const apiData = await apiRes.json();
    expect(apiData.status).toBe("failed");

    await page.goto(`/jobs/${jobId}`);

    // Job status badge should show "failed"
    await expect(page.getByText("failed").first()).toBeVisible();

    // Step should have error indicator (XCircle icon)
    const stepButton = page
      .locator("button")
      .filter({ hasText: /doom/ })
      .first();
    await expect(stepButton).toBeVisible();

    // Error message should be visible (AlertCircle + pre)
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
    await expect(async () => {
      const res = await fetch(`${baseURL}/api/jobs/${jobId}`);
      const data = await res.json();
      expect(["completed", "failed"]).toContain(data.status);
    }).toPass({ timeout: 30000 });

    const apiRes = await fetch(`${baseURL}/api/jobs/${jobId}`);
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

    // Both steps should be visible
    await expect(page.getByText("step-fail")).toBeVisible();
    await expect(page.getByText("step-after")).toBeVisible();

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
    await expect(async () => {
      const res = await fetch(`${baseURL}/api/jobs/${jobId}`);
      const data = await res.json();
      expect(["completed", "failed"]).toContain(data.status);
    }).toPass({ timeout: 30000 });

    const apiRes = await fetch(`${baseURL}/api/jobs/${jobId}`);
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
    await expect(page.getByText("step-fail")).toBeVisible();
    await expect(page.getByText("step-after")).toBeVisible();
  });

  test("step logs show timestamps and stderr coloring", async ({
    page,
    baseURL,
  }) => {
    const jobId = await triggerJob(baseURL!, "hello-with-warning", {
      name: "stderr-test",
    });

    // Wait for job to complete
    await expect(async () => {
      const res = await fetch(`${baseURL}/api/jobs/${jobId}`);
      const data = await res.json();
      expect(["completed", "failed"]).toContain(data.status);
    }).toPass({ timeout: 30000 });

    await page.goto(`/jobs/${jobId}`);

    // Expand the step
    const stepButton = page
      .locator("button")
      .filter({ hasText: /warn.hello/ })
      .first();
    await expect(stepButton).toBeVisible();
    await stepButton.click();

    // Wait for logs to load
    await expect(page.getByRole("tab", { name: "Logs" })).toBeVisible();

    // Timestamps should be visible (HH:MM:SS.mmm pattern)
    await expect(
      page.locator("span.text-zinc-600").first(),
    ).toBeVisible();

    // Stderr lines should have red coloring
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
    await expect(async () => {
      const res = await fetch(`${baseURL}/api/jobs/${jobId}`);
      const data = await res.json();
      expect(["completed", "failed"]).toContain(data.status);
    }).toPass({ timeout: 30000 });

    const apiRes = await fetch(`${baseURL}/api/jobs/${jobId}`);
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

  test("job detail shows timeline/graph toggle for multi-step job", async ({
    page,
    baseURL,
  }) => {
    const jobId = await triggerJob(baseURL!, "data-pipeline", {
      data: "dag-test",
    });

    // Wait for job to complete
    await expect(async () => {
      const res = await fetch(`${baseURL}/api/jobs/${jobId}`);
      const data = await res.json();
      expect(["completed", "failed"]).toContain(data.status);
    }).toPass({ timeout: 30000 });

    await page.goto(`/jobs/${jobId}`);
    await expect(
      page.getByRole("heading", { name: "data-pipeline" }),
    ).toBeVisible();

    // Toggle buttons should be visible for multi-step job
    const timelineBtn = page.getByRole("button", { name: "Timeline" });
    const graphBtn = page.getByRole("button", { name: "Graph" });
    await expect(timelineBtn).toBeVisible();
    await expect(graphBtn).toBeVisible();

    // Click Graph to switch to DAG view
    await graphBtn.click();

    // React Flow canvas should be present
    await expect(page.locator(".react-flow")).toBeVisible();
  });
});
