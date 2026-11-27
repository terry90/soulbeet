// The desktop pictures: search results, an album's tracks, the downloads
// panel, the dashboard and the settings page.

import { expect, test } from '@playwright/test';
import { openDownloadsPanel, openSettings } from '../helpers/app.js';
import { save, showSearch } from './save.js';
import { holdTransfers } from './transfers.js';

test('search, album, downloads, dashboard and settings', async ({ page }) => {
  const showTransfers = await holdTransfers(page);
  const outside = await showSearch(page);
  // The search form at the top leaves room for its status line and three results.
  await page
    .locator('form', { has: page.locator('#search-query') })
    .evaluate((form) => {
      // Room for the form's glow under the header.
      form.style.scrollMarginTop = '24px';
      form.scrollIntoView({ block: 'start' });
    });
  await save(page, 'search');

  const results = page.locator('section', {
    has: page.getByRole('heading', { name: 'Catalog', exact: true }),
  });
  const album = results.getByRole('listitem').first();
  await album.getByRole('paragraph').first().click();
  // The album has one Download button, and then one per track.
  await expect(album.getByRole('button', { name: 'Download' }).nth(1)).toBeVisible({ timeout: 45_000 });
  // Brings the results to the top so the picture shows the tracks, not the search box.
  await page
    .getByRole('heading', { name: 'Catalog', exact: true })
    .evaluate((heading) => heading.scrollIntoView({ block: 'start' }));
  await save(page, 'album');

  const title = await album.getByRole('paragraph').first().innerText();
  const artist = await album.locator('div.flex-grow > button').first().innerText();
  showTransfers(
    artist,
    title,
    await album.locator('ul li div.flex-grow > span:first-child').allTextContents(),
  );
  await openDownloadsPanel(page);
  await expect(page.getByText('LIB', { exact: true }).first()).toBeVisible();
  await save(page, 'downloads');
  await page.getByRole('button', { name: 'Close downloads' }).click();

  await page.goto('/dashboard');
  await expect(page).toHaveURL(/\/dashboard$/);
  await expect(page.getByText('Rating Distribution')).toBeVisible();
  await save(page, 'dashboard');

  await openSettings(page);
  await expect(page.getByText('/music/library')).toBeVisible();
  await save(page, 'settings');

  expect(outside).toEqual([]);
});
