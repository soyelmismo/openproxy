// e2e/config-recording-ttl.spec.ts — Config page Recording TTL save action.
//
// This test exercises the dashboard wiring for the new Recording TTL section:
// the Save Recording TTL button must call the proxied admin endpoint with the
// value from the input and show the existing success toast feedback.

import { test, expect, type Page } from '@playwright/test';

test('Config Recording TTL save action posts the configured TTL', async ({ page }: { page: Page }) => {
  let putSeen = false;

  await page.route('**/admin/api/config/recording-ttl', async (route) => {
    putSeen = true;
    expect(route.request().method()).toBe('PUT');
    expect(route.request().headers()['content-type']).toContain('application/json');
    expect(JSON.parse(route.request().postData() || '')).toEqual({ recording_ttl_secs: 123 });
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ recording_ttl_secs: 123, applies_to: 'next_prune_tick' }),
    });
  });

  await page.goto('http://localhost:8790/');
  await page.click('a[href="#/config"]');
  await expect(page.locator('#main')).toBeVisible();

  const input = page.locator('input[name="recording_ttl_secs"]');
  await expect(input).toBeVisible();
  await input.fill('123');
  await page.click('button[data-action="configSaveRecordingTtl"]');

  await expect.poll(() => putSeen).toBe(true);
  await expect(page.locator('text=Recording TTL set to 123s').first()).toBeVisible();
});
