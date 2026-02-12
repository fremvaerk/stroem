import { test, expect } from "@playwright/test";
import { login } from "./helpers";

test.describe("Login", () => {
  test("successful login redirects to dashboard", async ({ page }) => {
    await login(page);
    await expect(page).toHaveURL("/");
    await expect(page.locator("text=Dashboard")).toBeVisible();
  });

  test("wrong password shows error", async ({ page }) => {
    await page.goto("/login");
    await page.fill('input[id="email"]', "admin@stroem.local");
    await page.fill('input[id="password"]', "wrongpassword");
    await page.click('button[type="submit"]');
    await expect(page.locator("text=Invalid email or password")).toBeVisible();
  });

  test("protected route redirects to login", async ({ page }) => {
    await page.goto("/tasks");
    await expect(page).toHaveURL("/login");
  });

  test("logout returns to login", async ({ page }) => {
    await login(page);
    await expect(page).toHaveURL("/");

    // Click sign out in sidebar
    await page.click("text=Sign out");
    await expect(page).toHaveURL("/login");
  });
});
