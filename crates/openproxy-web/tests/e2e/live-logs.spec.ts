// @see tsconfig.test.json for type settings.

import { test, expect, type Page } from '@playwright/test';

test('Live Logs WebSocket connection', async ({ page }: { page: Page }) => {
  // Navigate to the dashboard
  await page.goto('http://localhost:8788/');

  // Click on the "Live Logs" link to navigate to the logs section
  await page.click('text=Live Logs');

  // Wait for the main content area to be ready
  await expect(page.locator('#main')).toBeVisible();

  // The WebSocket transitions very fast in this environment, so the
  // badge can show either the "connecting" or the "connected" state
  // by the time we read it. Assert it is one of the two valid
  // pre-connected / connected states (NOT the "disconnected" error
  // state), then wait for "connected".
  const statusBadge = page.locator('#logs-connection-status');
  await expect(statusBadge).toHaveText(/🟡 connecting|🟢 connected/);
  await expect(statusBadge).toHaveText('🟢 connected');

  // Verify the logs container is present and shows the table
  // header. (We used to assert the "No recent requests yet" empty
  // state, but the backend persists request rows across test runs,
  // so the container is virtually never empty when the page loads.)
  const logsContainer = page.locator('#logs');
  await expect(logsContainer).toBeVisible();
  await expect(logsContainer.locator('text=Time').first()).toBeVisible();
  await expect(logsContainer.locator('text=Phase').first()).toBeVisible();

  // Give the WebSocket a moment to push some logs (adjust as needed)
  await page.waitForTimeout(3000);

  // After connection, ensure the logs container still exists and may contain data
  await expect(logsContainer).toBeVisible();
});
