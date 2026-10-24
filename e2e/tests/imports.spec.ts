// The import queue end to end: one import at a time per library with
// waiting rows, repeat clicks that join the running download, a skipped
// import kept for a Retry, an import with a match picked from what beets
// found, a typed MusicBrainz link or the file's own tags, and a MusicBrainz
// outage reported as a failure.
// These tests script the MusicBrainz stub and reset the stubs, so the project
// runs serially after everything else. Each test downloads releases no other
// spec touches: the app's transfer registry outlives a stub reset.

import type { BrowserContext, Locator, Page } from '@playwright/test';
import { expect, test } from '@playwright/test';
import { basename } from 'node:path';
import { freshSession, openDownloadsPanel, performSearch, transferRow } from '../helpers/app.js';
import { mbRequests, resetStubs, setMbMode, stubState } from '../helpers/control.js';
import { downloadsDirOnHost } from '../helpers/env.js';
import {
  expectFileAppears,
  importedTrackPath,
  libraryItems,
  listFilesRecursive,
} from '../helpers/files.js';
import { shot } from '../helpers/shots.js';
import { weakTitle } from '../stubs/src/mb-server.js';
import {
  ashLullaby,
  bellwether,
  cinderPsalm,
  cinderPsalmChoir,
  copperSeason,
  ferryLights,
  findPeer,
  quietRelay,
  saltRoads,
  thimbleHymn,
  tinMoons,
  vesperBells,
  type FixtureRelease,
  type PeerShare,
} from '../fixtures/dataset.js';

test.describe.configure({ timeout: 240_000 });

test.afterEach(async () => {
  await resetStubs();
});

/** The release's files on collector_01, the source auto-select picks. */
function sharesOf(release: FixtureRelease): PeerShare[] {
  const mbids = new Set(release.tracks.map((track) => track.mbid));
  return (findPeer('collector_01')?.shares ?? []).filter((share) =>
    mbids.has(share.recordingMbid),
  );
}

/**
 * The Download button of the search result titled exactly `title`: "Cinder
 * Psalm" is also part of the MB-only "Cinder Psalm (Choir Version)", which no
 * peer shares.
 */
function downloadButton(page: Page, title: string) {
  return page
    .locator('li')
    .filter({ has: page.getByText(title, { exact: true }) })
    .getByRole('button', { name: 'Download' });
}

function drawer(page: Page) {
  return page.locator('div.bg-beet-panel').filter({ hasText: 'Active Transfers' });
}

async function expectAllInLibrary(page: Page, release: FixtureRelease, folderName: string) {
  for (const share of sharesOf(release)) {
    await expect(transferRow(page, share.remotePath).getByText('LIB', { exact: true })).toBeVisible(
      { timeout: 180_000 },
    );
  }
  for (const track of release.tracks) {
    await expectFileAppears(
      importedTrackPath(folderName, release.artist, release.title, track.title, 'flac'),
      30_000,
    );
  }
}

/**
 * Downloads the single's track with MusicBrainz weak for beets and waits for
 * its row to end SKIP with "No close match". MusicBrainz stays weak, so a
 * later import can only go through by the choice the test makes.
 */
async function skippedRow(
  context: BrowserContext,
  page: Page,
  release: FixtureRelease,
  folderName: string,
) {
  await setMbMode('weak');
  const { folder } = await freshSession(context, page, folderName);

  await performSearch(page, release.title, 'TRACK');
  await downloadButton(page, release.tracks[0]!.title).click();
  await openDownloadsPanel(page);

  const [share] = sharesOf(release);
  const row = transferRow(page, share!.remotePath);
  await expect(row.getByText('SKIP', { exact: true })).toBeVisible({ timeout: 120_000 });
  await expect(row.getByText(/^No close match/)).toBeVisible();
  return { folder, row };
}

/** Retry, Import as-is and the matches are offered together, or none of them. */
async function expectImportActions(row: Locator, offered: boolean) {
  const count = offered ? 1 : 0;
  await expect(row.getByRole('button', { name: 'Retry import' })).toHaveCount(count);
  await expect(row.getByRole('button', { name: 'Import as-is' })).toHaveCount(count);
  await expect(row.getByRole('button', { name: /^(Show|Hide) matches/ })).toHaveCount(count);
}

/** The paths beets asked MusicBrainz for after the first `from`. */
async function mbPathsSince(from: number): Promise<string[]> {
  return (await mbRequests()).slice(from);
}

test('every row of a singleton batch ends at LIB', async ({ context, page }) => {
  const { folder } = await freshSession(context, page, 'imports-singleton');

  await performSearch(page, copperSeason.title, 'ALBUM');
  await downloadButton(page, copperSeason.title).click();
  await openDownloadsPanel(page);

  await expectAllInLibrary(page, copperSeason, folder.name);
});

