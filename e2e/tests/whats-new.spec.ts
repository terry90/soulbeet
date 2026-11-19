// The "What's new" dialog opened from the footer. Its text comes from
// CHANGELOG.md, which the app embeds at build time, so the test reads the same
// file for what to expect.

import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { expect, test } from '@playwright/test';
import { loginViaApi, registerUser, uniqueUser } from '../helpers/app.js';
import { e2eRoot } from '../helpers/env.js';
import { shot } from '../helpers/shots.js';

/** The newest section's label and headline as the dialog shows them. */
function newestSection(): { label: string; headline: string } {
  const lines = readFileSync(join(dirname(e2eRoot), 'CHANGELOG.md'), 'utf8').split('\n');
  const heading = lines.find((line) => line.startsWith('## '));
  const headline = lines.find((line) => line.startsWith('### '));
  if (heading === undefined || headline === undefined) {
    throw new Error('CHANGELOG.md has no section');
  }
  const release = heading.slice(3);
  // A release turns '## Unreleased' into '## <version> - <date>'.
  const label = release === 'Unreleased' ? 'Coming in the next release' : `v${release.split(' - ')[0]}`;
  return { label, headline: headline.slice(4).replaceAll('`', '') };
}

test("opens What's new from the footer and closes it with Escape", async ({ context, page }) => {
  // A new account has read everything already, so nothing opens on its own.
  const user = uniqueUser('whats-new');
  await registerUser(context.request, user);
  await loginViaApi(context, user);
  await page.goto('/');

  const { label, headline } = newestSection();
  await page.getByRole('button', { name: "What's new" }).click();

  const dialog = page.getByRole('dialog', { name: "What's new in Soulbeet" });
  await expect(dialog).toBeVisible();
  await expect(dialog.getByRole('heading', { name: headline })).toBeVisible();
  await expect(dialog.getByText(label, { exact: true })).toBeVisible();
  await shot(page.locator('body'), 'whats-new');

  await page.keyboard.press('Escape');
  await expect(dialog).toBeHidden();
});
