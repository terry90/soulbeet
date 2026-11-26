// The Direct Connect download backend on the AirDC++ double: an admin gives the sidecar's
// address and login and turns it on, auto-download imports an album only Direct Connect peers
// have, a file only a passive peer has is not offered to a passive AirDC++, a peer gone at queue
// time is replaced by the next one, a cancel removes the bundle and keeps finished files, and
// turning it off stops the searches. It switches a backend for the whole instance, so the
// project runs alone, after the GD Studio one.

import { existsSync } from 'node:fs';
import { join, relative } from 'node:path';
import type { Page } from '@playwright/test';
import { expect, test } from '@playwright/test';
import {
  freshSession,
  openConfigTab,
  openDownloadsPanel,
  openServiceStatus,
  openSettings,
  performSearch,
  transferRow,
} from '../helpers/app.js';
import { asAdmin, disableBackend, enableBackend } from '../helpers/backends.js';
import { airdcppState, resetAirdcpp } from '../helpers/control.js';
import { airdcppUrlForApp, downloadsDirOnHost } from '../helpers/env.js';
import { expectFileAppears, importedTrackPath } from '../helpers/files.js';
import {
  airdcppPassword,
  airdcppUser,
  atticPeer,
  lighthousePeer,
  relayPeer,
} from '../fixtures/airdcpp-dataset.js';
import {
  atticSignals,
  collectedDispatches,
  liveAtTheExchange,
  semaphoreGardens,
} from '../fixtures/dataset.js';

test.describe.configure({ mode: 'serial', timeout: 240_000 });

const DC = 'Direct Connect';

/** Gives the sidecar's address and login in Settings > Config, saves them and checks them. */
async function setUpSidecar(page: Page): Promise<void> {
  await openSettings(page);
  await openConfigTab(page);
  await page.getByLabel('AirDC++ address').fill(airdcppUrlForApp);
  await page.getByLabel('AirDC++ user name').fill(airdcppUser);
  await page.getByLabel('AirDC++ password').fill(airdcppPassword);
  await page.getByRole('button', { name: 'Save Direct Connect' }).click();
  await expect(page.getByText('Direct Connect settings saved')).toBeVisible();
  await expect(page.getByLabel('AirDC++ password')).toHaveValue('');
  await expect(page.getByLabel('AirDC++ password')).toHaveAttribute(
    'placeholder',
    'Stored. Type a new one to change it',
  );
  await page.getByRole('button', { name: 'Check AirDC++' }).click();
  await expect(page.getByText('Soulbeet can use this AirDC++')).toBeVisible({ timeout: 30_000 });
  await expect(page.getByText('1 of 1 connected')).toBeVisible();
}

/** The host path of a file the double names by its own path in the shared downloads folder. */
function onHost(target: string, root: string): string {
  return join(downloadsDirOnHost, relative(root, target));
}

// A failed run must not leave the backend on for the next one.
test.afterAll(async ({ browser }) => {
  await asAdmin(browser, (page) => disableBackend(page, DC));
  await resetAirdcpp();
});

test('an admin sets up AirDC++, turns Direct Connect on and the status line shows it online', async ({
  browser,
  context,
  page,
}) => {
  await resetAirdcpp();
  await asAdmin(browser, async (adminPage) => {
    await setUpSidecar(adminPage);
    await enableBackend(adminPage, DC);
  });

  await freshSession(context, page, 'dc-on');
  await openServiceStatus(page);
  await expect(page.getByText('DIRECT CONNECT ONLINE', { exact: true })).toBeVisible({
    timeout: 30_000,
  });
  await expect(page.getByText('SOULSEEK ONLINE', { exact: true })).toBeVisible();
});

