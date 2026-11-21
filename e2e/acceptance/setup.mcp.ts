// Opens the MCP instance before the acceptance tests: a new password for the
// seeded admin, the last reason the security gate keeps MCP closed, then the
// same app configuration as the main suite's setup. The database outlives a
// run, so a rerun signs in with the chosen password and configures again: an
// open gate says only that the password was chosen, not that a run got as
// far as the configuration.

import { expect, test } from '@playwright/test';
import { chooseAdminPassword, configureInstance, loginViaApi, shippedAdmin } from '../helpers/app.js';
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
  const shipped = await page.request.post('/api/auth/login', { data: shippedAdmin });
  if (shipped.ok()) {
    expect(first.status()).toBe(503);
    // An anonymous caller is not told which check fails; the 200 after the new admin password
    // below shows it was the only one.
    expect(await first.json()).toEqual({
      error: 'mcp_unavailable',
      message: 'MCP access stays off until the security checks in Settings pass',
    });
    await chooseAdminPassword(page.context(), mcpAdminPassword);
  } else {
    await loginViaApi(page.context(), { username: 'admin', password: mcpAdminPassword });
  }
  await page.goto('/');
  await configureInstance(page, mcpSlskdUrlForApp, mcpMusicDirForApp);

  const open = await probe();
  expect(open.status()).toBe(200);
  expect(((await open.json()) as { resource: string }).resource).toBe(`${mcpBaseUrl}/mcp`);
});
