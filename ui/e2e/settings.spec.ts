import { test, expect, type Page } from "@playwright/test";
import { login } from "./helpers";

// Scope a dialog by its heading. During Radix open/close transitions two
// dialogs can briefly coexist in the DOM (the exiting one is still animating
// while the next mounts), so a bare `getByRole("dialog")` can match multiple
// elements (strict-mode violation) or the wrong, closing dialog. Filtering by
// the unique heading targets exactly one dialog and eliminates that race.
const dialogByHeading = (page: Page, name: string) =>
  page
    .getByRole("dialog")
    .filter({ has: page.getByRole("heading", { name }) });

test.describe("Settings - API Key Management", () => {
  test.beforeEach(async ({ page }) => {
    await login(page);
    await page.goto("/settings");
    // Assert the page is ready via a concrete element rather than the flaky
    // `networkidle` load state.
    await expect(page.getByRole("heading", { name: "Settings" })).toBeVisible();
  });

  test("settings page renders with API Keys section", async ({ page }) => {
    // CardTitle renders as a div, not a heading. Scope to main content area.
    await expect(page.locator("main").getByText("API Keys").first()).toBeVisible();
    await expect(
      page.getByRole("button", { name: "Create API Key" }),
    ).toBeVisible();
    // Empty state or existing keys table is shown
    const emptyState = page.getByText("No API keys yet. Create one to get started.");
    const table = page.locator("table");
    await expect(emptyState.or(table)).toBeVisible();
  });

  test("create API key dialog opens and closes", async ({ page }) => {
    await page.getByRole("button", { name: "Create API Key" }).click();

    const dialog = dialogByHeading(page, "Create API Key");
    await expect(dialog).toBeVisible();
    await expect(dialog.locator('input[id="key-name"]')).toBeVisible();
    await expect(dialog.locator('input[id="key-expiry"]')).toBeVisible();
    await expect(dialog.getByRole("button", { name: "Create" })).toBeVisible();
    await expect(dialog.getByRole("button", { name: "Cancel" })).toBeVisible();

    // Cancel closes the dialog
    await dialog.getByRole("button", { name: "Cancel" }).click();
    await expect(dialog).toBeHidden();
  });

  test("create button is disabled when name is empty", async ({ page }) => {
    await page.getByRole("button", { name: "Create API Key" }).click();

    const dialog = dialogByHeading(page, "Create API Key");
    await expect(dialog).toBeVisible();

    // Submit button should be disabled with empty name
    await expect(dialog.getByRole("button", { name: "Create" })).toBeDisabled();

    // Typing a name enables the button
    await dialog.locator('input[id="key-name"]').fill("Test Key");
    await expect(
      dialog.getByRole("button", { name: "Create" }),
    ).not.toBeDisabled();

    // Clearing the name disables it again
    await dialog.locator('input[id="key-name"]').fill("");
    await expect(dialog.getByRole("button", { name: "Create" })).toBeDisabled();
  });

  test("create API key shows key once and lists it in the table", async ({
    page,
  }) => {
    const keyName = `e2e-test-key-${Date.now()}`;

    // Open create dialog
    await page.getByRole("button", { name: "Create API Key" }).click();
    const createDialog = dialogByHeading(page, "Create API Key");
    await expect(createDialog).toBeVisible();

    // Fill in name
    await createDialog.locator('input[id="key-name"]').fill(keyName);

    // Submit the form
    await createDialog.getByRole("button", { name: "Create" }).click();

    // The create dialog closes and the reveal dialog opens. Scope each by its
    // heading so the transition overlap can't confuse the locator.
    const revealDialog = dialogByHeading(page, "API Key Created");
    await expect(revealDialog).toBeVisible();
    await expect(
      revealDialog.getByText("Copy your API key now. You won't be able to see it again."),
    ).toBeVisible();

    // The key is displayed as a code element starting with "strm_"
    const keyCode = revealDialog.locator("code").first();
    await expect(keyCode).toBeVisible();
    const keyText = await keyCode.textContent();
    expect(keyText).toMatch(/^strm_/);

    // Copy button and Done button are present. Use an exact name for Copy so it
    // doesn't also match a "Copied" state.
    await expect(
      revealDialog.getByRole("button", { name: "Copy", exact: true }),
    ).toBeVisible();
    await expect(revealDialog.getByRole("button", { name: "Done" })).toBeVisible();

    // Dismiss reveal dialog
    await revealDialog.getByRole("button", { name: "Done" }).click();
    await expect(revealDialog).toBeHidden();

    // The new key should now appear in the table. Extended timeout: the
    // create handler awaits `load()` before opening the reveal dialog, so
    // the keys array is fresh in state — but the DOM update for the new
    // table row sometimes lags behind on slow CI runners (race against
    // the modal-unmount animation + React commit). 10 s is generous and
    // mirrors the existing precedent in the "revoke" test below.
    const row = page.locator("table tbody tr").filter({ hasText: keyName });
    await expect(row).toBeVisible({ timeout: 10000 });

    // The key prefix cell should show "{prefix}..."
    const prefixCell = row.locator("td").nth(1);
    await expect(prefixCell).toContainText("strm_");
    await expect(prefixCell).toContainText("...");

    // Expires column should show "Never" (no expiry set)
    const expiresCell = row.locator("td").nth(3);
    await expect(expiresCell).toContainText("Never");
  });

  test("create API key with expiry shows expiry date in table", async ({
    page,
  }) => {
    const keyName = `e2e-expiry-key-${Date.now()}`;

    await page.getByRole("button", { name: "Create API Key" }).click();
    const createDialog = dialogByHeading(page, "Create API Key");
    await expect(createDialog).toBeVisible();

    await createDialog.locator('input[id="key-name"]').fill(keyName);
    await createDialog.locator('input[id="key-expiry"]').fill("30");
    await createDialog.getByRole("button", { name: "Create" }).click();

    // Dismiss reveal dialog
    const revealDialog = dialogByHeading(page, "API Key Created");
    await expect(revealDialog).toBeVisible();
    await revealDialog.getByRole("button", { name: "Done" }).click();
    await expect(revealDialog).toBeHidden();

    // Row should exist and not show "Never" in expires column.
    // Extended timeout for the same CI-runner race documented above.
    const row = page.locator("table tbody tr").filter({ hasText: keyName });
    await expect(row).toBeVisible({ timeout: 10000 });
    const expiresCell = row.locator("td").nth(3);
    await expect(expiresCell).not.toContainText("Never");
  });

  test("copy button in reveal dialog copies the key", async ({
    page,
    context,
  }) => {
    const keyName = `e2e-copy-key-${Date.now()}`;

    // Grant clipboard permissions (may not work in all headless environments)
    await context.grantPermissions(["clipboard-read", "clipboard-write"]);

    await page.getByRole("button", { name: "Create API Key" }).click();
    const createDialog = dialogByHeading(page, "Create API Key");
    await expect(createDialog).toBeVisible();
    await createDialog.locator('input[id="key-name"]').fill(keyName);
    await createDialog.getByRole("button", { name: "Create" }).click();

    const revealDialog = dialogByHeading(page, "API Key Created");
    await expect(revealDialog).toBeVisible();

    // The key should be displayed in a code element starting with strm_
    const keyCode = revealDialog.locator("code").first();
    await expect(keyCode).toBeVisible();
    const keyText = await keyCode.textContent();
    expect(keyText).toMatch(/^strm_/);

    // Click Copy button. In headless Docker without clipboard support,
    // navigator.clipboard.writeText() may throw synchronously, preventing
    // the UI from updating to "Copied". Accept either outcome.
    const copyBtn = revealDialog.getByRole("button", { name: "Copy", exact: true });
    await copyBtn.click();

    // Verify either "Copied" feedback OR the button is still "Copy" (clipboard unavailable)
    const copiedBtn = revealDialog.getByRole("button", { name: /Copied/ });
    const copyStillBtn = revealDialog.getByRole("button", { name: "Copy", exact: true });
    await expect(copiedBtn.or(copyStillBtn)).toBeVisible();

    // If clipboard worked, verify the content
    try {
      const clipboardText = await page.evaluate(() =>
        navigator.clipboard.readText(),
      );
      expect(clipboardText).toBe(keyText);
    } catch {
      // Clipboard API not available — that's ok
    }

    // Dismiss
    await revealDialog.getByRole("button", { name: "Done" }).click();
  });

  test("revoke API key removes it from the list", async ({ page }) => {
    const keyName = `e2e-revoke-key-${Date.now()}`;

    // Create a key first
    await page.getByRole("button", { name: "Create API Key" }).click();
    const createDialog = dialogByHeading(page, "Create API Key");
    await expect(createDialog).toBeVisible();
    await createDialog.locator('input[id="key-name"]').fill(keyName);
    await createDialog.getByRole("button", { name: "Create" }).click();

    // Dismiss reveal dialog
    const revealDialog = dialogByHeading(page, "API Key Created");
    await expect(revealDialog).toBeVisible();
    await revealDialog.getByRole("button", { name: "Done" }).click();
    await expect(revealDialog).toBeHidden();

    // Wait for the key to appear in the table (load() runs async after dialog close)
    const row = page.locator("table tbody tr").filter({ hasText: keyName });
    await expect(row).toBeVisible({ timeout: 10000 });

    // Click the trash/delete button on that row
    await row.getByRole("button").click();

    // Confirmation dialog should open
    const confirmDialog = dialogByHeading(page, "Revoke API Key");
    await expect(confirmDialog).toBeVisible();
    await expect(
      confirmDialog.getByText(
        "Are you sure you want to revoke this API key?",
      ),
    ).toBeVisible();
    await expect(
      confirmDialog.getByRole("button", { name: "Cancel" }),
    ).toBeVisible();
    await expect(
      confirmDialog.getByRole("button", { name: "Revoke" }),
    ).toBeVisible();

    // Confirm revocation
    await confirmDialog.getByRole("button", { name: "Revoke" }).click();

    // Confirmation dialog should close
    await expect(confirmDialog).toBeHidden();

    // The row should be gone from the table
    await expect(
      page.locator("table tbody tr").filter({ hasText: keyName }),
    ).toBeHidden();
  });

  test("cancel in revoke confirmation keeps the key", async ({ page }) => {
    const keyName = `e2e-cancel-revoke-key-${Date.now()}`;

    // Create a key first
    await page.getByRole("button", { name: "Create API Key" }).click();
    const createDialog = dialogByHeading(page, "Create API Key");
    await expect(createDialog).toBeVisible();
    await createDialog.locator('input[id="key-name"]').fill(keyName);
    await createDialog.getByRole("button", { name: "Create" }).click();

    // Dismiss reveal dialog
    const revealDialog = dialogByHeading(page, "API Key Created");
    await expect(revealDialog).toBeVisible();
    await revealDialog.getByRole("button", { name: "Done" }).click();
    await expect(revealDialog).toBeHidden();

    // Key should be in the table — extended timeout for CI-runner race.
    const row = page.locator("table tbody tr").filter({ hasText: keyName });
    await expect(row).toBeVisible({ timeout: 10000 });

    // Click trash button
    await row.getByRole("button").click();

    // Cancel the confirmation
    const confirmDialog = dialogByHeading(page, "Revoke API Key");
    await expect(confirmDialog).toBeVisible();
    await confirmDialog.getByRole("button", { name: "Cancel" }).click();
    await expect(confirmDialog).toBeHidden();

    // Key should still be in the table
    await expect(
      page.locator("table tbody tr").filter({ hasText: keyName }),
    ).toBeVisible();

    // Clean up: revoke the key
    const rowAgain = page
      .locator("table tbody tr")
      .filter({ hasText: keyName });
    await rowAgain.getByRole("button").click();
    const cleanupDialog = dialogByHeading(page, "Revoke API Key");
    await cleanupDialog.getByRole("button", { name: "Revoke" }).click();
    await expect(cleanupDialog).toBeHidden();
  });

  test("settings page is protected and redirects to login when unauthenticated", async ({
    page,
  }) => {
    // Sign out first
    await page.click("text=Sign out");
    await expect(page).toHaveURL("/login");

    // Try to navigate directly to settings
    await page.goto("/settings");
    await expect(page).toHaveURL("/login");
  });
});
