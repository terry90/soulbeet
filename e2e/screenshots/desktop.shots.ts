// The desktop pictures: search results, an album's tracks, the downloads
// panel, the dashboard and the settings page.

import { expect, test } from '@playwright/test';
import { openDownloadsPanel, openSettings } from '../helpers/app.js';
import { save, searchAsUser } from './save.js';

test('search, album, downloads, dashboard and settings', async ({ page }) => {
  await searchAsUser(page);
  await save(page, 'search');

  const album = page.getByRole('listitem').first();
  await album.getByRole('paragraph').first().click();
  // The album has one Download button, and then one per track.
  await expect(album.getByRole('button', { name: 'Download' }).nth(1)).toBeVisible({ timeout: 45_000 });
  await save(page, 'album');

  await openDownloadsPanel(page);
  await save(page, 'downloads');
  await page.getByRole('button', { name: 'Close downloads' }).click();

  await page.getByRole('link', { name: 'Dashboard' }).click();
  await expect(page).toHaveURL(/\/dashboard$/);
  await save(page, 'dashboard');

  await openSettings(page);
  await save(page, 'settings');
});
