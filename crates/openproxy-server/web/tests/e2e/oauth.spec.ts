import { test, expect, type Page } from '@playwright/test';

test.describe('OAuth Flows', () => {
  test('manual callback submission handles success/failure', async ({ page }: { page: Page }) => {
    // Mock the provider details and models so the page renders
    await page.route('**/admin/api/providers/antigravity', async route => {
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({ id: 'antigravity', name: 'Antigravity', auth_type: 'oauth', is_active: true })
      });
    });
    await page.route('**/admin/api/providers/antigravity/models', async route => {
      await route.fulfill({ status: 200, contentType: 'application/json', body: '[]' });
    });

    // Navigate to the antigravity provider detail view
    await page.goto('/#/providers/antigravity');

    // Wait for the view to load
    await page.waitForSelector('h2:has-text("antigravity")', { timeout: 10000 });

    // Ensure the manual OAuth section is visible
    const authUrlInput = page.locator('#oauth-auth-url');
    await expect(authUrlInput).toBeVisible();

    // 1. Submit without an active flow should show an error toast
    const submitBtn = page.locator('button:has-text("Submit")');
    await submitBtn.click();
    
    // Check for toast error
    const toast = page.locator('.toast.error').first();
    await expect(toast).toBeVisible();
    await expect(toast).toContainText('No OAuth flow in progress');

    // 2. Set up a mock OAuth flow in sessionStorage
    await page.evaluate(() => {
      sessionStorage.setItem('openproxy:oauth', JSON.stringify({
        provider: 'antigravity',
        type: 'oauth_code',
        code_verifier: 'mock-verifier',
        state: 'mock-state'
      }));
    });

    // We don't need to reload because the handler reads from sessionStorage on click
    const callbackInput = page.locator('#oauth-callback-input');
    await callbackInput.fill('https://localhost/callback?code=mock-code&state=mock-state');

    // Start a network route to intercept the /oauth/antigravity/exchange call
    await page.route('**/admin/api/oauth/antigravity/exchange', async route => {
      const request = route.request();
      expect(request.method()).toBe('POST');
      const postData = JSON.parse(request.postData() || '{}');
      expect(postData.code).toBe('mock-code');
      expect(postData.state).toBe('mock-state');
      
      // Mock success response
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({ account_id: 999 })
      });
    });

    // Click submit
    await submitBtn.click();

    // Expect a success toast
    const successToast = page.locator('.toast.success').first();
    await expect(successToast).toBeVisible();
    await expect(successToast).toContainText('OAuth connection successful');
  });
});
