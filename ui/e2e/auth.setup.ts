import { test as setup, expect } from "@playwright/test";

const authFile = "e2e/.auth/user.json";

setup("authenticate", async ({ page }) => {
  await page.goto("/login");
  await page.fill('input[id="email"]', "admin@stroem.local");
  await page.fill('input[id="password"]', "admin");
  await page.click('button[type="submit"]');
  await page.waitForURL("/");

  // Save signed-in state so all tests reuse it without hitting /login again
  await page.context().storageState({ path: authFile });
});
