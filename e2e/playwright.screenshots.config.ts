import { defineConfig, devices } from '@playwright/test';
import { baseUrl } from './helpers/env.js';

// Retakes the pictures in ../screenshots on a running Soulbeet, yours or the
// e2e stack: E2E_BASE_URL, SHOTS_USER, SHOTS_PASSWORD and SHOTS_QUERY (an
// album title to search for), then `npm run screenshots`.
export default defineConfig({
  testDir: '.',
  fullyParallel: false,
  workers: 1,
  retries: 0,
  reporter: [['list']],
  timeout: 180_000,
  expect: { timeout: 10_000 },
  use: { baseURL: baseUrl },
  projects: [
    {
      name: 'desktop',
      testMatch: /screenshots\/desktop\.shots\.ts/,
      use: { ...devices['Desktop Chrome'], viewport: { width: 1280, height: 800 }, deviceScaleFactor: 2 },
    },
    {
      name: 'mobile',
      testMatch: /screenshots\/mobile\.shots\.ts/,
      use: { ...devices['Pixel 7'] },
    },
  ],
});
