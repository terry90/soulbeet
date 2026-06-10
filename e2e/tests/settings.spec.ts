// Settings flows: folder management, user management, search preferences.
// The Config tab (slskd connection) is covered by the setup project.

import { expect, test } from '@playwright/test';
import { loginViaApi, openSettings, registerUser, uniqueUser } from '../helpers/app.js';
import { musicDirForApp } from '../helpers/env.js';


test.beforeEach(async ({ context, page }) => {
  const user = uniqueUser('settings');
  await registerUser(context.request, user);
  await loginViaApi(context, user);
  await page.goto('/');
  await openSettings(page);
});

test('creates and deletes a music folder', async ({ page }) => {
  await page.getByRole('button', { name: 'Library' }).click();

  await page.getByPlaceholder('My Music').fill('Vault');
  await page.getByPlaceholder('/home/user/Music').fill(`${musicDirForApp}/settings-vault`);
  await page.getByRole('button', { name: 'Add Folder' }).click();

  await expect(page.getByText('Folder added successfully')).toBeVisible();
  await expect(page.getByText('Vault', { exact: true })).toBeVisible();

  await page.getByRole('button', { name: 'Delete' }).first().click();
  await expect(page.getByText('Folder deleted successfully')).toBeVisible();
  await expect(page.getByText('Vault', { exact: true })).not.toBeVisible();
});

test('creates another user account', async ({ page }) => {
  await page.getByRole('button', { name: 'Users' }).click();

  const name = `made-in-ui-${Date.now()}`;
  await page.getByPlaceholder('Username', { exact: true }).fill(name);
  await page.getByPlaceholder('Password', { exact: true }).fill('a-decent-password');
  await page.getByRole('button', { name: 'Create User' }).click();

  await expect(page.getByText(`User '${name}' created successfully`)).toBeVisible();
  await expect(page.getByText(name, { exact: true })).toBeVisible();
});

test('persists the default metadata provider', async ({ page }) => {
  await page.getByRole('button', { name: 'Search', exact: true }).click();

  const select = page.locator('select').first();
  await select.selectOption({ label: 'MusicBrainz' });
  await page.getByRole('button', { name: 'Save Preferences' }).click();

  await expect(page.getByText('Settings saved successfully')).toBeVisible();

  // Leave and come back (client-side), then check the choice stuck.
  await page.getByRole('link', { name: 'Dashboard' }).click();
  await openSettings(page);
  await page.getByRole('button', { name: 'Search', exact: true }).click();
  await expect(page.locator('select').first()).toHaveValue('musicbrainz');
});
