// What the screenshot specs share: a new account on the stub stack with the
// pictures' search done, and where a picture is saved.

import { dirname, join } from 'node:path';
import type { Page } from '@playwright/test';
import { expect } from '@playwright/test';
import { nightGarden } from '../fixtures/dataset.js';
import { loginViaApi, performSearch, registerUser, uniqueUser } from '../helpers/app.js';
import { e2eRoot } from '../helpers/env.js';
import { drawCovers } from './covers.js';
import { stageApp, stageOutside } from './stage.js';

/** What the pictures search for: five albums and EPs of two artists. */
const QUERY = nightGarden.title;

/**
 * Stages the page (see stage.ts and covers.ts), signs a new account in and
 * searches albums for QUERY once every service reads online. Returns the
 * outside URLs the page asked for, which stay refused.
 */
export async function showSearch(page: Page): Promise<string[]> {
  const outside = await stageOutside(page);
  await stageApp(page);
  await drawCovers(page);
  const user = uniqueUser('shots');
  await registerUser(page.request, user);
  await loginViaApi(page.context(), user);
  await page.goto('/');
  // The first health answers come while beets is still being checked.
  await expect(page.getByText('All services online')).toBeVisible({ timeout: 60_000 });
  await performSearch(page, QUERY, 'ALBUM');
  return outside;
}

/** Saves the visible page as `screenshots/<name>.png` at the repository root, once it has settled. */
export async function save(page: Page, name: string): Promise<void> {
  // Off every control, so no row shows its hover look.
  await page.mouse.move(1, 1);
  await page.waitForLoadState('networkidle');
  await page.evaluate(() => document.fonts.ready);
  await page.screenshot({
    path: join(dirname(e2eRoot), 'screenshots', `${name}.png`),
    animations: 'disabled',
  });
}
