// Settings flows: folder management and user management.
// The Config tab (slskd connection) is covered by the setup project.

import { randomUUID } from 'node:crypto';
import { expect, test } from '@playwright/test';
import {
  admin,
  loginViaApi,
  openLibraryTab,
  openSettings,
  registerUser,
  uniqueUser,
  type TestUser,
} from '../helpers/app.js';
import { musicDirForApp } from '../helpers/env.js';


let user: TestUser;

test.beforeEach(async ({ context, page }) => {
  user = uniqueUser('settings');
  await registerUser(context.request, user);
  await loginViaApi(context, user);
  await page.goto('/');
  await openSettings(page);
});

test('views and updates the personal music library folder', async ({ page }) => {
  await openLibraryTab(page);

  // Registration auto-provisions the user's single personal library folder.
  await expect(page.getByText(`${musicDirForApp}/${user.username}`, { exact: true })).toBeVisible();
  await page.getByRole('button', { name: 'Edit Folder' }).click();

  const updatedPath = `${musicDirForApp}/settings-vault-${randomUUID().slice(0, 8)}`;
  await page.getByPlaceholder('My Music').fill('Vault');
  await page.getByPlaceholder('/music/alice').fill(updatedPath);
  await page.getByRole('button', { name: 'Save Changes' }).click();

  await expect(page.getByText('Music library folder saved.')).toBeVisible();
  await expect(page.getByText('Vault', { exact: true })).toBeVisible();
  await expect(page.getByText(updatedPath, { exact: true })).toBeVisible();

  // Persisted across reload.
  await page.reload();
  await openLibraryTab(page);
  await expect(page.getByText('Vault', { exact: true })).toBeVisible();
  await expect(page.getByText(updatedPath, { exact: true })).toBeVisible();
});

test('creates another user account', async ({ context, page }) => {
  // Only an admin may create users.
  await context.clearCookies();
  await loginViaApi(context, admin);
  await page.reload();
  await openSettings(page);
  await page.getByRole('button', { name: 'Users' }).click();

  const name = `made-in-ui-${Date.now()}`;
  await page.getByPlaceholder('Username', { exact: true }).fill(name);
  await page.getByPlaceholder('Password', { exact: true }).fill('a-decent-password');
  await page.getByRole('button', { name: 'Create User' }).click();

  await expect(page.getByText(`User '${name}' created successfully`)).toBeVisible();
  await expect(page.getByText(name, { exact: true })).toBeVisible();
});

test('a non-admin sees only their own account', async ({ page }) => {
  await page.getByRole('button', { name: 'Users' }).click();

  await expect(page.getByText('Your Account')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Create User' })).toHaveCount(0);
});

test('a new password takes the current one', async ({ context, page }) => {
  await page.getByRole('button', { name: 'Users' }).click();

  // A session left open is not enough to give the account a new password.
  const change = page.getByRole('button', { name: 'Change Password' });
  await page.getByLabel('Current password').fill('not-the-password');
  await page.getByLabel('New password').fill('a-new-password');
  await change.click();
  await expect(page.getByText('Wrong password.')).toBeVisible();

  await page.getByLabel('Current password').fill(user.password);
  await change.click();
  await expect(page.getByText('Password updated')).toBeVisible();
  // This browser stays signed in, and the new password opens the account.
  expect((await page.request.get('/api/auth/me')).status()).toBe(200);
  await context.clearCookies();
  await loginViaApi(context, { ...user, password: 'a-new-password' });
});
