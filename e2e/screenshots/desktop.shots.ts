// The desktop pictures: search results, an album's tracks, the downloads
// panel, the dashboard and the settings page.

import { expect, test } from '@playwright/test';
import { openDownloadsPanel, openSettings } from '../helpers/app.js';
import { save, searchAsUser } from './save.js';
import { holdTransfers } from './transfers.js';

test('search, album, downloads, dashboard and settings', async ({ page }) => {
  const showTransfers = await holdTransfers(page);
  await searchAsUser(page);
  await save(page, 'search');

  const album = page.getByRole('listitem').first();
  await album.getByRole('paragraph').first().click();
  // The album has one Download button, and then one per track.
  await expect(album.getByRole('button', { name: 'Download' }).nth(1)).toBeVisible({ timeout: 45_000 });
  // Brings the results to the top so the picture shows the tracks, not the search box.
  await page.getByRole('heading', { name: 'Search Results' }).evaluate((heading) => heading.scrollIntoView({ block: 'start' }));
  await save(page, 'album');

  const [title, artist] = await album.getByRole('paragraph').allTextContents();
  showTransfers(artist, title, await album.locator('span.flex-grow').allTextContents());
  await openDownloadsPanel(page);
  await expect(page.getByText('LIB', { exact: true }).first()).toBeVisible();
  await save(page, 'downloads');
  await page.getByRole('button', { name: 'Close downloads' }).click();

  await page.getByRole('link', { name: 'Dashboard' }).click();
  await expect(page).toHaveURL(/\/dashboard$/);
  await expect(page.getByText('Loading stats...')).toBeHidden({ timeout: 60_000 });
  await save(page, 'dashboard');

  await openSettings(page);
  await save(page, 'settings');
});
