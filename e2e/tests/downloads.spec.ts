// The core pipeline, end to end: UI search -> auto source selection ->
// slskd stub delivers real FLACs -> the monitor picks them up -> real beets
// autotags against the MusicBrainz stub -> files land in the library tree.

import { expect, test } from '@playwright/test';
import { freshSession, openDownloadsPanel, performSearch, transferRow } from '../helpers/app.js';
import { expectFileAppears, importedTrackPath, listFilesRecursive } from '../helpers/files.js';
import { glassAtlas, senders } from '../fixtures/dataset.js';
import { basename } from 'node:path';
import { downloadsDirOnHost } from '../helpers/env.js';

test.describe.configure({ timeout: 180_000 });

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

  // beets moved (not copied) the file out of its batch folder, and slskd never
  // had to write a renamed {name}_{ticks} copy next to an existing one.
  const leftovers = listFilesRecursive(downloadsDirOnHost).filter((path) => {
    const name = basename(path);
    return name === '01 - Northern Line.flac' || /_\d{18}\./.test(name);
  });
  expect(leftovers).toEqual([]);
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
