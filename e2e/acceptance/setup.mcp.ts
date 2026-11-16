// Opens the MCP instance once, before the acceptance tests: the same app
// configuration as the main suite's setup, then a new password for the
// seeded admin, the last reason the security gate keeps MCP closed.

import { expect, test } from '@playwright/test';
import { configureInstance, loginViaUi, setPassword } from '../helpers/app.js';
import {
  mcpAdminPassword,
  mcpBaseUrl,
  mcpMusicDirForApp,
  mcpSlskdUrlForApp,
  navidromeAdminPassword,
  navidromeAdminUser,
  navidromeUrl,
  seededAdminId,
} from '../helpers/env.js';

test('open the MCP instance', async ({ page }) => {
  const probe = () => page.request.get('/.well-known/oauth-protected-resource/mcp');
  const first = await probe();
  // A run against this instance already opened it.
  if (first.status() === 200) return;
  expect(first.status()).toBe(503);
  expect(((await first.json()) as { reasons: { code: string }[] }).reasons.map((r) => r.code)).toEqual([
    'default_admin_password',
  ]);

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
  // Navidrome refuses admin/admin, so the login falls back to the seeded local account.
  await loginViaUi(page, { username: 'admin', password: 'admin' });
  await configureInstance(page, mcpSlskdUrlForApp, mcpMusicDirForApp);
  await setPassword(page.context(), seededAdminId, mcpAdminPassword);

  const open = await probe();
  expect(open.status()).toBe(200);
  expect(((await open.json()) as { resource: string }).resource).toBe(`${mcpBaseUrl}/mcp`);
});
