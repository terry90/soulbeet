// Metadata search through the UI, served by the MusicBrainz stub.

import type { Page } from '@playwright/test';
import { expect, test } from '@playwright/test';
import { freshSession } from '../helpers/app.js';
import { glassAtlas, maritime } from '../fixtures/dataset.js';

async function search(page: Page, query: string, type?: 'ALBUM' | 'TRACK') {
  if (type) {
    await page.getByRole('button', { name: type, exact: true }).filter({ visible: true }).click();
  }
  await page.getByPlaceholder('Search artist, album or track...').fill(query);
  await page.getByRole('button', { name: 'SEARCH' }).filter({ visible: true }).click();
}

// The app spaces MusicBrainz requests out (1 req/s by design), so results
// can queue up behind other workers' searches.
const RESULT_TIMEOUT = 45_000;

test('finds a track with artist and album metadata', async ({ context, page }) => {
  await freshSession(context, page, 'search-track');

  await search(page, 'Northern Line', 'TRACK');

  await expect(page.getByText('Search Results')).toBeVisible({ timeout: RESULT_TIMEOUT });
  await expect(page.getByText('Northern Line')).toBeVisible();
  await expect(page.getByText(glassAtlas.artist).first()).toBeVisible();
});

test('narrows a track search with the artist field', async ({ context, page }) => {
  await freshSession(context, page, 'search-artist');

  await page
    .getByPlaceholder('Artist (opt)')
    .filter({ visible: true })
    .fill(maritime.artist);
  await search(page, 'Maritime', 'TRACK');

  await expect(page.getByText(maritime.artist).first()).toBeVisible({ timeout: RESULT_TIMEOUT });
});

test('finds an album and expands its tracklist', async ({ context, page }) => {
  await freshSession(context, page, 'search-album');

  await search(page, glassAtlas.title, 'ALBUM');

  const albumCard = page.getByText(glassAtlas.title).first();
  await expect(albumCard).toBeVisible({ timeout: RESULT_TIMEOUT });
  await albumCard.click();

  for (const track of glassAtlas.tracks) {
    await expect(page.getByText(track.title).first()).toBeVisible({ timeout: RESULT_TIMEOUT });
  }
});

test('shows the empty state when nothing matches', async ({ context, page }) => {
  await freshSession(context, page, 'search-empty');

  await search(page, 'definitely not in the catalog', 'TRACK');

  await expect(page.getByText('No signals found in the ether.')).toBeVisible({
    timeout: RESULT_TIMEOUT,
  });
});
