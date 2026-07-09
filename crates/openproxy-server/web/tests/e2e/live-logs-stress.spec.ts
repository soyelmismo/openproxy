import { test, expect, type Page } from '@playwright/test';

test.describe('Live Logs Stress Test', () => {
  test('handles 1000 logs pushed rapidly without crashing', async ({ page }: { page: Page }) => {
    test.setTimeout(60000);
    // Intercept WebSocket and inject 1000 messages
    await page.routeWebSocket('**/admin/ws**', ws => {
      const server = ws.connectToServer();
      
      ws.onMessage(message => {
        server.send(message);
      });
      
      let injected = false;
      server.onMessage(message => {
        ws.send(message);
        
        // Inject burst of 1000 logs after the first message (snapshot) from the server
        if (!injected) {
          injected = true;
          const baseTime = Date.now();
          for (let i = 0; i < 1000; i++) {
            ws.send(JSON.stringify({
              type: "usage_row",
              row: {
                id: i + 1,
                request_id: `stress-req-${i}`,
                created_at: new Date(baseTime + i).toISOString(),
                method: "POST",
                path: "/v1/chat/completions",
                provider_id: "gemini",
                account_id: 1,
                model: "gemini-1.5-flash",
                status_code: 200,
                phase: "streaming",
                prompt_tokens: 10,
                completion_tokens: 20,
                total_ms: 100
              }
            }));
          }
        }
      });
    });

    await page.goto('/#/logs');

    // Wait for the last of the stress rows to appear
    const lastRow = page.locator('.log-row[data-request-id="stress-req-999"]');
    
    // Dump the DOM for debugging if it fails, or just dump it before waiting
    try {
      await lastRow.waitFor({ state: 'visible', timeout: 5000 });
    } catch (e) {
      console.log("DUMPING LOGS CONTAINER INNER HTML:");
      console.log(await page.locator('#logs').innerHTML());
      throw e;
    }

    // Verify UI is still responsive by clicking the row
    await lastRow.click();
    await expect(page.locator('.log-detail-modal')).toBeVisible();

    // Verify we can close it
    const modal = page.locator('.log-detail-modal');
    await modal.locator('.close-btn').click();
    await expect(modal).toBeHidden();
  });
});
