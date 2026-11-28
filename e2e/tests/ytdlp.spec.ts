// The YouTube Music download backend on fake yt-dlp and ytmusicapi packages: an admin turns it
// on, an album no Soulseek peer has is offered under its own lossy heading and imports once the
// user picks it, a user who never takes lossy files is not offered it, and a rate limit shows on
// the status line. It switches a backend for the whole instance and ends with YouTube Music
// paused for an hour, so the project runs alone, after the archive.

import type { Page } from '@playwright/test';
import { expect, test } from '@playwright/test';
import {
  freshSession,
  openDownloadsPanel,
  openLibraryTab,
  openServiceStatus,
  openSettings,
  performSearch,
  transferRow,
} from '../helpers/app.js';
import { asAdmin, disableBackend, enableBackend } from '../helpers/backends.js';
import { expectFileAppears, importedTrackPath, libraryItems } from '../helpers/files.js';
import { morseBloom, semaphoreGardens } from '../fixtures/dataset.js';
import { landedName, morseBloomAlbum, rateLimitedRelease } from '../fixtures/ytdlp-dataset.js';

test.describe.configure({ mode: 'serial', timeout: 180_000 });

const YOUTUBE = 'YouTube Music';

/** Starts the auto-download of the album `title` from the search results. */
async function autoDownload(page: Page, title: string): Promise<void> {
  await performSearch(page, title, 'ALBUM');
  const card = page.locator('li').filter({ hasText: title }).first();
  await card.getByRole('button', { name: 'Download' }).click();
}

// A failed run must not leave the backend on.
test.afterAll(async ({ browser }) => {
  await asAdmin(browser, (page) => disableBackend(page, YOUTUBE));
});

test('an admin turns YouTube Music on and the status line shows it online and lossy', async ({
  browser,
  context,
  page,
}) => {
  await asAdmin(browser, (adminPage) => enableBackend(adminPage, YOUTUBE));

  await freshSession(context, page, 'ytdlp-on');
  await openServiceStatus(page);
  await expect(page.getByText('YOUTUBE MUSIC ONLINE · LOSSY', { exact: true })).toBeVisible({
    timeout: 30_000,
  });
  await expect(page.getByText('SOULSEEK ONLINE', { exact: true })).toBeVisible();
});

test('an album only YouTube Music has is offered as lossy, and the pick imports', async ({
  context,
  page,
}) => {
  const { folder } = await freshSession(context, page, 'ytdlp-pick');

  await autoDownload(page, morseBloom.title);

  // Never picked on its own for a user who is asked first, which every user is until they
  // choose otherwise. The match is confident, so the toast says why it was not picked.
  await expect(
    page.getByText(`Only ${YOUTUBE} has ${morseBloom.title}, and its files are lossy.`),
  ).toBeVisible({ timeout: 90_000 });
  await page.getByText('Pick a source manually').click();
  await expect(page.getByText('Download Options')).toBeVisible();
  await expect(page.getByText(`Lossy, from ${YOUTUBE}`, { exact: true })).toBeVisible();

  const group = page
    .locator('div.rounded-md')
    .filter({ has: page.getByText('LOSSY · YOUTUBE MUSIC', { exact: true }) })
    .last();
  const selectAll = group.getByRole('button', { name: 'Select All' });
  if (!(await selectAll.isVisible())) {
    await group.getByText(morseBloomAlbum.title, { exact: true }).first().click();
  }
  await selectAll.click();
  await page.getByRole('button', { name: 'Start download' }).click();

  await openDownloadsPanel(page);
  for (const track of morseBloomAlbum.tracks) {
    const row = transferRow(page, landedName(morseBloomAlbum, track));
    await expect(row).toBeVisible({ timeout: 60_000 });
    await expect(row.getByText('LOSSY · YOUTUBE MUSIC', { exact: true })).toBeVisible();
  }
  for (const track of morseBloom.tracks) {
    const imported = importedTrackPath(
      folder.name,
      morseBloom.artist,
      morseBloom.title,
      track.title,
      'opus',
    );
    await expectFileAppears(imported, 150_000);
  }
  // beets matched each file to the recording it was fetched for.
  const matched = libraryItems(folder.name).map((item) => item.mbTrackId).sort();
  expect(matched).toEqual(morseBloom.tracks.map((track) => track.mbid).sort());
});

test('a user who never takes lossy files is not offered them', async ({ context, page }) => {
  await freshSession(context, page, 'ytdlp-never');
  await openSettings(page);
  await openLibraryTab(page);
  const choice = page.locator('select[name="lossy_fallback"]');
  await expect(choice).toBeVisible();
  const saved = page.waitForResponse(
    (r) => new URL(r.url()).pathname === '/api/settings' && r.request().method() === 'POST',
  );
  await choice.selectOption({ label: 'Never use it' });
  expect((await saved).ok()).toBeTruthy();

  await page.getByRole('link', { name: 'Search' }).click();
  await autoDownload(page, semaphoreGardens.title);

  // Soulseek has no peer for it, and YouTube Music is not asked.
  const card = page.locator('li').filter({ hasText: semaphoreGardens.title }).first();
  await expect(card.getByTitle('No results found')).toBeVisible({ timeout: 90_000 });
  await expect(page.getByText(`Lossy, from ${YOUTUBE}`)).toHaveCount(0);
});

test('a rate limit from YouTube shows on the status line', async ({ context, page }) => {
  await freshSession(context, page, 'ytdlp-limited');

  // YouTube Music answers the search for this album with a rate limit.
  await autoDownload(page, rateLimitedRelease.title);
  const card = page.locator('li').filter({ hasText: rateLimitedRelease.title }).first();
  await expect(card.getByTitle('No results found')).toBeVisible({ timeout: 90_000 });

  await openServiceStatus(page);
  await expect(
    page.getByText('YOUTUBE MUSIC RATE-LIMITED · LOSSY', { exact: true }),
  ).toBeVisible({ timeout: 30_000 });
  await expect(
    page.getByText(/^YouTube rate-limited this server until \d\d:\d\d UTC$/).first(),
  ).toBeVisible();
});
