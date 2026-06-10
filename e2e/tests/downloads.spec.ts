// The core pipeline, end to end: UI search -> auto source selection ->
// slskd stub delivers real FLACs -> the monitor picks them up -> real beets
// autotags against the MusicBrainz stub -> files land in the library tree.

import type { Page } from '@playwright/test';
import { expect, test } from '@playwright/test';
import { freshSession, performSearch } from '../helpers/app.js';
import { expectFileAppears, fileExists, importedTrackPath } from '../helpers/files.js';
import { glassAtlas, senders } from '../fixtures/dataset.js';
import { join } from 'node:path';
import { downloadsDirOnHost } from '../helpers/env.js';

test.describe.configure({ timeout: 180_000 });

async function openDownloadsPanel(page: Page) {
  await page.getByRole('button', { name: 'Downloads', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Active Transfers' })).toBeVisible();
}

/** The transfer row in the downloads panel, located by its remote path. */
function transferRow(page: Page, remotePath: string) {
  return page.locator('div.group').filter({ has: page.getByTitle(remotePath, { exact: true }) });
}

test('downloads and imports a single track', async ({ context, page }) => {
  const { folder } = await freshSession(context, page, 'happy-track');

  await performSearch(page, 'Northern Line', 'TRACK');

  const row = page.locator('li').filter({ hasText: 'Northern Line' }).first();
  await row.getByRole('button', { name: 'Download' }).click();

  await openDownloadsPanel(page);
  const item = transferRow(
    page,
    'Music\\FLAC\\Static Harbor\\Glass Atlas\\01 - Northern Line.flac',
  );
  await expect(item).toBeVisible({ timeout: 60_000 });

  // Badge walks the pipeline and ends at LIB (imported into the library).
  await expect(item.getByText('LIB', { exact: true })).toBeVisible({ timeout: 120_000 });

  const imported = importedTrackPath(
    folder.name,
    glassAtlas.artist,
    glassAtlas.title,
    'Northern Line',
    'flac',
  );
  await expectFileAppears(imported, 30_000);

  // beets moved (not copied) the file out of the downloads area.
  expect(fileExists(join(downloadsDirOnHost, 'Glass Atlas', '01 - Northern Line.flac'))).toBe(
    false,
  );
});

test('downloads and imports a full album from the album card', async ({ context, page }) => {
  const { folder } = await freshSession(context, page, 'happy-album');

  await performSearch(page, senders.title, 'ALBUM');

  const card = page.locator('li').filter({ hasText: senders.title }).first();
  await card.getByRole('button', { name: 'Download' }).click();

  for (const track of senders.tracks) {
    const imported = importedTrackPath(
      folder.name,
      senders.artist,
      senders.title,
      track.title,
      'flac',
    );
    await expectFileAppears(imported, 150_000);
  }
});
