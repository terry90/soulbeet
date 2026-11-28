// Mobile interaction coverage (Pixel 7 emulation, touch enabled). Guards tap
// handling on the download icon and source picker so the UI stays responsive
// on phones.

import { expect, test } from '@playwright/test';
import { freshSession, performSearch } from '../helpers/app.js';
import { expectFileAppears, importedTrackPath } from '../helpers/files.js';
import { foglight } from '../fixtures/dataset.js';

test.describe.configure({ timeout: 180_000 });

test('taps a track to download it', async ({ context, page }) => {
  const { folder } = await freshSession(context, page, 'mobile-tap');

  await performSearch(page, foglight.title, 'TRACK');
  const row = page.locator('li').filter({ hasText: foglight.title }).first();
  await row.getByRole('button', { name: 'Download' }).tap();

  // The page must stay responsive while the pipeline runs: this used to
  // freeze when a dropped-signal write panicked the WASM runtime.
  await page.getByRole('button', { name: 'Active transfers drawer' }).tap();
  await expect(page.getByRole('heading', { name: 'Active Transfers' })).toBeVisible();

  const imported = importedTrackPath(
    folder.name,
    foglight.artist,
    foglight.title,
    foglight.tracks[0]!.title,
    'flac',
  );
  await expectFileAppears(imported, 120_000);
});

test('taps search sources to open download options and returns to catalog', async ({
  context,
  page,
}) => {
  await freshSession(context, page, 'mobile-sources');

  await performSearch(page, 'Paper Lanterns', 'TRACK');
  const row = page.locator('li').filter({ hasText: 'Paper Lanterns' }).first();
  await row.getByRole('button', { name: 'Search sources' }).tap();

  await expect(page.getByText('Download Options')).toBeVisible();
  await page.getByRole('button', { name: 'Back' }).tap();
  await expect(page.getByRole('heading', { name: 'Catalog', exact: true })).toBeVisible();
});
