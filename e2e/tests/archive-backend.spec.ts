// The Internet Archive download backend on the archive.org double: an admin turns it on,
// auto-download finds and imports an album only the archive has, a source the user picks by
// hand names the archive and links its page, a file archive.org refuses fails with its words,
// and turning it off takes it off the status line. It switches a backend for the whole
// instance, so the project runs alone, after every other one.

import type { Browser, BrowserContext, Page } from '@playwright/test';
import { expect, test } from '@playwright/test';
import {
  admin,
  freshSession,
  loginViaApi,
  openDownloadsPanel,
  openServiceStatus,
  performSearch,
  transferRow,
} from '../helpers/app.js';
import { disableBackend, enableBackend } from '../helpers/backends.js';
import { navidromeAdminPassword, navidromeAdminUser, navidromeEnabled } from '../helpers/env.js';
import { expectFileAppears, importedTrackPath } from '../helpers/files.js';
import {
  driftwoodCodesItem,
  pylonSongItem,
  wirelessOrchardsItem,
} from '../fixtures/archive-dataset.js';
import { driftwoodCodes, pylonSong, wirelessOrchards } from '../fixtures/dataset.js';

test.describe.configure({ mode: 'serial', timeout: 180_000 });

const ARCHIVE = 'Internet Archive';

/**
 * Signs `context` in as the seeded admin. Once navidrome.spec.ts has signed it in through
 * Navidrome, the account is linked to Navidrome's admin and only Navidrome's password opens it.
 */
async function signInAdmin(context: BrowserContext): Promise<void> {
  const local = await context.request.post('/api/auth/login', { data: admin });
  if (local.ok()) {
    return;
  }
  expect(navidromeEnabled, `admin sign-in: ${local.status()}`).toBe(true);
  await loginViaApi(context, { username: navidromeAdminUser, password: navidromeAdminPassword });
}

/** Runs `act` on a page of a new context signed in as the admin. */
async function asAdmin(browser: Browser, act: (page: Page) => Promise<void>): Promise<void> {
  const context = await browser.newContext();
  try {
    await signInAdmin(context);
    const page = await context.newPage();
    await page.goto('/');
    await act(page);
  } finally {
    await context.close();
  }
}

// A failed run must not leave the backend on for the next one.
test.afterAll(async ({ browser }) => {
  await asAdmin(browser, (page) => disableBackend(page, ARCHIVE));
});

test('an admin turns the archive on and the status line shows it online', async ({
  browser,
  context,
  page,
}) => {
  await asAdmin(browser, (adminPage) => enableBackend(adminPage, ARCHIVE));

  await freshSession(context, page, 'archive-on');
  await openServiceStatus(page);
  await expect(page.getByText('INTERNET ARCHIVE ONLINE', { exact: true })).toBeVisible({
    timeout: 30_000,
  });
  await expect(page.getByText('SOULSEEK ONLINE', { exact: true })).toBeVisible();
});

test('auto-download fetches an album only the archive has and imports it', async ({
  context,
  page,
}) => {
  const { folder } = await freshSession(context, page, 'archive-album');

  await performSearch(page, wirelessOrchards.title, 'ALBUM');
  const card = page.locator('li').filter({ hasText: wirelessOrchards.title }).first();
  await card.getByRole('button', { name: 'Download' }).click();

  await openDownloadsPanel(page);
  const first = wirelessOrchardsItem.files[0]!;
  await expect(transferRow(page, `${wirelessOrchardsItem.identifier}/${first.name}`)).toBeVisible({
    timeout: 60_000,
  });
  for (const track of wirelessOrchards.tracks) {
    const imported = importedTrackPath(
      folder.name,
      wirelessOrchards.artist,
      wirelessOrchards.title,
      track.title,
      'flac',
    );
    await expectFileAppears(imported, 150_000);
  }
});

test('a source picked by hand names the archive and links its page', async ({
  context,
  page,
}) => {
  const { folder } = await freshSession(context, page, 'archive-pick');

  await performSearch(page, driftwoodCodes.title, 'ALBUM');
  const card = page.locator('li').filter({ hasText: driftwoodCodes.title }).first();
  await card.getByRole('button', { name: 'Download' }).click();

  // The archive's low bitrate WMA scores under the automatic pick, as the Soulseek one does.
  await expect(page.getByText(`No confident match for ${driftwoodCodes.title}.`)).toBeVisible({
    timeout: 60_000,
  });
  await page.getByText('Pick a source manually').click();
  await expect(page.getByText('Download Options')).toBeVisible();

  const group = page
    .locator('div.rounded-md')
    .filter({ has: page.getByText(ARCHIVE, { exact: true }) })
    .last();
  await expect(group.getByText(ARCHIVE, { exact: true })).toBeVisible();
  // Only the archive's group links a source page.
  const sourcePage = page.getByRole('link', { name: 'Source page' });
  await expect(sourcePage).toHaveCount(1);
  await expect(sourcePage).toHaveAttribute(
    'href',
    `https://archive.org/details/${driftwoodCodesItem.identifier}`,
  );

  const selectAll = group.getByRole('button', { name: 'Select All' });
  if (!(await selectAll.isVisible())) {
    await group.getByText(ARCHIVE, { exact: true }).click();
  }
  await selectAll.click();
  await page.locator('select[name="dl_folder"]').selectOption({ label: folder.name });
  await page.getByRole('button', { name: 'Start download' }).click();

  await openDownloadsPanel(page);
  for (const file of driftwoodCodesItem.files) {
    await expect(transferRow(page, `${driftwoodCodesItem.identifier}/${file.name}`)).toBeVisible({
      timeout: 60_000,
    });
  }
});

test('a file archive.org refuses fails with its words', async ({ context, page }) => {
  await freshSession(context, page, 'archive-refused');
  const track = pylonSong.tracks[0]!;

  await performSearch(page, track.title, 'TRACK');
  const row = page.locator('li').filter({ hasText: track.title }).first();
  await row.getByRole('button', { name: 'Download' }).click();

  await openDownloadsPanel(page);
  const file = pylonSongItem.files[0]!;
  const item = transferRow(page, `${pylonSongItem.identifier}/${file.name}`);
  await expect(item.getByText('ERR', { exact: true })).toBeVisible({ timeout: 90_000 });
  await expect(item.getByText('archive.org refused this file (restricted)')).toBeVisible();
});

test('turning the archive off takes it off the status line', async ({
  browser,
  context,
  page,
}) => {
  await asAdmin(browser, (adminPage) => disableBackend(adminPage, ARCHIVE));

  await freshSession(context, page, 'archive-off');
  await openServiceStatus(page);
  await expect(page.getByText('SOULSEEK ONLINE', { exact: true })).toBeVisible({
    timeout: 30_000,
  });
  await expect(page.getByText(/^INTERNET ARCHIVE /)).toHaveCount(0);
});
