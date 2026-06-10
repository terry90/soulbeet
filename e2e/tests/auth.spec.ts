import { expect, test } from '@playwright/test';
import { loginViaUi, registerUser, uniqueUser } from '../helpers/app.js';

test('unauthenticated visitors are redirected to login', async ({ page }) => {
  await page.goto('/');
  await expect(page).toHaveURL(/\/login$/);
  await expect(page.getByRole('button', { name: 'AUTHENTICATE' })).toBeVisible();

  await page.goto('/settings');
  await expect(page).toHaveURL(/\/login$/);
});

test('rejects wrong credentials with a visible error', async ({ page }) => {
  await page.goto('/login');
  await page.getByPlaceholder('Enter username').fill('admin');
  await page.getByPlaceholder('Enter password').fill('definitely-wrong');
  await page.getByRole('button', { name: 'AUTHENTICATE' }).click();

  await expect(page.getByText('Invalid username or password')).toBeVisible();
  await expect(page).toHaveURL(/\/login$/);
});

test('logs a registered user in and out through the UI', async ({ page }) => {
  const user = uniqueUser('auth');
  await registerUser(page.context().request, user);

  await loginViaUi(page, user);
  await expect(page.getByPlaceholder('Search artist, album or track...')).toBeVisible();

  await page.getByRole('button', { name: 'Logout' }).click();
  await expect(page).toHaveURL(/\/login$/);

  // Session is actually gone, not just the page swapped.
  await page.goto('/dashboard');
  await expect(page).toHaveURL(/\/login$/);
});

test('submits the login form with the Enter key', async ({ page }) => {
  const user = uniqueUser('auth-enter');
  await registerUser(page.context().request, user);

  await page.goto('/login');
  await page.getByPlaceholder('Enter username').fill(user.username);
  await page.getByPlaceholder('Enter password').fill(user.password);
  await page.getByPlaceholder('Enter password').press('Enter');

  await expect(page).toHaveURL(/\/$/);
});
