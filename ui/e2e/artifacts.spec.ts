import { test, expect } from "@playwright/test";
import { login, triggerJob, waitForJob } from "./helpers";

test.describe("Artifacts", () => {
  test.beforeEach(async ({ page }) => {
    await login(page);
  });

  test("artifacts section appears with uploaded files", async ({
    page,
    baseURL,
  }) => {
    // `produce-artifacts` is defined in tests/e2e-workspace/ (see Task B13).
    // If the task isn't present (e.g. running this spec against a stripped-down
    // playground), skip rather than fail — the section is null-rendered when
    // there are no artifacts, which is correct behaviour we test separately.
    let jobId: string;
    try {
      jobId = await triggerJob(baseURL!, "produce-artifacts", {});
    } catch (err) {
      test.skip(
        true,
        `produce-artifacts task not available in test workspace: ${err instanceof Error ? err.message : String(err)}`,
      );
      return;
    }

    await waitForJob(baseURL!, jobId);

    await page.goto(`/jobs/${jobId}`);

    // The section is labelled by an h2 with id="artifacts-heading".
    await expect(
      page.getByRole("heading", { name: "Artifacts" }),
    ).toBeVisible();

    // produce-artifacts writes report.html (inline-safe? no — HTML is download)
    // and status.txt (text/plain → inline). At least one of each disposition
    // should be present.
    await expect(page.getByText("report.html")).toBeVisible();
    await expect(page.getByText("status.txt")).toBeVisible();

    // Both Open ↗ and Download buttons render as anchor tags via Button asChild.
    // text/plain (status.txt) → "Open ↗"; text/html (report.html) → "Download".
    await expect(
      page.getByRole("link", { name: /Download/ }).first(),
    ).toBeVisible();
    await expect(
      page.getByRole("link", { name: /Open/ }).first(),
    ).toBeVisible();
  });

  test("artifacts section is hidden when no artifacts produced", async ({
    page,
    baseURL,
  }) => {
    // hello-world doesn't touch ARTIFACTS_DIR → ArtifactList renders null.
    const jobId = await triggerJob(baseURL!, "hello-world", {
      name: "no-artifacts",
    });
    await waitForJob(baseURL!, jobId);

    await page.goto(`/jobs/${jobId}`);
    await expect(
      page.getByRole("heading", { name: "hello-world" }),
    ).toBeVisible();
    await expect(
      page.getByRole("heading", { name: "Artifacts" }),
    ).toHaveCount(0);
  });
});
