#!/usr/bin/env node
import { defineConfig } from '@playwright/test';

// Isolated renderer regression suite: no production API, token or database.
export default defineConfig({
  testDir: './tests/e2e',
  testMatch: '**/zai-quota-pools.spec.ts',
  workers: 1,
  timeout: 30000,
  use: { headless: true, locale: 'en-US', launchOptions: { executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH, ignoreDefaultArgs: ["--disable-dev-shm-usage"] }, baseURL: 'http://127.0.0.1' },
});
