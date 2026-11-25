// The phone picture: search results on a narrow screen.

import { expect, test } from '@playwright/test';
import { save, showSearch } from './save.js';

test('search results on a phone', async ({ page }) => {
  const outside = await showSearch(page);
  await save(page, 'mobile');
  expect(outside).toEqual([]);
});
