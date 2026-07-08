// playwright.config.js — minimal config for the e2e suite. The
// project has no pre-existing config, so Playwright's auto-detect
// path is the test glob. We pin the test dir to tests/e2e and let
// Playwright pick the rest from defaults.
import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: './tests/e2e',
  timeout: 30 * 1000,
  expect: { timeout: 10 * 1000 },
  reporter: 'list',
  use: { 
    headless: true, 
    baseURL: 'http://localhost:8790',
    storageState: 'tests/e2e/storageState.json',
    locale: 'en-US'
  },
  // Single worker to avoid fighting the WebSocket / shared DB.
  workers: 1,
  webServer: {
    command: 'cargo run --release -p openproxy-server',
    port: 8790,
    reuseExistingServer: !process.env.CI,
    cwd: '../../..',
    timeout: 5 * 60 * 1000,
    env: {
      OPENPROXY_MASTER_KEY: 'AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=',
      OPENPROXY_CONFIG: 'crates/openproxy-server/web/tests/e2e/config.test.toml'
    }
  },
});
