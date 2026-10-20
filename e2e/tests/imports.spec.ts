// The import queue end to end: one import at a time per library with
// waiting rows, repeat clicks that join the running download, a skipped
// import kept for a Retry, and a MusicBrainz outage reported as a failure.
// These tests script the MusicBrainz stub and reset the stubs, so the project
// runs serially after everything else. Each test downloads releases no other
// spec touches: the app's transfer registry outlives a stub reset.

import type { Page } from '@playwright/test';
import { expect, test } from '@playwright/test';
import { basename } from 'node:path';
import { freshSession, openDownloadsPanel, performSearch, transferRow } from '../helpers/app.js';
import { resetStubs, setMbMode, stubState } from '../helpers/control.js';
import { downloadsDirOnHost } from '../helpers/env.js';
import { expectFileAppears, importedTrackPath, listFilesRecursive } from '../helpers/files.js';
import {
  copperSeason,
  ferryLights,
  findPeer,
  quietRelay,
  saltRoads,
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

function downloadButton(page: Page, title: string) {
  return page
    .locator('li')
    .filter({ hasText: title })
    .first()
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
  await setMbMode('weak');
  const { folder } = await freshSession(context, page, 'imports-skip');

  await performSearch(page, vesperBells.title, 'TRACK');
  await downloadButton(page, vesperBells.title).click();
  await openDownloadsPanel(page);

  const [share] = sharesOf(vesperBells);
  const row = transferRow(page, share!.remotePath);
  await expect(row.getByText('SKIP', { exact: true })).toBeVisible({ timeout: 120_000 });
  await expect(row.getByText(/^No close match/)).toBeVisible();
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
