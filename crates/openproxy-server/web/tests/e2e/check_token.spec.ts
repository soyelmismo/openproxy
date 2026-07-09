import { test } from '@playwright/test';
test('check token', async ({ page }) => {
  await page.goto('http://localhost:8790/');
  const t = await page.evaluate(() => localStorage.getItem("openproxy:adminToken"));
  console.log("TOKEN IN BROWSER:", t);
  await page.waitForTimeout(2000);
  console.log("HTML:", await page.locator("h2").first().textContent());
});
