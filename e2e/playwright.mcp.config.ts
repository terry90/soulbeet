import { defineConfig, devices } from '@playwright/test';
import { mcpBaseUrl } from './helpers/env.js';

// The MCP acceptance suite. Its stack (compose.mcp.yml) lives outside this
// config; the contract project talks to the stubs only.
export default defineConfig({
  testDir: './acceptance',
  fullyParallel: false,
  workers: 1,
  retries: 0,
  forbidOnly: !!process.env.CI,
  reporter: process.env.CI ? [['list'], ['html', { open: 'never' }]] : [['list']],
  timeout: 15 * 60_000,
  expect: { timeout: 10_000 },
  use: {
    baseURL: mcpBaseUrl,
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
  },
  projects: [
    { name: 'contracts', testMatch: /\.contract\.ts/ },
    {
      // Configures the MCP instance and opens its security gate, once.
      name: 'setup-mcp',
      dependencies: ['contracts'],
      testMatch: /setup\.mcp\.ts/,
      use: { ...devices['Desktop Chrome'] },
    },
    {
      name: 'mcp',
      dependencies: ['setup-mcp'],
      testMatch: /\.acceptance\.ts/,
      use: { ...devices['Desktop Chrome'] },
    },
  ],
});
