// Global app configuration, done once before every other project: points the
// app at the slskd stub through the real settings UI. slskd config is
// app-wide (app_config table), not per-user.

import { expect, test } from '@playwright/test';
import { loginViaUi, openSettings } from '../helpers/app.js';
import { slskdApiKey, slskdUrlForApp } from '../helpers/env.js';

test('configure slskd connection', async ({ page }) => {
  // The migration seeds a local admin/admin user.
  await loginViaUi(page, { username: 'admin', password: 'admin' });

  await openSettings(page);
  await page.getByRole('button', { name: 'Config' }).click();

  await page.getByPlaceholder('http://localhost:5030').fill(slskdUrlForApp);
  await page.getByPlaceholder('Enter slskd API key').fill(slskdApiKey);
  await page.getByRole('button', { name: 'Save Configuration' }).click();

  await expect(page.getByText('Configuration saved')).toBeVisible();
});
