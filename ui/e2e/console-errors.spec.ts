import { test, expect } from "@playwright/test";

const BASE_URL = process.env.BASE_URL || "http://localhost:8080";

test.describe("Console errors audit", () => {
  let consoleErrors: string[] = [];
  let consoleWarnings: string[] = [];

  test("capture console errors across all pages", async ({ page }) => {
    // Collect all console messages
    page.on("console", (msg) => {
      if (msg.type() === "error") {
        consoleErrors.push(`[${msg.type()}] ${msg.text()}`);
      }
      if (msg.type() === "warning") {
        consoleWarnings.push(`[${msg.type()}] ${msg.text()}`);
      }
    });

    page.on("pageerror", (err) => {
      consoleErrors.push(`[pageerror] ${err.message}`);
    });

    // 1. Login page
    console.log("--- Navigating to /login ---");
    await page.goto(`${BASE_URL}/login`);
    await page.waitForLoadState("networkidle");

    // Login
    const emailInput = page.locator('input[type="email"]');
    if (await emailInput.isVisible()) {
      await emailInput.fill("admin@stroem.local");
      await page.locator('input[type="password"]').fill("admin");
      await page.locator('button[type="submit"]').click();
      await page.waitForURL("**/", { timeout: 5000 }).catch(() => {});
      await page.waitForLoadState("networkidle");
    }

    console.log("--- Dashboard ---");
    // 2. Dashboard
    await page.goto(`${BASE_URL}/`);
    await page.waitForLoadState("networkidle");
    await page.waitForTimeout(1000);

    // 3. Tasks page
    console.log("--- Tasks ---");
    await page.goto(`${BASE_URL}/tasks`);
    await page.waitForLoadState("networkidle");
    await page.waitForTimeout(1000);

    // 4. Task detail (hello-world)
    console.log("--- Task detail ---");
    await page.goto(`${BASE_URL}/tasks/hello-world`);
    await page.waitForLoadState("networkidle");
    await page.waitForTimeout(1000);

    // 5. Jobs page
    console.log("--- Jobs ---");
    await page.goto(`${BASE_URL}/jobs`);
    await page.waitForLoadState("networkidle");
    await page.waitForTimeout(1000);

    // 6. Find a completed job and visit its detail page
    console.log("--- Job detail ---");
    const jobLinks = page.locator("a[href^='/jobs/']");
    const firstJobHref = await jobLinks.first().getAttribute("href");
    if (firstJobHref) {
      await page.goto(`${BASE_URL}${firstJobHref}`);
      await page.waitForLoadState("networkidle");
      await page.waitForTimeout(2000);
    }

    // Print results
    console.log("\n========== CONSOLE ERRORS ==========");
    if (consoleErrors.length === 0) {
      console.log("No console errors found!");
    } else {
      for (const err of consoleErrors) {
        console.log(err);
      }
    }

    console.log("\n========== CONSOLE WARNINGS ==========");
    if (consoleWarnings.length === 0) {
      console.log("No console warnings found!");
    } else {
      for (const warn of consoleWarnings) {
        console.log(warn);
      }
    }

    console.log(`\nTotal: ${consoleErrors.length} errors, ${consoleWarnings.length} warnings`);
  });
});