test('two albums to one library show WAIT then LIB', async ({ context, page }) => {
  // Every beets lookup takes 3 s, so the imports queue up behind each other.
  await setMbMode('normal', 3000);
  const { folder } = await freshSession(context, page, 'imports-wait');

  await performSearch(page, ferryLights.title, 'ALBUM');
  await downloadButton(page, ferryLights.title).click();
  await performSearch(page, saltRoads.title, 'ALBUM');
  await downloadButton(page, saltRoads.title).click();
  await openDownloadsPanel(page);

  const waiting = drawer(page)
    .locator('div.group')
    .filter({ hasText: 'Waiting for library (1 ahead)' })
    .filter({ has: page.getByText('WAIT', { exact: true }) });
  await expect(waiting.first()).toBeVisible({ timeout: 120_000 });

  await expectAllInLibrary(page, ferryLights, folder.name);
  await expectAllInLibrary(page, saltRoads, folder.name);
});

test('rapid re-clicks give one transfer and no tick copies', async ({ context, page }) => {
  const { folder } = await freshSession(context, page, 'imports-reclick');

  await performSearch(page, tinMoons.title, 'ALBUM');
  let requests = 0;
  page.on('request', (request) => {
    if (request.method() === 'POST' && request.url().endsWith('/api/auto-download')) {
      requests++;
    }
  });
  // The icon disables itself once it re-renders, so a user's fast clicks are
  // dispatched in one go: each reaches the handler and sends a request.
  await downloadButton(page, tinMoons.title).evaluate((button: HTMLElement) => {
    button.click();
    button.click();
    button.click();
  });
  await expect.poll(() => requests).toBe(3);
  await openDownloadsPanel(page);
  await expectAllInLibrary(page, tinMoons, folder.name);

  // slskd itself refuses a second enqueue of an unfinished file, so one
  // transfer per file alone would not show the app joined the repeats: it
  // must never have asked slskd twice.
  const { rejections, transfers } = await stubState();
  expect(rejections).toEqual({ alreadyInProgress: 0, throttled: 0 });
  for (const share of sharesOf(tinMoons)) {
    expect(
      transfers.filter((t) => t.filename === share.remotePath),
      share.remotePath,
    ).toHaveLength(1);
  }
  const tickCopies = listFilesRecursive(downloadsDirOnHost).filter((path) =>
    /_\d{18}\./.test(basename(path)),
  );
  expect(tickCopies).toEqual([]);
});

test('a skipped row shows the reason, the kept folder and the beets output', async ({
  context,
  page,
}) => {
  const { folder, row } = await skippedRow(context, page, vesperBells, 'imports-skip');
  // A track search names its batch folder after the track.
  await expect(row.getByText(/^Kept in .*Vesper Bells \[[0-9a-z]{8}\]/)).toBeVisible();
  await row.getByRole('button', { name: 'Show beets output' }).click();
  await expect(row.locator('pre')).not.toBeEmpty();

  await setMbMode('normal');
  await row.getByRole('button', { name: 'Retry import' }).click();
  await expect(row.getByText('LIB', { exact: true })).toBeVisible({ timeout: 120_000 });
  await expectFileAppears(
    importedTrackPath(
      folder.name,
      vesperBells.artist,
      vesperBells.title,
      vesperBells.tracks[0]!.title,
      'flac',
    ),
    30_000,
  );
});

test('a skipped row imports with the match picked from what beets found', async ({
  context,
  page,
}) => {
  const { folder, row } = await skippedRow(context, page, cinderPsalm, 'imports-pick');
  await expect(row.getByText('No close match (2 candidates)', { exact: true })).toBeVisible();
  await expectImportActions(row, true);
  await shot(row, '01-skipped-row');

  await row.getByRole('button', { name: 'Show matches' }).click();
  await expect(row.getByRole('button', { name: 'Hide matches (2)' })).toBeVisible();
  await expect(row.getByRole('radio')).toHaveCount(2);
  const own = `${cinderPsalm.artist} - ${weakTitle(cinderPsalm.tracks[0]!.title)}`;
  await expect(row.getByRole('radio', { name: own })).toBeVisible();
  // The other match, a weaker fit than the file's own recording.
  const picked = cinderPsalmChoir.tracks[0]!;
  const pickedTitle = weakTitle(picked.title);
  const option = `${cinderPsalmChoir.artist} - ${pickedTitle}`;
  await row.getByText(option, { exact: true }).click();
  await expect(row.getByRole('radio', { name: option })).toBeChecked();
  await shot(row, '02-picker-open');

  // The picker fits a phone without scrolling sideways, and so do the
  // drawer's list and the page.
  await page.setViewportSize({ width: 412, height: 839 });
  await expect.poll(() => row.evaluate((el) => el.scrollWidth - el.clientWidth)).toBe(0);
  const list = drawer(page).locator(':scope > div.overflow-y-auto');
  await expect.poll(() => list.evaluate((el) => el.scrollWidth - el.clientWidth)).toBe(0);
  await expect
    .poll(() => page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth))
    .toBeLessThanOrEqual(0);
  const box = await row.boundingBox();
  expect(box!.x + box!.width).toBeLessThanOrEqual(412);
  await shot(drawer(page), '03-picker-phone');
  await page.setViewportSize({ width: 1280, height: 720 });

  const asked = (await mbRequests()).length;
  await row.getByRole('button', { name: 'Import with this match' }).click();
  await expect(row.getByText('LIB', { exact: true })).toBeVisible({ timeout: 120_000 });
  await expectImportActions(row, false);
  await shot(row, '04-picked-row');

  await expectFileAppears(
    importedTrackPath(folder.name, cinderPsalm.artist, cinderPsalm.title, pickedTitle, 'flac'),
    30_000,
  );
  expect(libraryItems(folder.name)).toEqual([{ title: pickedTitle, mbTrackId: picked.mbid }]);
  // beets looked up the picked recording and nothing else.
  expect(await mbPathsSince(asked)).toEqual([`/ws/2/recording/${picked.mbid}`]);
});