test('auto-download fetches an album only Direct Connect peers have and imports it', async ({
  context,
  page,
}) => {
  const { folder } = await freshSession(context, page, 'dc-album');

  await performSearch(page, semaphoreGardens.title, 'ALBUM');
  const card = page.locator('li').filter({ hasText: semaphoreGardens.title }).first();
  await card.getByRole('button', { name: 'Download' }).click();

  await openDownloadsPanel(page);
  await expect(transferRow(page, lighthousePeer.files[0]!.path)).toBeVisible({ timeout: 90_000 });
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

test('a file only a passive peer has is not offered to a passive AirDC++', async ({
  context,
  page,
}) => {
  await freshSession(context, page, 'dc-passive');
  const track = liveAtTheExchange.tracks[0]!;

  await performSearch(page, track.title, 'TRACK');
  const row = page.locator('li').filter({ hasText: track.title }).first();
  await row.getByRole('button', { name: 'Download' }).click();

  await expect(row.getByTitle('No results found')).toBeVisible({ timeout: 120_000 });
  const state = await airdcppState();
  expect(state.queued.filter((q) => q.target.includes(track.title))).toEqual([]);
});

test('a peer gone from its hub at queue time is replaced by the next Direct Connect peer', async ({
  context,
  page,
}) => {
  const { folder } = await freshSession(context, page, 'dc-failover');
  const track = collectedDispatches.tracks[0]!;

  await performSearch(page, track.title, 'TRACK');
  const row = page.locator('li').filter({ hasText: track.title }).first();
  await row.getByRole('button', { name: 'Download' }).click();

  const imported = importedTrackPath(
    folder.name,
    collectedDispatches.artist,
    collectedDispatches.title,
    track.title,
    'mp3',
  );
  await expectFileAppears(imported, 150_000);
  const state = await airdcppState();
  const sent = state.queued.filter((q) => q.target.includes(track.title));
  expect(sent.map((q) => q.nick)).toEqual([relayPeer.nick]);
});

test('cancelling a Direct Connect download removes its bundle and keeps the finished file', async ({
  context,
  page,
}) => {
  const { folder } = await freshSession(context, page, 'dc-cancel');
  const done = atticSignals.tracks[0]!;

  await performSearch(page, atticSignals.title, 'ALBUM');
  const card = page.locator('li').filter({ hasText: atticSignals.title }).first();
  await card.getByRole('button', { name: 'Download' }).click();

  await openDownloadsPanel(page);
  const stalledPath = atticPeer.files[1]!.path;
  const item = transferRow(page, stalledPath);
  await expect(item).toBeVisible({ timeout: 90_000 });
  // The album's files as the double queued them: the one still running, and the one sent.
  // Soulbeet removes a sent file's bundle as soon as it found the file, so that one is known
  // by its queue entry and its removal once it left the bundle list.
  const attic = async () => {
    const state = await airdcppState();
    const queued = state.queued.filter((q) => q.target.includes(atticSignals.title));
    const running = state.bundles.filter(
      (b) => b.state === 'running' && queued.some((q) => q.id === b.id),
    );
    const finished = queued.filter(
      (q) =>
        state.bundles.some((b) => b.id === q.id && b.state === 'completed') ||
        state.removals.some((r) => r.id === q.id && r.finished),
    );
    return { running, finished };
  };
  await expect
    .poll(
      async () => {
        const { running, finished } = await attic();
        return { running: running.length, finished: finished.length };
      },
      { timeout: 30_000 },
    )
    .toEqual({ running: 1, finished: 1 });
  const now = await attic();
  const running = now.running[0]!;
  const finished = now.finished[0]!;

  await item.hover();
  await item.getByTitle('Cancel download').click();
  await expect(item.getByText('CANCEL', { exact: true })).toBeVisible({ timeout: 30_000 });

  await expect
    .poll(async () => (await airdcppState()).removals.map((r) => r.id), { timeout: 30_000 })
    .toContain(running.id);
  const after = await airdcppState();
  // No removal ever asked for finished files to go.
  for (const removal of after.removals) {
    expect(removal.body).toEqual({ remove_finished: false });
  }
  expect(after.removals.find((r) => r.id === running.id)!.finished).toBe(false);
  expect(existsSync(`${onHost(running.target, after.root)}.dctmp`)).toBe(false);
  // The finished file is in its batch folder, or already imported from it.
  const imported = importedTrackPath(
    folder.name,
    atticSignals.artist,
    atticSignals.title,
    done.title,
    'flac',
  );
  await expect
    .poll(() => existsSync(onHost(finished.target, after.root)) || existsSync(imported), {
      timeout: 60_000,
    })
    .toBe(true);
});

test('turning Direct Connect off stops searching it and takes it off the status line', async ({
  browser,
  context,
  page,
}) => {
  await asAdmin(browser, (adminPage) => disableBackend(adminPage, DC));
  const before = (await airdcppState()).searches;

  await freshSession(context, page, 'dc-off');
  const track = liveAtTheExchange.tracks[0]!;
  await performSearch(page, track.title, 'TRACK');
  const row = page.locator('li').filter({ hasText: track.title }).first();
  await row.getByRole('button', { name: 'Download' }).click();
  await expect(row.getByTitle('No results found')).toBeVisible({ timeout: 120_000 });
  expect((await airdcppState()).searches).toBe(before);

  await openServiceStatus(page);
  await expect(page.getByText('SOULSEEK ONLINE', { exact: true })).toBeVisible({
    timeout: 30_000,
  });
  await expect(page.getByText(/^DIRECT CONNECT /)).toHaveCount(0);
});
