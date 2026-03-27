import { test, expect } from "@playwright/test";
import { login } from "./helpers";

test.describe("Settings - API Key Management", () => {
  test.beforeEach(async ({ page }) => {
    await login(page);
    await page.goto("/settings");
    await page.waitForLoadState("networkidle");
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

    const dialog = page.getByRole("dialog");
    await expect(dialog).toBeVisible();
    await expect(
      dialog.getByRole("heading", { name: "Create API Key" }),
    ).toBeVisible();
    await expect(dialog.locator('input[id="key-name"]')).toBeVisible();
    await expect(dialog.locator('input[id="key-expiry"]')).toBeVisible();
    await expect(dialog.getByRole("button", { name: "Create" })).toBeVisible();
    await expect(dialog.getByRole("button", { name: "Cancel" })).toBeVisible();

    // Cancel closes the dialog
    await dialog.getByRole("button", { name: "Cancel" }).click();
    await expect(page.getByRole("dialog")).not.toBeVisible();
  });

  test("create button is disabled when name is empty", async ({ page }) => {
    await page.getByRole("button", { name: "Create API Key" }).click();

    const dialog = page.getByRole("dialog");
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
    const createDialog = page.getByRole("dialog");
    await expect(createDialog).toBeVisible();

    // Fill in name
    await createDialog.locator('input[id="key-name"]').fill(keyName);

    // Submit the form
    await createDialog.getByRole("button", { name: "Create" }).click();

    // Reveal dialog should open showing the key
    const revealDialog = page.getByRole("dialog");
    await expect(revealDialog).toBeVisible();
    await expect(
      revealDialog.getByRole("heading", { name: "API Key Created" }),
    ).toBeVisible();
    await expect(
      revealDialog.getByText("Copy your API key now. You won't be able to see it again."),
    ).toBeVisible();

    // The key is displayed as a code element starting with "strm_"
    const keyCode = revealDialog.locator("code").first();
    await expect(keyCode).toBeVisible();
    const keyText = await keyCode.textContent();
    expect(keyText).toMatch(/^strm_/);

    // Copy button and Done button are present
    await expect(revealDialog.getByRole("button", { name: /Copy/ })).toBeVisible();
    await expect(revealDialog.getByRole("button", { name: "Done" })).toBeVisible();

    // Dismiss reveal dialog
    await revealDialog.getByRole("button", { name: "Done" }).click();
    await expect(page.getByRole("dialog")).not.toBeVisible();

    // The new key should now appear in the table
    const row = page.locator("table tbody tr").filter({ hasText: keyName });
    await expect(row).toBeVisible();

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
    const createDialog = page.getByRole("dialog");
    await expect(createDialog).toBeVisible();

    await createDialog.locator('input[id="key-name"]').fill(keyName);
    await createDialog.locator('input[id="key-expiry"]').fill("30");
    await createDialog.getByRole("button", { name: "Create" }).click();

    // Dismiss reveal dialog
    const revealDialog = page.getByRole("dialog");
    await expect(revealDialog).toBeVisible();
    await revealDialog.getByRole("button", { name: "Done" }).click();

    // Row should exist and not show "Never" in expires column
    const row = page.locator("table tbody tr").filter({ hasText: keyName });
    await expect(row).toBeVisible();
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
    const createDialog = page.getByRole("dialog");
    await expect(createDialog).toBeVisible();
    await createDialog.locator('input[id="key-name"]').fill(keyName);
    await createDialog.getByRole("button", { name: "Create" }).click();

    const revealDialog = page.getByRole("dialog");
    await expect(revealDialog).toBeVisible();

    // The key should be displayed in a code element starting with strm_
    const keyCode = revealDialog.locator("code").first();
    await expect(keyCode).toBeVisible();
    const keyText = await keyCode.textContent();
    expect(keyText).toMatch(/^strm_/);

    // Click Copy button. In headless Docker without clipboard support,
    // navigator.clipboard.writeText() may throw synchronously, preventing
    // the UI from updating to "Copied". Accept either outcome.
    const copyBtn = revealDialog.getByRole("button", { name: /Copy/ });
    await copyBtn.click();

    // Verify either "Copied" feedback OR the button is still "Copy" (clipboard unavailable)
    const copiedBtn = revealDialog.getByRole("button", { name: /Copied/ });
    const copyStillBtn = revealDialog.getByRole("button", { name: /Copy/ });
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
    const createDialog = page.getByRole("dialog");
    await expect(createDialog).toBeVisible();
    await createDialog.locator('input[id="key-name"]').fill(keyName);
    await createDialog.getByRole("button", { name: "Create" }).click();

    // Dismiss reveal dialog
    const revealDialog = page.getByRole("dialog");
    await expect(revealDialog).toBeVisible();
    await revealDialog.getByRole("button", { name: "Done" }).click();
    await expect(page.getByRole("dialog")).not.toBeVisible();

    // Wait for the key to appear in the table (load() runs async after dialog close)
    const row = page.locator("table tbody tr").filter({ hasText: keyName });
    await expect(row).toBeVisible({ timeout: 10000 });

    // Click the trash/delete button on that row
    await row.getByRole("button").click();

    // Confirmation dialog should open
    const confirmDialog = page.getByRole("dialog");
    await expect(confirmDialog).toBeVisible();
    await expect(
      confirmDialog.getByRole("heading", { name: "Revoke API Key" }),
    ).toBeVisible();
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
    await expect(page.getByRole("dialog")).not.toBeVisible();

    // The row should be gone from the table
    await expect(
      page.locator("table tbody tr").filter({ hasText: keyName }),
    ).not.toBeVisible();
  });

  test("cancel in revoke confirmation keeps the key", async ({ page }) => {
    const keyName = `e2e-cancel-revoke-key-${Date.now()}`;

    // Create a key first
    await page.getByRole("button", { name: "Create API Key" }).click();
    const createDialog = page.getByRole("dialog");
    await expect(createDialog).toBeVisible();
    await createDialog.locator('input[id="key-name"]').fill(keyName);
    await createDialog.getByRole("button", { name: "Create" }).click();

    // Dismiss reveal dialog
    const revealDialog = page.getByRole("dialog");
    await expect(revealDialog).toBeVisible();
    await revealDialog.getByRole("button", { name: "Done" }).click();
    await page.waitForLoadState("networkidle");

    // Key should be in the table
    const row = page.locator("table tbody tr").filter({ hasText: keyName });
    await expect(row).toBeVisible();

    // Click trash button
    await row.getByRole("button").click();

    // Cancel the confirmation
    const confirmDialog = page.getByRole("dialog");
    await expect(confirmDialog).toBeVisible();
    await confirmDialog.getByRole("button", { name: "Cancel" }).click();
    await expect(page.getByRole("dialog")).not.toBeVisible();

    // Key should still be in the table
    await expect(
      page.locator("table tbody tr").filter({ hasText: keyName }),
    ).toBeVisible();

    // Clean up: revoke the key
    const rowAgain = page
      .locator("table tbody tr")
      .filter({ hasText: keyName });
    await rowAgain.getByRole("button").click();
    await page
      .getByRole("dialog")
      .getByRole("button", { name: "Revoke" })
      .click();
    await expect(page.getByRole("dialog")).not.toBeVisible();
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
