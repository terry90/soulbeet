// The GD Studio download backend on its double: an admin turns it on, auto-download finds and
// imports an album only NetEase has under its NetEase names, a file whose MD5 is not the one it
// is named by fails, an outage takes its status line offline while Soulseek still downloads,
// and once it is off it gets no more calls. It switches a backend for the whole instance and
// scripts the double, so the project runs alone, after YouTube Music's.

import { expect, test } from '@playwright/test';
import {
  freshSession,
  openDownloadsPanel,
  openServiceStatus,
  performSearch,
  transferRow,
} from '../helpers/app.js';
import { asAdmin, disableBackend, enableBackend } from '../helpers/backends.js';
import {
  gdstudioCalls,
  resetGdStudio,
  setGdStudioMode,
  setNetEaseTrack,
} from '../helpers/control.js';
import { expectFileAppears, importedTrackPath } from '../helpers/files.js';
import { glassAtlas, morseBloom, semaphoreGardens } from '../fixtures/dataset.js';
import {
  landingName,
  morseBloomTracks,
  semaphoreGardensTracks,
} from '../fixtures/gdstudio-dataset.js';

test.describe.configure({ mode: 'serial', timeout: 180_000 });

const GD = 'NetEase (GD Studio)';
const LINE = /^NETEASE \(GD STUDIO\) /;

test.beforeAll(async () => {
  await resetGdStudio();
});

// A failed run must not leave the backend on, or the double scripted, for the next one.
test.afterAll(async ({ browser }) => {
  await setGdStudioMode('up');
  await asAdmin(browser, (page) => disableBackend(page, GD));
});

/** Downloads Glass Atlas's "Northern Line" from Soulseek into a new user's folder. */
async function soulseekDownload(
  context: Parameters<typeof freshSession>[0],
  page: Parameters<typeof freshSession>[1],
  folderName: string,
): Promise<void> {
  const { folder } = await freshSession(context, page, folderName);
  await performSearch(page, 'Northern Line', 'TRACK');
  const row = page.locator('li').filter({ hasText: 'Northern Line' }).first();
  await row.getByRole('button', { name: 'Download' }).click();
  await expectFileAppears(
    importedTrackPath(folder.name, glassAtlas.artist, glassAtlas.title, 'Northern Line', 'flac'),
    150_000,
  );
}

test('an admin turns NetEase on after the confirm and the status line shows it online', async ({
  browser,
  context,
  page,
}) => {
  await asAdmin(browser, (adminPage) => enableBackend(adminPage, GD));

  await freshSession(context, page, 'gdstudio-on');
  await openServiceStatus(page);
  await expect(page.getByText('NETEASE (GD STUDIO) ONLINE', { exact: true })).toBeVisible({
    timeout: 30_000,
  });
  await expect(page.getByText('SOULSEEK ONLINE', { exact: true })).toBeVisible();
});

test('auto-download fetches an album only NetEase has and imports it', async ({
  context,
  page,
}) => {
  const { folder } = await freshSession(context, page, 'gdstudio-album');

  await performSearch(page, semaphoreGardens.title, 'ALBUM');
  const card = page.locator('li').filter({ hasText: semaphoreGardens.title }).first();
  await card.getByRole('button', { name: 'Download' }).click();

  await openDownloadsPanel(page);
  for (const [i, track] of semaphoreGardensTracks.entries()) {
    await expect(transferRow(page, `${track.id}/${landingName(track, i + 1)}`)).toBeVisible({
      timeout: 60_000,
    });
  }
  for (const track of semaphoreGardens.tracks) {
    const imported = importedTrackPath(
      folder.name,
      semaphoreGardens.artist,
      semaphoreGardens.title,
      track.title,
      'flac',
    );
    await expectFileAppears(imported, 150_000);
  }
});

test('a file that does not match the MD5 it is named by fails and is not kept', async ({
  context,
  page,
}) => {
  const track = morseBloomTracks[0]!;
  await setNetEaseTrack(track.id, 'wrongmd5');
  try {
    await freshSession(context, page, 'gdstudio-md5');
    await performSearch(page, morseBloom.tracks[0]!.title, 'TRACK');
    const row = page.locator('li').filter({ hasText: track.name }).first();
    await row.getByRole('button', { name: 'Download' }).click();

    await openDownloadsPanel(page);
    const item = transferRow(page, `${track.id}/${track.name} [${track.id}].${track.format}`);
    await expect(item.getByText('ERR', { exact: true })).toBeVisible({ timeout: 90_000 });
    await expect(
      item.getByText('The file NetEase sent does not match the MD5 it is named by'),
    ).toBeVisible();
  } finally {
    await setNetEaseTrack(track.id, 'normal');
  }
});

test('with GD Studio down its line reads offline while Soulseek still downloads', async ({
  context,
  page,
}) => {
  await setGdStudioMode('outage');
  try {
    // Auto-download asks GD Studio too, which answers 503.
    await soulseekDownload(context, page, 'gdstudio-outage');
    await page.reload();
    await openServiceStatus(page);
    await expect(page.getByText('NETEASE (GD STUDIO) OFFLINE', { exact: true })).toBeVisible({
      timeout: 30_000,
    });
    await expect(page.getByText('SOULSEEK ONLINE', { exact: true })).toBeVisible();
  } finally {
    await setGdStudioMode('up');
  }
});

test('turned off, NetEase leaves the status line and gets no more calls', async ({
  browser,
  context,
  page,
}) => {
  await asAdmin(browser, (adminPage) => disableBackend(adminPage, GD));
  const before = await gdstudioCalls();

  await soulseekDownload(context, page, 'gdstudio-off');
  await page.reload();
  await openServiceStatus(page);
  await expect(page.getByText('SOULSEEK ONLINE', { exact: true })).toBeVisible({
    timeout: 30_000,
  });
  await expect(page.getByText(LINE)).toHaveCount(0);
  expect((await gdstudioCalls()).total).toBe(before.total);
});

test('GD Studio never got more than 45 calls in 5 minutes', async () => {
  const calls = await gdstudioCalls();
  expect(calls.total).toBeGreaterThan(0);
  expect(calls.maxInWindow).toBeLessThanOrEqual(45);
});
