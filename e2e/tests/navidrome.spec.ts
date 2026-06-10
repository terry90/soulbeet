// Navidrome integration, only when the stack runs one (compose does, the
// native dev stack does not). With NAVIDROME_URL set, logins authenticate
// against Navidrome first and fall back to local accounts.

import { expect, test } from '@playwright/test';
import { loginViaUi, openSettings } from '../helpers/app.js';
import { navidromeAdminPassword, navidromeAdminUser, navidromeEnabled } from '../helpers/env.js';

test.skip(!navidromeEnabled, 'stack has no Navidrome (set E2E_NAVIDROME=1)');

test('authenticates through Navidrome and reports the connection', async ({ page }) => {
  await loginViaUi(page, { username: navidromeAdminUser, password: navidromeAdminPassword });

  await openSettings(page);
  await page.getByRole('button', { name: 'Config' }).click();

  await expect(page.getByText('Connected', { exact: true })).toBeVisible({ timeout: 30_000 });
});

test('still accepts local accounts when Navidrome rejects them', async ({ page }) => {
  // The seeded admin/admin user does not exist in Navidrome with that
  // password; the local Argon2 fallback must let it in.
  await loginViaUi(page, { username: 'admin', password: 'admin' });
  await expect(page.getByPlaceholder('Search artist, album or track...')).toBeVisible();
});
