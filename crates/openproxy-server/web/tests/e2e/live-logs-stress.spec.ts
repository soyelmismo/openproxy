import { test, expect, type Page } from '@playwright/test';

test.describe('Live Logs Stress Test', () => {
  test('handles 1000 logs pushed rapidly without crashing', async ({ page }: { page: Page }) => {
    // Intercept WebSocket and inject 1000 messages
    await page.routeWebSocket('**/admin/ws**', ws => {
      const server = ws.connectToServer();
      
      ws.onMessage(message => {
        server.send(message);
      });
      server.onMessage(message => {
        ws.send(message);
      });
      
      // Inject burst of 1000 logs
      for (let i = 0; i < 1000; i++) {
        ws.send(JSON.stringify({
          type: "log_row",
          request_id: `stress-req-${i}`,
          timestamp: new Date().toISOString(),
          method: "POST",
          path: "/v1/chat/completions",
          provider_id: "gemini",
          account_id: 1,
          model: "gemini-1.5-flash",
          status: 200,
          phase: "streaming",
          tokens_in: 10,
          tokens_out: 20,
          total_ms: 100
        }));
      }
    });

    await page.goto('/#/logs');

    // Wait for the last of the stress rows to appear
    const lastRow = page.locator('tr[data-id="stress-req-999"]');
    await lastRow.waitFor({ state: 'visible', timeout: 15000 });

    // Verify UI is still responsive by clicking the row
    await lastRow.click();
    await expect(page.locator('.log-detail-modal')).toBeVisible();

    // Verify we can close it
    await page.locator('.modal-close').click();
    await expect(page.locator('.log-detail-modal')).toBeHidden();
  });
});
