// The phone picture: search results on a narrow screen.

import { test } from '@playwright/test';
import { save, searchAsUser } from './save.js';

test('search results on a phone', async ({ page }) => {
  await searchAsUser(page);
  await save(page, 'mobile');
});
