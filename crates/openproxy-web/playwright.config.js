// playwright.config.js — minimal config for the e2e suite. The
// project has no pre-existing config, so Playwright's auto-detect
// path is the test glob. We pin the test dir to tests/e2e and let
// Playwright pick the rest from defaults.
const { defineConfig } = require('@playwright/test');

module.exports = defineConfig({
  testDir: './tests/e2e',
  timeout: 30 * 1000,
  expect: { timeout: 10 * 1000 },
  reporter: 'list',
  use: { headless: true, baseURL: 'http://localhost:8788' },
  // Single worker to avoid fighting the WebSocket / shared DB.
  workers: 1,
});
