// Metadata search through the UI, served by the MusicBrainz stub.

import type { Page } from '@playwright/test';
import { expect, test } from '@playwright/test';
import { freshSession } from '../helpers/app.js';
import {
  atticSignals,
  collectedDispatches,
  glassAtlas,
  liveAtTheExchange,
  maritime,
  morseBloom,
  pylonSong,
  semaphoreGardens,
  wirelessOrchards,
} from '../fixtures/dataset.js';

async function search(page: Page, query: string, type?: 'ALBUM' | 'TRACK') {
  if (type) {
    await page.getByRole('radio', { name: type }).check();
  }
  await page.getByRole('searchbox').fill(query);
  await page.getByRole('button', { name: 'Search', exact: true }).click();
}

async function searchByArtist(page: Page, artist: string, query: string, type: 'ALBUM' | 'TRACK') {
  await page.getByRole('button', { name: 'Add artist filter' }).click();
  await page.getByPlaceholder('Artist name').fill(artist);
  await search(page, query, type);
}

// The app spaces MusicBrainz requests out (1 req/s by design), so results
// can queue up behind other workers' searches.
const RESULT_TIMEOUT = 45_000;

// Orchid Telegraph's releases as the app lists them: studio albums, then
// studio EPs, then compilations before live albums before demos, then
// singles, each oldest first. The demo's release has no status; its bootleg
// is left out.
const discography = [
  { album: wirelessOrchards, facts: '2014 · Album' },
  { album: semaphoreGardens, facts: '2017 · Album' },
  { album: morseBloom, facts: '2015 · EP' },
  { album: collectedDispatches, facts: '2020 · Album · Compilation' },
  { album: liveAtTheExchange, facts: '2018 · Album · Live' },
  { album: atticSignals, facts: '2012 · Album · Demo' },
  { album: pylonSong, facts: '2019 · Single' },
];

/** The rows under "Albums by Orchid Telegraph": `discography`, in order. */
async function expectDiscography(page: Page) {
  const heading = page.getByRole('heading', { name: `Albums by ${wirelessOrchards.artist}` });
  await expect(heading).toBeVisible({ timeout: RESULT_TIMEOUT });
  const rows = page
    .getByRole('list')
    .filter({ hasText: wirelessOrchards.title })
    .getByRole('listitem');
  await expect(rows).toHaveCount(discography.length);
  await expect(rows).toContainText(discography.map(({ album }) => album.title));
  await expect(rows).toContainText(discography.map(({ facts }) => facts));
}

test('finds a track with artist and album metadata', async ({ context, page }) => {
  await freshSession(context, page, 'search-track');

  await search(page, 'Northern Line', 'TRACK');

  await expect(page.getByText('Search Results')).toBeVisible({ timeout: RESULT_TIMEOUT });
  await expect(page.getByText('Northern Line')).toBeVisible();
  await expect(page.getByText(glassAtlas.artist).first()).toBeVisible();
});

test('narrows a track search with the artist field', async ({ context, page }) => {
  await freshSession(context, page, 'search-artist');

  await page.getByRole('button', { name: 'Add artist filter' }).click();
  await page.getByPlaceholder('Artist name').fill(maritime.artist);
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

test('lists the releases of an artist given alone, studio albums first, singles last', async ({
  context,
  page,
}) => {
  await freshSession(context, page, 'search-discography');

  await searchByArtist(page, wirelessOrchards.artist, '', 'ALBUM');

  await expectDiscography(page);
});

test('lists the same albums for a title of "*", in a track search too', async ({
  context,
  page,
}) => {
  await freshSession(context, page, 'search-discography-star');

  await searchByArtist(page, wirelessOrchards.artist, '*', 'TRACK');

  await expectDiscography(page);
});

test('says when no artist carries the name', async ({ context, page }) => {
  await freshSession(context, page, 'search-discography-nobody');

  await searchByArtist(page, 'Nobody Plays Here', '', 'ALBUM');

  await expect(page.getByText('No artist named “Nobody Plays Here” on MusicBrainz.')).toBeVisible({
    timeout: RESULT_TIMEOUT,
  });
});

test('expands the tracklist of a listed album, one whose release has no status too', async ({
  context,
  page,
}) => {
  await freshSession(context, page, 'search-discography-album');

  await searchByArtist(page, wirelessOrchards.artist, '', 'ALBUM');

  for (const album of [wirelessOrchards, atticSignals]) {
    const albumCard = page.getByText(album.title).first();
    await expect(albumCard).toBeVisible({ timeout: RESULT_TIMEOUT });
    await albumCard.click();

    for (const track of album.tracks) {
      await expect(page.getByText(track.title).first()).toBeVisible({ timeout: RESULT_TIMEOUT });
    }
  }
});
