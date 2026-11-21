// Global app configuration, done once before every other project: chooses
// the seeded admin's password, then points the app at the slskd stub through
// the real settings UI.

import { test } from '@playwright/test';
import { admin, chooseAdminPassword, configureInstance, loginViaUi, shippedAdmin } from '../helpers/app.js';
import { appAdminPassword, musicDirForApp, slskdUrlForApp } from '../helpers/env.js';

test('configure slskd connection', async ({ page }) => {
  // The migration seeds admin/admin, which signs in only to choose a new
  // password. A rerun against the same database finds it chosen.
  const shipped = await page.request.post('/api/auth/login', { data: shippedAdmin });
  if (shipped.ok()) {
    await chooseAdminPassword(page.context(), appAdminPassword);
  }
  await loginViaUi(page, admin);
  await configureInstance(page, slskdUrlForApp, musicDirForApp);
});
