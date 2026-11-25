import { defineConfig, devices } from '@playwright/test';
import { baseUrl } from './helpers/env.js';

// Retakes the pictures in ../screenshots on the stub stack (scripts/dev-stack.sh
// up), then `npm run screenshots`. The suite's app configuration runs first and
// points the app at the slskd stub, so the downloader reads online for real.
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
      name: 'setup',
      testMatch: /screenshots\/setup\.shots\.ts/,
      use: { ...devices['Desktop Chrome'] },
    },
    {
      name: 'desktop',
      dependencies: ['setup'],
      testMatch: /screenshots\/desktop\.shots\.ts/,
      use: { ...devices['Desktop Chrome'], viewport: { width: 1280, height: 800 }, deviceScaleFactor: 2 },
    },
    {
      name: 'mobile',
      dependencies: ['setup'],
      testMatch: /screenshots\/mobile\.shots\.ts/,
      use: { ...devices['Pixel 7'] },
    },
  ],
});
