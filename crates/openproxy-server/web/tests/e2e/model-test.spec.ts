import { test, expect, type Page } from '@playwright/test';

test.describe('Model Test functionality', () => {
  test('Clicking Test on a model calls the API and shows success', async ({ page }: { page: Page }) => {
    await page.route('**/admin/api/providers/gemini', async route => {
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({ id: 'gemini', name: 'Gemini', auth_type: 'api_key', is_active: true })
      });
    });
    // Mock the models endpoint so the page renders some models
    await page.route('**/admin/api/models*', async route => {
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify([
          { row_id: 1, provider_id: 'gemini', model_id: 'gemini-1.5-flash', active: true, target_format: 'openai', discovered_at: '2025-01-01', model_type: 'chat' }
        ])
      });
    });

    // Navigate to the gemini provider detail view
    await page.goto('/#/providers/gemini');

    // Wait for the provider page to load and models to populate
    await page.waitForSelector('h2:has-text("gemini")', { timeout: 10000 });
    
    // Wait for at least one model row's Test button to appear
    const testBtn = page.locator('button[id^="test-btn-"]').first();
    await testBtn.waitFor({ state: 'visible' });

    // Assert the button says Test initially
    await expect(testBtn).toHaveText('Test');

    // Intercept the test endpoint
    await page.route('**/admin/api/models/*/test', async route => {
      const request = route.request();
      expect(request.method()).toBe('POST');
      
      // Send a mock successful response after a tiny delay
      setTimeout(() => {
        route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify({ status: 200, elapsed_ms: 150 })
        }).catch(() => {});
      }, 100);
    });

    // Click the Test button
    await testBtn.click();

    // Verify it changed to "Testing..."
    // Since Playwright runs fast, this might flip quickly, but we can try to catch it,
    // or just wait for the final "✓" state
    await expect(testBtn).toHaveText('✓', { timeout: 2000 });

    // Verify the background color
    await expect(testBtn).toHaveCSS('background-color', 'rgb(166, 227, 161)');
  });
});
