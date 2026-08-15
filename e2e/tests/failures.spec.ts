// Failure-path coverage. These tests script the slskd stub's peer behavior
// and availability, so the project runs serially after everything else and
// restores stub state between tests.

import type { Page } from '@playwright/test';
import { expect, test } from '@playwright/test';
import { freshSession, performSearch } from '../helpers/app.js';
import { resetStubs, setOutage, setPeerBehavior } from '../helpers/control.js';
import { expectFileAppears, importedTrackPath } from '../helpers/files.js';
import { driftwoodCodes, glassAtlas } from '../fixtures/dataset.js';

test.describe.configure({ timeout: 180_000 });

test.afterEach(async () => {
  await resetStubs();
});

function transferRow(page: Page, remotePath: string) {
  return page.locator('div.group').filter({ has: page.getByTitle(remotePath, { exact: true }) });
}

test('falls back to manual source picking when no source scores high enough', async ({
  context,
  page,
}) => {
  await freshSession(context, page, 'fail-fallback');

  await performSearch(page, driftwoodCodes.title, 'ALBUM');

  const card = page.locator('li').filter({ hasText: driftwoodCodes.title }).first();
  await card.getByRole('button', { name: 'Download' }).click();

  await expect(page.getByText(`No confident match for ${driftwoodCodes.title}.`)).toBeVisible({
    timeout: 60_000,
  });
  await page.getByText('Pick a source manually').click();

  await expect(page.getByText('Download Options')).toBeVisible();
  await expect(page.getByText('WMA', { exact: true }).first()).toBeVisible();
});

test('fails tracks whose transfer never shows up in slskd', async ({ context, page }) => {
  await setPeerBehavior('collector_01', 'ghost');
  await freshSession(context, page, 'fail-ghost');

  await performSearch(page, 'Paper Lanterns', 'TRACK');
  const row = page.locator('li').filter({ hasText: 'Paper Lanterns' }).first();
  await row.getByRole('button', { name: 'Download' }).click();

  await page.getByRole('button', { name: 'Downloads', exact: true }).click();
  const item = transferRow(
    page,
    'Music\\FLAC\\Static Harbor\\Glass Atlas\\02 - Paper Lanterns.flac',
  );
  await expect(item).toBeVisible({ timeout: 60_000 });

  // The monitor gives up after its empty-poll grace period and fails the row
  // instead of leaving it queued forever.
  await expect(item.getByText('ERR', { exact: true })).toBeVisible({ timeout: 90_000 });
  await expect(item.getByText('Download never appeared in slskd')).toBeVisible();
});

test('surfaces a mid-transfer error and recovers on retry', async ({ context, page }) => {
  await setPeerBehavior('collector_01', 'flaky');
  await setPeerBehavior('mp3_hoarder', 'offline');
  const { folder } = await freshSession(context, page, 'fail-flaky');

  await performSearch(page, 'Northern Line', 'TRACK');
  const row = page.locator('li').filter({ hasText: 'Northern Line' }).first();
  await row.getByRole('button', { name: 'Download' }).click();

  await page.getByRole('button', { name: 'Downloads', exact: true }).click();
  const item = transferRow(
    page,
    'Music\\FLAC\\Static Harbor\\Glass Atlas\\01 - Northern Line.flac',
  );
  await expect(item.getByText('ERR', { exact: true })).toBeVisible({ timeout: 90_000 });

  // Source recovers; a second attempt goes through end to end. The drawer
  // overlays the results, so close it before reaching for the row again.
  await page.getByRole('button', { name: 'Close downloads' }).click();
  await setPeerBehavior('collector_01', 'happy');
  await row.getByRole('button', { name: 'Download' }).click();

  const imported = importedTrackPath(
    folder.name,
    glassAtlas.artist,
    glassAtlas.title,
    'Northern Line',
    'flac',
  );
  await expectFileAppears(imported, 120_000);
});

test('rides out slskd auto-retry of a failed transfer', async ({ context, page }) => {
  // slskd 0.26 retries failed downloads itself: the transfer briefly shows
  // "Completed, Errored", then re-queues and succeeds. The monitor must not
  // act on the transient failure; the proof is that the file still imports.
  await setPeerBehavior('collector_01', 'retryflap');
  await setPeerBehavior('mp3_hoarder', 'offline');
  const { folder } = await freshSession(context, page, 'fail-retryflap');

  await performSearch(page, 'Paper Lanterns', 'TRACK');
  const row = page.locator('li').filter({ hasText: 'Paper Lanterns' }).first();
  await row.getByRole('button', { name: 'Download' }).click();

  const imported = importedTrackPath(
    folder.name,
    glassAtlas.artist,
    glassAtlas.title,
    'Paper Lanterns',
    'flac',
  );
  await expectFileAppears(imported, 120_000);
});

