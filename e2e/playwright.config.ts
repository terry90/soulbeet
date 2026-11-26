import { defineConfig, devices } from '@playwright/test';
import { baseUrl, user90BaseUrl } from './helpers/env.js';

// Stack lifecycles live outside this config (compose.e2e.yml in CI,
// scripts/dev-stack.sh natively); the suite targets whatever E2E_BASE_URL
// and E2E_USER90_BASE_URL point at.
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
      // The same setup on the instance with the issue #90 beets config.
      name: 'setup-user90',
      testMatch: /setup\.app\.ts/,
      use: { ...devices['Desktop Chrome'], baseURL: user90BaseUrl },
    },
    {
      name: 'chromium',
      dependencies: ['setup'],
      testIgnore: [
        /mobile\.spec\.ts/,
        /failures\.spec\.ts/,
        /imports\.spec\.ts/,
        /user90\.spec\.ts/,
        /navidrome\.spec\.ts/,
        /admin-password\.spec\.ts/,
        /archive-backend\.spec\.ts/,
        /ytdlp\.spec\.ts/,
        /gdstudio\.spec\.ts/,
        /direct-connect\.spec\.ts/,
      ],
      use: { ...devices['Desktop Chrome'] },
    },
    {
      name: 'user90',
      dependencies: ['setup-user90'],
      testMatch: /user90\.spec\.ts/,
      use: { ...devices['Desktop Chrome'], baseURL: user90BaseUrl },
    },
    {
      name: 'mobile',
      dependencies: ['setup'],
      testMatch: /mobile\.spec\.ts/,
      use: { ...devices['Pixel 7'] },
    },
    {
      // Gives the seeded admin the shipped password and chooses a new one,
      // so it runs while no other spec signs in as admin, and before the
      // Navidrome spec links that account to Navidrome.
      name: 'admin-password',
      dependencies: ['chromium', 'mobile', 'user90'],
      testMatch: /admin-password\.spec\.ts/,
      workers: 1,
      use: { ...devices['Desktop Chrome'] },
    },
    {
      // Changes the seeded admin's password, so it runs while no other spec
      // signs in as admin.
      name: 'navidrome',
      dependencies: ['chromium', 'mobile', 'user90', 'admin-password'],
      testMatch: /navidrome\.spec\.ts/,
      workers: 1,
      use: { ...devices['Desktop Chrome'] },
    },
    {
      // Tests that mutate shared stub state (peer behavior, outages) run
      // alone, after everything else.
      name: 'failures',
      dependencies: ['chromium', 'mobile', 'user90', 'navidrome'],
      testMatch: /failures\.spec\.ts/,
      workers: 1,
      use: { ...devices['Desktop Chrome'] },
    },
    {
      // Import queue scenarios script the MusicBrainz stub and reset the
      // stubs, so they run alone too, after the failures.
      name: 'imports',
      dependencies: ['failures'],
      testMatch: /imports\.spec\.ts/,
      workers: 1,
      use: { ...devices['Desktop Chrome'] },
    },
    {
      // Turns the Internet Archive backend on for the whole instance, which every search then
      // asks too, so it runs alone, last.
      name: 'archive',
      dependencies: ['imports'],
      testMatch: /archive-backend\.spec\.ts/,
      workers: 1,
      use: { ...devices['Desktop Chrome'] },
    },
    {
      // Turns YouTube Music on for the whole instance, and ends with YouTube rate-limiting it,
      // a pause that outlives the spec, so it runs alone, after the archive. No retry: a rerun
      // starts inside that pause, where its first test cannot pass.
      name: 'ytdlp',
      dependencies: ['archive'],
      testMatch: /ytdlp\.spec\.ts/,
      workers: 1,
      retries: 0,
      use: { ...devices['Desktop Chrome'] },
    },
    {
      // Turns the GD Studio backend on for the whole instance and scripts its double, so it
      // runs alone, after YouTube Music.
      name: 'gdstudio',
      dependencies: ['ytdlp'],
      testMatch: /gdstudio\.spec\.ts/,
      workers: 1,
      use: { ...devices['Desktop Chrome'] },
    },
    {
      // Turns Direct Connect on for the whole instance, as the archive project does its
      // backend, so it runs alone, after the GD Studio project.
      name: 'direct-connect',
      dependencies: ['gdstudio'],
      testMatch: /direct-connect\.spec\.ts/,
      workers: 1,
      use: { ...devices['Desktop Chrome'] },
    },
  ],
});
