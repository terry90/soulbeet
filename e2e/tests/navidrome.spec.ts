// Navidrome integration, only when the stack runs one (compose does, the
// native dev stack does not). With NAVIDROME_URL set, logins authenticate
// against Navidrome first and fall back to local accounts.

import { expect, test } from '@playwright/test';
import {
  changePassword,
  loginViaApi,
  loginViaUi,
  openSettings,
  registerUser,
  uniqueUser,
} from '../helpers/app.js';
import { navidromeAdminPassword, navidromeAdminUser, navidromeEnabled } from '../helpers/env.js';

test.skip(!navidromeEnabled, 'stack has no Navidrome (set E2E_NAVIDROME=1)');

test('authenticates through Navidrome and reports the connection', async ({ context, page }) => {
  // The seeded admin takes the Navidrome admin's password for this test.
  await loginViaApi(context, { username: 'admin', password: 'admin' });
  await changePassword(context, 'admin', navidromeAdminPassword);
  await context.clearCookies();

  try {
    await loginViaUi(page, { username: navidromeAdminUser, password: navidromeAdminPassword });

    await openSettings(page);
    await page.getByRole('button', { name: 'Config' }).click();

    await expect(page.getByText('Connected', { exact: true })).toBeVisible({ timeout: 30_000 });
  } finally {
    // Restore admin/admin so setup.app.ts keeps working on reruns.
    await context.clearCookies();
    await loginViaApi(context, { username: 'admin', password: navidromeAdminPassword });
    await changePassword(context, navidromeAdminPassword, 'admin');
  }
});

test('still accepts local accounts when Navidrome rejects them', async ({ page }) => {
  // A locally registered user is unknown to Navidrome, so its auth fails
  // and the local Argon2 fallback must let them in.
  const user = uniqueUser('local-only');
  await registerUser(page.context().request, user);

  await loginViaUi(page, user);
  await expect(page.getByPlaceholder('Search artist, album or track...')).toBeVisible();
});
