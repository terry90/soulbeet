// Unified single-box catalog search through the UI, exercising both the
// primary Deezer stub (canonical ranking, compilation deduplication, karaoke
// filtering, album tracklists, artist discographies, deep-catalog switch) and
// automatic MusicBrainz fallback.

import type { Locator, Page } from '@playwright/test';
import { expect, test } from '@playwright/test';
import { freshSession, performSearch } from '../helpers/app.js';
import {
  atticSignals,
  collectedDispatches,
  glassAtlas,
  liveAtTheAnnex,
  liveAtTheExchange,
  maritime,
  morseBloom,
  pylonSong,
  semaphoreGardens,
  wirelessOrchards,
} from '../fixtures/dataset.js';

// The app spaces MusicBrainz requests out (1 req/s by design), so fallback
// results can queue up behind other workers' searches.
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

function catalogSection(page: Page): Locator {
  return page.locator('section', {
    has: page.getByRole('heading', { name: 'Catalog', exact: true }),
  });
}

test('collapses compilation duplicates and strips karaoke spam for a single-box Deezer query', async ({
  context,
  page,
}) => {
  await freshSession(context, page, 'search-deezer-reaper');

  await performSearch(page, 'Blue oyster the reaper', 'ALL');

  const catalog = catalogSection(page);
  await expect(catalog.getByText('Deezer', { exact: true })).toBeVisible();
  await expect(page.getByText('Showing deeper catalog results from MusicBrainz')).toHaveCount(0);

  // One canonical studio track on Agents of Fortune, zero compilation duplicates,
  // and zero karaoke/tribute spam.
  const trackRows = catalog.locator('ul > li');
  await expect(trackRows).toHaveCount(1);
  await expect(trackRows.first()).toContainText("(Don't Fear) The Reaper");
  await expect(trackRows.first()).toContainText('Blue Öyster Cult');
  await expect(trackRows.first()).toContainText('Agents of Fortune');

  await expect(catalog.getByText('Greatest Hits')).toHaveCount(0);
  await expect(catalog.getByText('The Essential Blue Öyster Cult')).toHaveCount(0);
  await expect(catalog.getByText(/Karaoke/i)).toHaveCount(0);

  // Explicit deep-catalog action switches the same query over to MusicBrainz.
  const deepButton = catalog.getByRole('button', {
    name: 'Search deeper catalog (MusicBrainz)',
  });
  await expect(deepButton).toBeVisible();
  const deepDone = page.waitForResponse(
    (response) =>
      new URL(response.url()).pathname === '/api/metadata/search/unified' &&
      response.request().method() === 'POST',
    { timeout: RESULT_TIMEOUT },
  );
  await deepButton.click();
  await deepDone;
  await expect(catalog.getByText('MusicBrainz', { exact: true })).toBeVisible({
    timeout: RESULT_TIMEOUT,
  });
});

test('filters by category pill and opens Deezer album tracklists and artist discographies', async ({
  context,
  page,
}) => {
  await freshSession(context, page, 'search-deezer-drilldown');

  await performSearch(page, 'Blue oyster', 'ALL');

  const catalog = catalogSection(page);
  await expect(catalog.getByRole('heading', { name: 'Artists', exact: true })).toBeVisible();
  await expect(catalog.getByRole('heading', { name: 'Tracks', exact: true })).toBeVisible();
  await expect(catalog.getByRole('heading', { name: 'Albums', exact: true })).toBeVisible();
  await expect(catalog.getByText(/Tribute Band/i)).toHaveCount(0);

  // Tracks pill shows only tracks.
  await page.getByRole('button', { name: /^Tracks(?:\s+\d+)?$/ }).click();
  await expect(catalog.getByRole('heading', { name: 'Tracks', exact: true })).toBeVisible();
  await expect(catalog.getByRole('heading', { name: 'Albums', exact: true })).toHaveCount(0);
  await expect(catalog.getByRole('heading', { name: 'Artists', exact: true })).toHaveCount(0);

  // Albums pill shows albums and expands Agents of Fortune's tracklist from Deezer.
  await page.getByRole('button', { name: /^Albums(?:\s+\d+)?$/ }).click();
  await expect(catalog.getByRole('heading', { name: 'Albums', exact: true })).toBeVisible();
  await expect(catalog.getByRole('heading', { name: 'Tracks', exact: true })).toHaveCount(0);

  const albumRow = catalog.locator('ul > li').filter({ hasText: 'Agents of Fortune' }).first();
  await albumRow.getByText('Agents of Fortune', { exact: true }).click();
  for (const title of [
    "This Ain't the Summer of Love",
    'True Confessions',
    "(Don't Fear) The Reaper",
    'E.T.I. (Extra Terrestrial Intelligence)',
  ]) {
    await expect(albumRow.getByText(title, { exact: true })).toBeVisible({
      timeout: RESULT_TIMEOUT,
    });
  }

  // Artists pill opens the inline artist catalog with top tracks and releases.
  await page.getByRole('button', { name: /^Artists(?:\s+\d+)?$/ }).click();
  await expect(catalog.getByRole('heading', { name: 'Artists', exact: true })).toBeVisible();
  await catalog.getByRole('button', { name: /Blue Öyster Cult/ }).click();

  await expect(page.getByRole('heading', { name: 'Blue Öyster Cult', exact: true })).toBeVisible({
    timeout: RESULT_TIMEOUT,
  });
  await expect(page.getByRole('heading', { name: 'Top Tracks', exact: true })).toBeVisible();
  await expect(
    page.getByRole('heading', { name: 'Albums, EPs & Singles', exact: true }),
  ).toBeVisible();
  await expect(page.getByText('Agents of Fortune').first()).toBeVisible();
  await expect(page.getByText('Fire of Unknown Origin').first()).toBeVisible();

  await page.getByRole('button', { name: 'Back to results' }).click();
  await expect(catalog).toBeVisible();
});

