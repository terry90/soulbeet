import { defineConfig, devices } from '@playwright/test';
import { baseUrl } from './helpers/env.js';

// Stack lifecycles live outside this config (compose.e2e.yml in CI,
// scripts/dev-stack.sh natively); the suite targets whatever E2E_BASE_URL
// points at.
export default defineConfig({
  testDir: './tests',
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  reporter: process.env.CI ? [['list'], ['html', { open: 'never' }]] : [['list']],
  timeout: 120_000,
  expect: { timeout: 10_000 },
  use: {
    baseURL: baseUrl,
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
  },
  projects: [
    {
      // One-time global app configuration through the real settings UI.
      name: 'setup',
      testMatch: /setup\.app\.ts/,
      use: { ...devices['Desktop Chrome'] },
    },
    {
      name: 'chromium',
      dependencies: ['setup'],
      testIgnore: [/mobile\.spec\.ts/, /failures\.spec\.ts/],
      use: { ...devices['Desktop Chrome'] },
    },
    {
      name: 'mobile',
      dependencies: ['setup'],
      testMatch: /mobile\.spec\.ts/,
      use: { ...devices['Pixel 7'] },
    },
    {
      // Tests that mutate shared stub state (peer behavior, outages) run
      // alone, after everything else.
      name: 'failures',
      dependencies: ['chromium', 'mobile'],
      testMatch: /failures\.spec\.ts/,
      workers: 1,
      use: { ...devices['Desktop Chrome'] },
    },
  ],
});
