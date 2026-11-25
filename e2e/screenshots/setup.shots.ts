// The suite's global app configuration (see configureApp), with the outside
// world staged as in the pictures, so the run asks nothing of the network.

import { expect, test } from '@playwright/test';
import { configureApp } from '../helpers/app.js';
import { stageOutside } from './stage.js';

test('configure slskd connection', async ({ page }) => {
  const outside = await stageOutside(page);
  await configureApp(page);
  expect(outside).toEqual([]);
});
