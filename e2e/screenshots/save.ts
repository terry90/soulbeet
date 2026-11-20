// What the screenshot specs share: the instance and account the pictures are
// taken on, and where a picture is saved.
//
// The pictures come from a real install, so they show real covers and a real
// library. Nothing is downloaded or changed there: the specs only search and
// open pages.

import { dirname, join } from 'node:path';
import type { Page } from '@playwright/test';
import { loginViaUi, performSearch } from '../helpers/app.js';
import { e2eRoot } from '../helpers/env.js';

function required(name: string): string {
  const value = process.env[name];
  if (value === undefined || value === '') {
    throw new Error(
      `${name} is not set. Run: E2E_BASE_URL=https://your.soulbeet SHOTS_USER=you SHOTS_PASSWORD=... SHOTS_QUERY="an album title" npm run screenshots`,
    );
  }
  return value;
}

/** Signs in as SHOTS_USER and shows the album results for SHOTS_QUERY. */
export async function searchAsUser(page: Page): Promise<void> {
  // Release notes the account has not read would open over the pictures.
  // Answered here, so nothing is marked as read on the instance.
  await page.route('**/api/changelog/seen', (route) => route.fulfill({ json: { major: 9999, minor: 0, patch: 0 } }));
  await loginViaUi(page, { username: required('SHOTS_USER'), password: required('SHOTS_PASSWORD') });
  await performSearch(page, required('SHOTS_QUERY'), 'ALBUM');
}

/** Saves the visible page as `screenshots/<name>.png` at the repository root, once it has settled. */
export async function save(page: Page, name: string): Promise<void> {
  await page.waitForLoadState('networkidle');
  await page.screenshot({
    path: join(dirname(e2eRoot), 'screenshots', `${name}.png`),
    animations: 'disabled',
  });
}
