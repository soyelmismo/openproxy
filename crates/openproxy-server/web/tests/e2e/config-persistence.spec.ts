import { test, expect } from '@playwright/test';
import { readFileSync } from 'node:fs';

// Real round-trip persistence coverage for the Recording TTL config card.
// No page.route mocks: the browser, the waitForResponse predicates, and the
// `page.request` API probes below all hit the real admin API of the server
// spawned by playwright.config.js (separate concern from
// config-recording-ttl.spec.ts, which covers the save wiring with mocked
// responses).

const recordingTtlEndpoint = '/admin/api/config/recording-ttl';
const runtimeConfigEndpoint = '/admin/api/config';
const storageStatePath = 'tests/e2e/storageState.json';

// The dashboard authenticates /admin/api/* with a Bearer token kept in
// localStorage (state/auth.ts). `page.request` shares cookies only, not
// localStorage, so API probes must send the header explicitly. The token is
// read from the same storageState.json the browser context is seeded with —
// single source of truth for test credentials.
function adminAuthHeaders(): Record<string, string> {
  const storageState = JSON.parse(readFileSync(storageStatePath, 'utf8')) as {
    origins: Array<{ origin: string; localStorage: Array<{ name: string; value: string }> }>;
  };
  const origin = storageState.origins.find((o) => o.origin === 'http://localhost:8790');
  const token = origin?.localStorage.find((e) => e.name === 'openproxy_admin_token')?.value;
  if (!token) throw new Error(`openproxy_admin_token not found in ${storageStatePath}`);
  return { Authorization: `Bearer ${token}` };
}

async function readRecordingTtl(page: import('@playwright/test').Page): Promise<number> {
  const response = await page.request.get(runtimeConfigEndpoint, {
    headers: adminAuthHeaders(),
  });
  expect(response.ok()).toBe(true);
  const body = await response.json() as { recording_ttl_secs?: unknown };
  expect(typeof body.recording_ttl_secs).toBe('number');
  return body.recording_ttl_secs as number;
}

test('persists Recording TTL through the real UI and admin API', async ({ page }) => {
  await page.goto('/#/config');

  const input = page.locator('input[name="recording_ttl_secs"]');
  await expect(input).toBeVisible();
  const originalValue = Number(await input.inputValue());
  expect(Number.isSafeInteger(originalValue)).toBe(true);
  expect(originalValue).toBeGreaterThanOrEqual(0);

  const updatedValue = originalValue === 0 ? 1 : 0;
  try {
    await input.fill(String(updatedValue));

    const saveResponse = page.waitForResponse((response) =>
      response.url().endsWith(recordingTtlEndpoint)
      && response.request().method() === 'PUT'
      && response.status() >= 200
      && response.status() < 300,
    );
    await page.getByRole('button', { name: /save/i }).click();
    const response = await saveResponse;
    expect(await response.json()).toMatchObject({ recording_ttl_secs: updatedValue });

    await expect(page.locator('#toast-container')).toContainText(
      `Recording TTL set to ${updatedValue}s`,
    );

    await page.reload();
    await expect(input).toHaveValue(String(updatedValue));

    const persistedValue = await readRecordingTtl(page);
    expect(persistedValue).toBe(updatedValue);
  } finally {
    const restoreResponse = await page.request.put(recordingTtlEndpoint, {
      headers: adminAuthHeaders(),
      data: { recording_ttl_secs: originalValue },
    });
    expect(restoreResponse.status()).toBeGreaterThanOrEqual(200);
    expect(restoreResponse.status()).toBeLessThan(300);
  }
});

test('rejects a negative Recording TTL at the real backend without persisting it', async ({ page }) => {
  await page.goto('/#/config');

  const input = page.locator('input[name="recording_ttl_secs"]');
  await expect(input).toBeVisible();
  const originalValue = await readRecordingTtl(page);

  // The real number input has min="0", so the browser UI rejects a negative
  // value before its change handler. Exercise the same backend validation
  // directly instead of forcing an invalid HTML control state.
  const response = await page.request.put(recordingTtlEndpoint, {
    headers: adminAuthHeaders(),
    data: { recording_ttl_secs: -1 },
  });
  expect(response.status()).toBeGreaterThanOrEqual(400);
  expect(response.status()).toBeLessThan(500);
  await expect(input).toHaveValue(String(originalValue));
  expect(await readRecordingTtl(page)).toBe(originalValue);
});
