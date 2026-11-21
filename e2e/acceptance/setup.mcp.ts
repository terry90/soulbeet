// Opens the MCP instance once, before the acceptance tests: a new password
// for the seeded admin, the last reason the security gate keeps MCP closed,
// then the same app configuration as the main suite's setup.

import { expect, test } from '@playwright/test';
import { chooseAdminPassword, configureInstance } from '../helpers/app.js';
import {
  mcpAdminPassword,
  mcpBaseUrl,
  mcpMusicDirForApp,
  mcpSlskdUrlForApp,
  navidromeAdminPassword,
  navidromeAdminUser,
  navidromeUrl,
} from '../helpers/env.js';

test('open the MCP instance', async ({ page }) => {
  const probe = () => page.request.get('/.well-known/oauth-protected-resource/mcp');
  const first = await probe();
  // A run against this instance already opened it.
  if (first.status() === 200) return;
  expect(first.status()).toBe(503);
  // An anonymous caller is not told which check fails; the 200 after the new admin password
  // below shows it was the only one.
  expect(await first.json()).toEqual({
    error: 'mcp_unavailable',
    message: 'MCP access stays off until the security checks in Settings pass',
  });

  // Navidrome is slower to start than the app, and every test after this one signs in through it.
  await expect
    .poll(
      async () =>
        (
          await fetch(`${navidromeUrl}/auth/login`, {
            method: 'POST',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify({ username: navidromeAdminUser, password: navidromeAdminPassword }),
          })
        ).status,
      { timeout: 60_000 },
    )
    .toBe(200);
  // Navidrome refuses admin/admin, so the login falls back to the seeded local account,
  // which signs in only to choose a new password.
  await chooseAdminPassword(page.context(), mcpAdminPassword);
  await page.goto('/');
  await configureInstance(page, mcpSlskdUrlForApp, mcpMusicDirForApp);

  const open = await probe();
  expect(open.status()).toBe(200);
  expect(((await open.json()) as { resource: string }).resource).toBe(`${mcpBaseUrl}/mcp`);
});
