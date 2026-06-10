// Mobile interaction coverage (Pixel 7 emulation, touch enabled). Guards the
// long-press/tap gesture handling on the download icon, which once shipped a
// WASM panic that froze the whole UI on phones.

import type { Locator } from '@playwright/test';
import { expect, test } from '@playwright/test';
import { freshSession, performSearch } from '../helpers/app.js';
import { expectFileAppears, importedTrackPath } from '../helpers/files.js';
import { foglight } from '../fixtures/dataset.js';

test.describe.configure({ timeout: 180_000 });


function dispatchPointer(locator: Locator, type: string): Promise<void> {
  return locator.dispatchEvent(type, {
    bubbles: true,
    cancelable: true,
    composed: true,
    pointerId: 1,
    pointerType: 'touch',
    isPrimary: true,
  });
}

test('taps a track to download it', async ({ context, page }) => {
  const { folder } = await freshSession(context, page, 'mobile-tap');

  await performSearch(page, foglight.title, 'TRACK');
  const row = page.locator('li').filter({ hasText: foglight.title }).first();
  await row.getByRole('button', { name: 'Download' }).tap();

  // The page must stay responsive while the pipeline runs: this used to
  // freeze when a dropped-signal write panicked the WASM runtime.
  await page.getByRole('button', { name: 'Downloads', exact: true }).tap();
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

test('opens the folder menu with a long-press instead of downloading', async ({
  context,
  page,
}) => {
  const { folder } = await freshSession(context, page, 'mobile-press');

  await performSearch(page, 'Paper Lanterns', 'TRACK');
  const row = page.locator('li').filter({ hasText: 'Paper Lanterns' }).first();
  const button = row.getByRole('button', { name: 'Download' });

  await dispatchPointer(button, 'pointerdown');
  await page.waitForTimeout(700);
  await dispatchPointer(button, 'pointerup');

  // Long-press opens the folder picker; the tap-download must NOT have fired,
  // so the menu lists the folder instead of the row showing a spinner.
  await expect(row.getByText(folder.name)).toBeVisible();
});

test('cancels the long-press when the touch turns into a scroll', async ({ context, page }) => {
  const { folder } = await freshSession(context, page, 'mobile-scroll');

  await performSearch(page, 'Undertow', 'TRACK');
  const row = page.locator('li').filter({ hasText: 'Undertow' }).first();
  const button = row.getByRole('button', { name: 'Download' });

  await dispatchPointer(button, 'pointerdown');
  await dispatchPointer(button, 'pointercancel');
  await page.waitForTimeout(700);

  // No menu: the gesture was abandoned, not held.
  await expect(row.getByText(folder.name)).not.toBeVisible();
});
