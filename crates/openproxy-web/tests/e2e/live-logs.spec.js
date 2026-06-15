const { test, expect } = require('@playwright/test');

test('Live Logs WebSocket connection', async ({ page }) => {
  // Navigate to the dashboard
  await page.goto('http://localhost:8788/');
  
  // Click on the "Live Logs" link to navigate to the logs section
  await page.click('text=Live Logs');
  
  // Wait for the main content area to be ready
  await expect(page.locator('#main')).toBeVisible();

  // Verify the connection status badge initially shows "disconnected"
  const statusBadge = page.locator('#logs-connection-status');
  await expect(statusBadge).toHaveText('🔴 disconnected');

  // Wait for the WebSocket to connect and the status to change to "connected"
  await expect(statusBadge).toHaveText('🟢 connected');

  // Verify that the logs container is present and initially empty
  const logsContainer = page.locator('#logs');
  await expect(logsContainer).toBeVisible();
  await expect(logsContainer).toContainText('No recent requests yet. Use the API to see logs appear here in real time.');
  
  // Give the WebSocket a moment to push some logs (adjust as needed)
  await page.waitForTimeout(3000);
  
  // After connection, ensure the logs container still exists and may contain data
  await expect(logsContainer).toBeVisible();
});