test('falls back automatically to MusicBrainz and shows track metadata', async ({
  context,
  page,
}) => {
  await freshSession(context, page, 'search-track');

  await performSearch(page, 'Northern Line', 'TRACK');

  const catalog = catalogSection(page);
  await expect(catalog.getByText('MusicBrainz', { exact: true })).toBeVisible();
  await expect(
    catalog.getByText('Showing deeper catalog results from MusicBrainz'),
  ).toBeVisible();
  await expect(catalog.getByText('Northern Line', { exact: true })).toBeVisible();
  await expect(catalog.getByText(glassAtlas.artist).first()).toBeVisible();
  await expect(catalog.getByText(glassAtlas.title).first()).toBeVisible();
});

test('narrows a track search with artist and title in the single box', async ({ context, page }) => {
  await freshSession(context, page, 'search-artist');

  await performSearch(page, 'Maritime', 'TRACK', maritime.artist);

  const catalog = catalogSection(page);
  await expect(catalog.getByText(maritime.artist).first()).toBeVisible({ timeout: RESULT_TIMEOUT });
  await expect(catalog.getByText('Maritime', { exact: true }).first()).toBeVisible();
});

test('finds a MusicBrainz album and expands its tracklist', async ({ context, page }) => {
  await freshSession(context, page, 'search-album');

  await performSearch(page, glassAtlas.title, 'ALBUM');

  const albumRow = catalogSection(page)
    .locator('ul > li')
    .filter({ hasText: glassAtlas.title })
    .first();
  await expect(albumRow).toBeVisible({ timeout: RESULT_TIMEOUT });
  await albumRow.getByText(glassAtlas.title, { exact: true }).click();

  for (const track of glassAtlas.tracks) {
    await expect(albumRow.getByText(track.title, { exact: true })).toBeVisible({
      timeout: RESULT_TIMEOUT,
    });
  }
});

test('shows the empty state when nothing matches in either catalog', async ({ context, page }) => {
  await freshSession(context, page, 'search-empty');

  const missing = 'definitely not in the catalog';
  await performSearch(page, missing, 'ALL');

  await expect(catalogSection(page).getByText(`No catalog matches for "${missing}".`)).toBeVisible({
    timeout: RESULT_TIMEOUT,
  });
});

test('opens a MusicBrainz artist discography (studio first, singles last) and expands albums', async ({
  context,
  page,
}) => {
  await freshSession(context, page, 'search-discography');

  await performSearch(page, wirelessOrchards.artist, 'ARTIST');

  await catalogSection(page)
    .getByRole('button', { name: new RegExp(wirelessOrchards.artist) })
    .click();

  await expect(
    page.getByRole('heading', { name: wirelessOrchards.artist, exact: true }),
  ).toBeVisible({ timeout: RESULT_TIMEOUT });
  await expect(
    page.getByRole('heading', { name: 'Albums, EPs & Singles', exact: true }),
  ).toBeVisible();

  const releaseRows = page
    .locator('section ul')
    .filter({ hasText: wirelessOrchards.title })
    .locator('> li');
  await expect(releaseRows).toHaveCount(discography.length);
  await expect(releaseRows).toContainText(discography.map(({ album }) => album.title));
  await expect(releaseRows).toContainText(discography.map(({ facts }) => facts));
  await expect(page.getByText(liveAtTheAnnex.title)).toHaveCount(0);

  // Expands both an official release and one whose release has no status.
  for (const album of [wirelessOrchards, atticSignals]) {
    const albumRow = releaseRows.filter({ hasText: album.title }).first();
    await albumRow.getByText(album.title, { exact: true }).click();

    for (const track of album.tracks) {
      await expect(albumRow.getByText(track.title, { exact: true })).toBeVisible({
        timeout: RESULT_TIMEOUT,
      });
    }
  }
});
