import { test, expect, type Page } from '@playwright/test';

test.describe('OAuth Flows', () => {
  test('manual callback submission handles success/failure', async ({ page }: { page: Page }) => {
    page.on('console', msg => console.log('BROWSER CONSOLE:', msg.text()));
    page.on('pageerror', err => console.log('BROWSER ERROR:', err.message));
    page.on('request', req => console.log('REQ:', req.method(), req.url()));

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

    // Ensure the manual OAuth section is visible for the test
    await page.evaluate(() => {
      const el = document.getElementById('oauth-manual-section');
      if (el) el.style.display = 'block';
    });

    // Ensure the manual OAuth section is visible
    const authUrlInput = page.locator('#oauth-auth-url');
    await expect(authUrlInput).toBeVisible();

    // 1. Submit without an active flow should show an error toast
    const submitBtn = page.locator('button:has-text("Submit")');
    await submitBtn.click();

    // Check for toast error
    const toast = page.locator('.toast.toast-error').first();
    await expect(toast).toBeVisible();
    await expect(toast).toContainText('No OAuth flow in progress');

    // 2. Set force_manual to true and click the actual OAuth button to trigger the authorize flow
    // Intercept window.open so it doesn't actually open a tab
    await page.evaluate(() => {
      (window as any).force_manual = true;
      window.open = function() { return null; };
    });

    await page.route('**/admin/api/oauth/antigravity/authorize*', async route => {
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({
          provider: 'antigravity',
          type: 'oauth_code',
          authorization_url: 'https://mock-auth',
          code_verifier: 'mock-verifier',
          state: 'mock-state'
        })
      });
    });

    const startPromise = page.waitForResponse('**/admin/api/oauth/antigravity/authorize*');
    const loginBtn = page.locator('.oauth-buttons button:has-text("antigravity")');
    await loginBtn.click();
    await startPromise;
    // Wait for showManualPasteForm to execute (it sets the auth URL and clears the callback input)
    await expect(page.locator('#oauth-auth-url')).toHaveValue('https://mock-auth');

    // Wait for the manual section to become visible from the click
    await expect(page.locator('#oauth-manual-section')).toBeVisible();
    const callbackInput = page.locator('#oauth-callback-input');
    await callbackInput.fill('https://localhost/callback?code=mock-code&state=mock-state');

    // Start a network route to intercept the /oauth/antigravity/exchange call
    await page.route('**/admin/api/oauth/antigravity/exchange', async route => {
      // Just mock success response, avoid expects inside route handlers
      // as they abort the request if they fail and swallow errors.
      
      // Mock success response
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({ account_id: 999 })
      });
    });

    // Mock accounts endpoint so it doesn't fail when refreshing state after login
    await page.route('**/admin/api/accounts', async route => {
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify([{ id: 999, name: 'Test Account' }])
      });
    });

    // Click submit
    await submitBtn.click();

    // Expect a success toast
    const successToast = page.locator('.toast.toast-success').first();
    await expect(successToast).toBeVisible({ timeout: 15000 });
    await expect(successToast).toContainText('Logged in with antigravity');
  });
});
