// The second instance starts with the beets config from issue #90, which
// loads no metadata source. The user must see why imports cannot work, every
// download must be refused with that reason, and fixing the file on disk must
// clear it without a restart.

import { expect, test } from '@playwright/test';
import { copyFileSync, renameSync } from 'node:fs';
import { join } from 'node:path';
import { freshSession, performSearch } from '../helpers/app.js';
import { e2eRoot, user90BrokenConfig, user90ConfigDirOnHost } from '../helpers/env.js';
import { maritime } from '../fixtures/dataset.js';

const BROKEN = 'No metadata source plugin loaded';

// The page polls health every 10 s, and the poll that notices the changed
// file starts a new check that the next poll reports.
const HEALTH_CHANGE_MS = 30_000;

// Both tests rewrite the one config file, so they run in order. A retry runs
// both again, and the first one puts the broken config back.
test.describe.configure({ mode: 'serial', timeout: 120_000 });

/** Replaces the instance's config with one rename, so it never reads half a file. */
function installConfig(source: string): void {
  const target = join(user90ConfigDirOnHost, 'config.yaml');
  const staged = `${target}.staged`;
  copyFileSync(source, staged);
  renameSync(staged, target);
}

test('a broken config is visible and refuses downloads', async ({ context, page }) => {
  installConfig(user90BrokenConfig);
  const { folder } = await freshSession(context, page, 'user90-broken');

  await expect(page.getByText('BEETS BROKEN', { exact: true })).toBeVisible({
    timeout: HEALTH_CHANGE_MS,
  });
  const banner = page.getByRole('link', { name: 'Fix in Config' }).locator('..');
  await expect(banner.getByText(BROKEN, { exact: true })).toBeVisible();

  // One tap: the download icon fails with the reason as its tooltip.
  await performSearch(page, maritime.title, 'TRACK');
  const row = page.locator('li').filter({ hasText: maritime.title }).first();
  const download = row.getByRole('button', { name: 'Download' });
  await download.click();
  await expect(download).toHaveAttribute('title', BROKEN);

  // A hand-picked source: the queue request is refused and the reason shown.
  await row.getByRole('button', { name: 'Search sources' }).click();
  await expect(page.getByText('Best match')).toBeVisible({ timeout: 60_000 });
  await page.getByRole('button', { name: 'Select All' }).first().click();
  await page.locator('select[name="dl_folder"]').selectOption({ label: folder.name });
  await page.getByRole('button', { name: 'Start download' }).click();
  await expect(page.locator('p').filter({ hasText: BROKEN })).toBeVisible();

  await banner.getByRole('link', { name: 'Fix in Config' }).click();
  await expect(page).toHaveURL(/\/settings$/);
  const beetsRow = page.getByRole('heading', { name: 'Beets', exact: true }).locator('..');
  await expect(beetsRow.getByText(BROKEN, { exact: true })).toBeVisible();
});

test('fixing the config at runtime turns health ready', async ({ context, page }) => {
  installConfig(join(e2eRoot, '.fixtures', 'beets-e2e.yaml'));
  await freshSession(context, page, 'user90-fixed');

  await expect(page.getByText('BEETS READY', { exact: true })).toBeVisible({
    timeout: HEALTH_CHANGE_MS,
  });
  await expect(page.getByRole('link', { name: 'Fix in Config' })).toHaveCount(0);
});
