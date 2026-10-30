import * as fs from "node:fs";
import { test, expect } from "@playwright/test";
import { login } from "./helpers";

const CONTENT = "col_a,col_b\n1,2\n";

test.describe("File inputs", () => {
  test.beforeEach(async ({ page }) => {
    await login(page);
  });

  test("upload, run, download and re-run with the same file", async ({ page }) => {
    await page.goto("/workspaces/default/tasks/file-demo");

    // Pick a file: the form uploads at once.
    await page.setInputFiles("#input-data_file", {
      name: "report.csv",
      mimeType: "text/csv",
      buffer: Buffer.from(CONTENT),
    });
    // "Replace" shows only once the upload is ready ("Remove" also shows after a failure).
    await expect(page.getByRole("button", { name: "Replace" })).toBeVisible();
    await expect(page.getByText("report.csv")).toBeVisible();

    await page.getByRole("button", { name: "Run Task" }).click();
    await page.waitForURL(/\/jobs\/.+/);
    const sourceJobUrl = page.url();

    // Job page: "Input files" card with the original name.
    await expect(page.getByRole("heading", { name: "Input files" })).toBeVisible();
    await expect(page.getByText("report.csv")).toBeVisible();

    // Download returns the file bytes (the UI fetches with the Bearer token, then clicks an <a download>).
    const [download] = await Promise.all([
      page.waitForEvent("download"),
      page.getByRole("button", { name: "Download report.csv" }).click(),
    ]);
    expect(download.suggestedFilename()).toBe("report.csv");
    expect(fs.readFileSync(await download.path(), "utf8")).toBe(CONTENT);

    // Re-run: the field shows the source file; running it again works.
    await page.getByRole("link", { name: "Re-run" }).click();
    await page.waitForURL(/\/workspaces\/default\/tasks\/file-demo/);
    await expect(page.getByText("report.csv")).toBeVisible();
    await expect(page.getByText("From the previous run")).toBeVisible();
    await page.getByRole("button", { name: "Run Task" }).click();
    await page.waitForURL(/\/jobs\/.+/);
    expect(page.url()).not.toBe(sourceJobUrl);

    await expect(page.getByRole("heading", { name: "Input files" })).toBeVisible();
    await expect(page.getByText("completed", { exact: true }).first()).toBeVisible({ timeout: 60_000 });
  });
});