test('a skipped row imports as-is with its own tags, without asking MusicBrainz', async ({
  context,
  page,
}) => {
  const { folder, row } = await skippedRow(context, page, ashLullaby, 'imports-asis');
  const track = ashLullaby.tracks[0]!;

  const asked = (await mbRequests()).length;
  await row.getByRole('button', { name: 'Import as-is' }).click();
  await expect(row.getByText('LIB', { exact: true })).toBeVisible({ timeout: 120_000 });
  await expect(row.getByText('Imported as-is (untagged)', { exact: true })).toBeVisible();
  await expectImportActions(row, false);
  await shot(row, '05-as-is-row');

  await expectFileAppears(
    importedTrackPath(folder.name, ashLullaby.artist, ashLullaby.title, track.title, 'flac'),
    30_000,
  );
  expect(libraryItems(folder.name)).toEqual([{ title: track.title, mbTrackId: '' }]);
  expect(await mbPathsSince(asked)).toEqual([]);
  // beets moved the file in, so nothing of it is left in the downloads.
  const left = listFilesRecursive(downloadsDirOnHost).filter((path) =>
    basename(path).includes(track.title),
  );
  expect(left).toEqual([]);
});

test('a typed MusicBrainz recording link imports with it, and a release link is refused', async ({
  context,
  page,
}) => {
  const { folder, row } = await skippedRow(context, page, thimbleHymn, 'imports-link');
  await row.getByRole('button', { name: 'Show matches' }).click();
  // beets did not find the recording the link names.
  const linked = bellwether.tracks[0]!;
  const linkedTitle = weakTitle(linked.title);
  await expect(row.getByRole('radio')).toHaveCount(1);
  await expect(row.getByRole('radio', { name: linkedTitle })).toHaveCount(0);

  const field = row.getByRole('textbox', { name: 'MusicBrainz recording link or ID' });
  const importButton = row.getByRole('button', { name: 'Import with this match' });
  const asked = (await mbRequests()).length;
  await field.fill(`https://musicbrainz.org/release/${thimbleHymn.releaseMbid}`);
  await importButton.click();
  // The refusal shows in the picker, and the skip reason stays.
  await expect(row.getByRole('status')).toHaveText(
    'Track imports need a MusicBrainz recording, not a release',
  );
  await expect(row.getByText(/^No close match/)).toBeVisible();
  await expect(row.getByText('SKIP', { exact: true })).toBeVisible();
  expect(await mbPathsSince(asked)).toEqual([]);

  // The refusal leaves the picker as it was, to correct the link.
  await field.fill(`https://musicbrainz.org/recording/${linked.mbid}`);
  await importButton.click();
  await expect(row.getByText('LIB', { exact: true })).toBeVisible({ timeout: 120_000 });
  await expectFileAppears(
    importedTrackPath(folder.name, thimbleHymn.artist, thimbleHymn.title, linkedTitle, 'flac'),
    30_000,
  );
  expect(libraryItems(folder.name)).toEqual([{ title: linkedTitle, mbTrackId: linked.mbid }]);
});

test('a lookup outage is reported as a failure, not a skip', async ({ context, page }) => {
  // beets retries a 503 for about 30 s before it gives up on the lookup.
  await setMbMode('unavailable');
  await freshSession(context, page, 'imports-outage');

  await performSearch(page, quietRelay.title, 'TRACK');
  await downloadButton(page, quietRelay.title).click();
  await openDownloadsPanel(page);

  const [share] = sharesOf(quietRelay);
  const row = transferRow(page, share!.remotePath);
  await expect(row.getByText('ERR', { exact: true })).toBeVisible({ timeout: 150_000 });
  await expect(row.getByText(/^Lookup failed in MusicBrainz/)).toBeVisible();
});
