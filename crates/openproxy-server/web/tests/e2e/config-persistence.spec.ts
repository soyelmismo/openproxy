import { test, expect } from '@playwright/test';
import { readFileSync } from 'node:fs';

// Real round-trip persistence coverage for the Recording TTL config card.
// No page.route mocks: the browser, the waitForResponse predicates, and the
// `page.request` API probes below all hit the real admin API of the server
// spawned by playwright.config.js.

const recordingTtlEndpoint = '/admin/api/config/recording-ttl';
const runtimeConfigEndpoint = '/admin/api/config';
const storageStatePath = 'tests/e2e/storageState.json';

// `page.request` shares cookies, not localStorage, so API probes must send the
// Bearer token (state/auth.ts) read from the seeded storageState.json.
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

// --- Concurrency caps: live-tunable pair, applied from the next request -----

const concurrencyEndpoint = '/admin/api/config/concurrency';

async function readConcurrency(page: import('@playwright/test').Page): Promise<{
  max_concurrent_per_key: number;
  max_concurrent_global: number;
}> {
  const response = await page.request.get(concurrencyEndpoint, {
    headers: adminAuthHeaders(),
  });
  expect(response.ok()).toBe(true);
  const body = await response.json() as {
    max_concurrent_per_key?: unknown;
    max_concurrent_global?: unknown;
  };
  expect(typeof body.max_concurrent_per_key).toBe('number');
  expect(typeof body.max_concurrent_global).toBe('number');
  return {
    max_concurrent_per_key: body.max_concurrent_per_key as number,
    max_concurrent_global: body.max_concurrent_global as number,
  };
}

test('persists the Concurrency caps through the real UI and admin API', async ({ page }) => {
  await page.goto('/#/config');

  const perKey = page.locator('input[name="max_concurrent_per_key"]');
  await expect(perKey).toBeVisible();
  const original = await readConcurrency(page);
  // The shipped defaults: a small fairness bound per key, a wide proxy ceiling.
  expect(original.max_concurrent_per_key).toBe(10);
  expect(original.max_concurrent_global).toBe(512);

  const updatedPerKey = 12;
  try {
    await perKey.fill(String(updatedPerKey));

    const saveResponse = page.waitForResponse((response) =>
      response.url().endsWith(concurrencyEndpoint)
      && response.request().method() === 'PUT'
      && response.status() >= 200
      && response.status() < 300,
    );
    await perKey.press('Enter');
    const response = await saveResponse;
    expect(await response.json()).toMatchObject({
      max_concurrent_per_key: updatedPerKey,
      applies_to: 'next_request',
    });

    await page.reload();
    await expect(perKey).toHaveValue(String(updatedPerKey));
    expect((await readConcurrency(page)).max_concurrent_per_key).toBe(updatedPerKey);
  } finally {
    const restoreResponse = await page.request.put(concurrencyEndpoint, {
      headers: adminAuthHeaders(),
      data: {
        max_concurrent_per_key: original.max_concurrent_per_key,
        max_concurrent_global: original.max_concurrent_global,
      },
    });
    expect(restoreResponse.status()).toBeGreaterThanOrEqual(200);
    expect(restoreResponse.status()).toBeLessThan(300);
  }
});

test('rejects a per-key cap above the global cap at the real backend', async ({ page }) => {
  await page.goto('/#/config');
  const original = await readConcurrency(page);

  const response = await page.request.put(concurrencyEndpoint, {
    headers: adminAuthHeaders(),
    data: {
      max_concurrent_per_key: original.max_concurrent_global + 1,
      max_concurrent_global: original.max_concurrent_global,
    },
  });
  expect(response.status()).toBeGreaterThanOrEqual(400);
  expect(response.status()).toBeLessThan(500);

  // Nothing was persisted by the refused write, and the UI still shows the
  // live values served by GET /admin/api/config/concurrency.
  expect(await readConcurrency(page)).toEqual(original);
  await expect(page.locator('input[name="max_concurrent_per_key"]'))
    .toHaveValue(String(original.max_concurrent_per_key));
});
