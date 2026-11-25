// Global app configuration, done once before every other project (see
// configureApp).

import { test } from '@playwright/test';
import { configureApp } from '../helpers/app.js';

test('configure slskd connection', async ({ page }) => {
  await configureApp(page);
});
