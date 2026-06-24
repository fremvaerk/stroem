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

    // Both Open ↗ and Download render as <button> elements (changed from
    // <a> when downloads moved to programmatic Blob-URL fetch — direct
    // browser nav can't attach the SPA's in-memory JWT, so all downloads
    // now go through `apiFetchRaw` + `URL.createObjectURL`).
    // text/plain (status.txt) → "Open ↗"; text/html (report.html) → "Download".
    await expect(
      page.getByRole("button", { name: /Download/ }).first(),
    ).toBeVisible();
    await expect(
      page.getByRole("button", { name: /Open/ }).first(),
    ).toBeVisible();
  });

  test("download button fetches with auth + triggers blob download", async ({
    page,
    baseURL,
  }) => {
    let jobId: string;
    try {
      jobId = await triggerJob(baseURL!, "produce-artifacts", {});
    } catch (err) {
      test.skip(
        true,
        `produce-artifacts task not available: ${err instanceof Error ? err.message : String(err)}`,
      );
      return;
    }
    await waitForJob(baseURL!, jobId);

    // Spy the authenticated artifact download request — must carry the
    // JWT in `Authorization: Bearer …`. Direct `<a href>` nav previously
    // omitted the header, hitting 401 in production.
    let sawAuthHeader = false;
    page.on("request", (req) => {
      if (req.url().includes("/api/jobs/") && req.url().includes("/artifacts/")) {
        const hdr = req.headers()["authorization"] ?? "";
        if (hdr.startsWith("Bearer ")) sawAuthHeader = true;
      }
    });

    await page.goto(`/jobs/${jobId}`);
    await expect(
      page.getByRole("heading", { name: "Artifacts" }),
    ).toBeVisible();

    // Trigger the download (Playwright catches the file save dialog via
    // page.waitForEvent("download")).
    const [download] = await Promise.all([
      page.waitForEvent("download"),
      page.getByRole("button", { name: /Download/ }).first().click(),
    ]);

    expect(sawAuthHeader).toBe(true);
    expect(download.suggestedFilename()).toBeTruthy();
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

  test("artifacts section surfaces fetch error row when API returns 500", async ({
    page,
    baseURL,
  }) => {
    // Mock the artifacts list endpoint to return 500 BEFORE navigating so the
    // failure is captured on the first (and any retry) fetch.
    await page.route("**/api/jobs/*/artifacts", (route) => {
      route.fulfill({
        status: 500,
        contentType: "application/json",
        body: JSON.stringify({ error: "internal" }),
      });
    });

    // Trigger any job — we just need a real job_id to load the detail page.
    const jobId = await triggerJob(baseURL!, "hello-world", {
      name: "fetch-error-test",
    });
    await waitForJob(baseURL!, jobId);

    // Track uncaught console errors so we can assert none of them are caused
    // by the fetch failure (the .catch handler should swallow it cleanly).
    const consoleErrors: string[] = [];
    page.on("pageerror", (err) => consoleErrors.push(err.message));

    await page.goto(`/jobs/${jobId}`);

    // The section header MUST be rendered so the user can see something went
    // wrong instead of the silent empty-section case.
    await expect(
      page.getByRole("heading", { name: "Artifacts" }),
    ).toBeVisible();

    // The muted error row carries data-testid="artifacts-fetch-error".
    await expect(
      page.getByTestId("artifacts-fetch-error"),
    ).toBeVisible();

    // No uncaught page errors from the fetch failure.
    expect(consoleErrors).toEqual([]);

    await page.unroute("**/api/jobs/*/artifacts");
  });
});
