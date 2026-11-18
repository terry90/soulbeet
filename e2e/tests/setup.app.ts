// Global app configuration, done once before every other project: points the
// app at the slskd stub through the real settings UI.

import { test } from '@playwright/test';
import { configureInstance, loginViaUi } from '../helpers/app.js';
import { musicDirForApp, slskdUrlForApp } from '../helpers/env.js';

test('configure slskd connection', async ({ page }) => {
  // The migration seeds a local admin/admin user.
  await loginViaUi(page, { username: 'admin', password: 'admin' });
  await configureInstance(page, slskdUrlForApp, musicDirForApp);
});
