// Talks to the app's HTTP API directly for test setup (user registration,
// login cookies, folders), so specs only exercise the UI flows they are
// actually about.

import { randomUUID } from 'node:crypto';
import type { APIRequestContext, BrowserContext, Page } from '@playwright/test';
import { expect } from '@playwright/test';
import { baseUrl, musicDirForApp } from './env.js';

export interface TestUser {
  username: string;
  password: string;
}

export function uniqueUser(prefix = 'user'): TestUser {
  return {
    username: `${prefix}-${randomUUID().slice(0, 8)}`,
    password: `pw-${randomUUID().slice(0, 12)}`,
  };
}

export async function registerUser(request: APIRequestContext, user: TestUser): Promise<void> {
  const response = await request.post(`${baseUrl}/api/auth/register`, {
    data: { username: user.username, password: user.password },
  });
  expect(response.ok(), `register ${user.username}: ${response.status()}`).toBeTruthy();
}

/**
 * Logs in through the API and plants the auth cookie into the browser
 * context. The login UI itself is covered by auth.spec.ts.
 */
export async function loginViaApi(context: BrowserContext, user: TestUser): Promise<void> {
  const response = await context.request.post(`${baseUrl}/api/auth/login`, {
    data: { username: user.username, password: user.password },
  });
  expect(response.ok(), `login ${user.username}: ${response.status()}`).toBeTruthy();
}

/** Creates a per-test music folder for the user and returns its app-side path. */
export async function createFolder(
  context: BrowserContext,
  name: string,
): Promise<{ name: string; path: string }> {
  const path = `${musicDirForApp}/${name}`;
  const response = await context.request.post(`${baseUrl}/api/folders`, {
    data: { name, path },
  });
  expect(response.ok(), `create folder ${name}: ${response.status()}`).toBeTruthy();
  return { name, path };
}

export async function loginViaUi(page: Page, user: TestUser): Promise<void> {
  await page.goto('/login');
  await page.getByPlaceholder('Enter username').fill(user.username);
  await page.getByPlaceholder('Enter password').fill(user.password);
  await page.getByRole('button', { name: 'AUTHENTICATE' }).click();
  await expect(page).toHaveURL(/\/$/);
}

/**
 * Opens the settings page through the navbar. A hard navigation to /settings
 * would bounce off the auth guard while the client-side auth state rehydrates.
 */
export async function openSettings(page: Page): Promise<void> {
  await page.getByRole('link', { name: 'Settings' }).click();
  await expect(page).toHaveURL(/\/settings$/);
}

/**
 * Runs a metadata search through the UI and waits for results. The app
 * spaces MusicBrainz requests out (1 req/s by design), so under parallel
 * test load results can take a while to come back.
 */
export async function performSearch(
  page: Page,
  query: string,
  type: 'ALBUM' | 'TRACK',
): Promise<void> {
  await page.getByRole('button', { name: type, exact: true }).filter({ visible: true }).click();
  await page.getByPlaceholder('Search artist, album or track...').fill(query);
  await page.getByRole('button', { name: 'SEARCH' }).filter({ visible: true }).click();
  await expect(page.getByText('Search Results')).toBeVisible({ timeout: 45_000 });
}

/** A registered, logged-in user with their own folder: the common baseline. */
export async function freshSession(
  context: BrowserContext,
  page: Page,
  folderName: string,
): Promise<{ user: TestUser; folder: { name: string; path: string } }> {
  const user = uniqueUser();
  await registerUser(context.request, user);
  await loginViaApi(context, user);
  const folder = await createFolder(context, folderName);
  await page.goto('/');
  return { user, folder };
}