test('falls over to the next peer when the best source errors', async ({ context, page }) => {
  // collector_01 ranks first (FLAC) and dies mid-transfer: 'flaky' ends at
  // "Completed, Errored" with no slskd retry. mp3_hoarder serves the same
  // album from a completely different remote path and in a different format,
  // so failover has to match the track by title rather than by filename.
  await setPeerBehavior('collector_01', 'flaky');
  const { folder } = await freshSession(context, page, 'fail-failover');

  await performSearch(page, 'Paper Lanterns', 'TRACK');
  const row = page.locator('li').filter({ hasText: 'Paper Lanterns' }).first();
  await row.getByRole('button', { name: 'Download' }).click();

  // The mp3 extension is the assertion that matters: the file can only exist
  // if the fallback peer served it.
  const imported = importedTrackPath(
    folder.name,
    glassAtlas.artist,
    glassAtlas.title,
    'Paper Lanterns',
    'mp3',
  );
  await expectFileAppears(imported, 120_000);

  // Failover rebinds one download rather than starting a second, so the dead
  // peer's row is replaced by the retry's instead of sitting beside it.
  await page.getByRole('button', { name: 'Downloads', exact: true }).click();
  const drawer = page.locator('div.bg-beet-panel').filter({ hasText: 'Active Transfers' });
  await expect(drawer.locator('div.group')).toHaveCount(1);
});

test('fails fast when the peer goes offline before enqueue', async ({ context, page }) => {
  // slskd reports an offline peer with "appears to be offline" in the enqueue
  // response body (404 on the batches endpoint). That is non-retryable: the
  // row must fail with the offline reason well before the ~14s the generic
  // 3-retry backoff would take.
  await setPeerBehavior('mp3_hoarder', 'offline');
  // enqueueoffline: still answers searches, but is gone by download time.
  await setPeerBehavior('collector_01', 'enqueueoffline');
  await freshSession(context, page, 'fail-offline');

  await performSearch(page, 'Paper Lanterns', 'TRACK');
  const row = page.locator('li').filter({ hasText: 'Paper Lanterns' }).first();
  await row.getByRole('button', { name: 'Download' }).click();

  await page.getByRole('button', { name: 'Downloads', exact: true }).click();
  const item = transferRow(
    page,
    'Music\\FLAC\\Static Harbor\\Glass Atlas\\02 - Paper Lanterns.flac',
  );
  await expect(item.getByText('ERR', { exact: true })).toBeVisible({ timeout: 10_000 });
  await expect(item.getByText(/is offline/)).toBeVisible();
});

test('surfaces a per-file enqueue rejection with its real reason', async ({ context, page }) => {
  // The batches endpoint reports enqueue failures per file as
  // {filename, message}; the row must carry that message instead of a
  // generic whole-batch error.
  await setPeerBehavior('collector_01', 'partialenqueue');
  await setPeerBehavior('mp3_hoarder', 'offline');
  await freshSession(context, page, 'fail-reject');

  await performSearch(page, 'Paper Lanterns', 'TRACK');
  const row = page.locator('li').filter({ hasText: 'Paper Lanterns' }).first();
  await row.getByRole('button', { name: 'Download' }).click();

  await page.getByRole('button', { name: 'Downloads', exact: true }).click();
  const item = transferRow(
    page,
    'Music\\FLAC\\Static Harbor\\Glass Atlas\\02 - Paper Lanterns.flac',
  );
  await expect(item.getByText('ERR', { exact: true })).toBeVisible({ timeout: 30_000 });
  await expect(item.getByText('Error: Enqueue rejected by peer')).toBeVisible();
});

test('reports a failed search when slskd is unreachable', async ({ context, page }) => {
  await freshSession(context, page, 'fail-outage');
  await setOutage(true);

  await performSearch(page, 'Undertow', 'TRACK');
  const row = page.locator('li').filter({ hasText: 'Undertow' }).first();
  await row.getByRole('button', { name: 'Download' }).click();

  // The row's download icon flips to the failed state, with the reason as
  // its tooltip.
  await expect(row.getByTitle('No results found')).toBeVisible({ timeout: 90_000 });
});

test('cancels a transfer stuck in the remote queue', async ({ context, page }) => {
  await setPeerBehavior('collector_01', 'stall');
  await freshSession(context, page, 'fail-cancel');

  await performSearch(page, 'Undertow', 'TRACK');
  const row = page.locator('li').filter({ hasText: 'Undertow' }).first();
  await row.getByRole('button', { name: 'Download' }).click();

  await page.getByRole('button', { name: 'Downloads', exact: true }).click();
  const item = transferRow(page, 'Music\\FLAC\\Static Harbor\\Glass Atlas\\03 - Undertow.flac');
  await expect(item).toBeVisible({ timeout: 60_000 });

  await item.hover();
  await item.getByTitle('Cancel download').click();

  await expect(item.getByText('CANCEL', { exact: true })).toBeVisible({ timeout: 30_000 });
});
