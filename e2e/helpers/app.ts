// Talks to the app's HTTP API directly for test setup (user registration,
// login cookies, folders), so specs only exercise the UI flows they are
// actually about. URLs are relative so each project's baseURL picks the
// instance.

import { randomUUID } from 'node:crypto';
import { copyFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import type { APIRequestContext, BrowserContext, Locator, Page } from '@playwright/test';
import { expect } from '@playwright/test';
import { audioFileName, type PeerShare } from '../fixtures/dataset.js';
import {
  appAdminPassword,
  e2eRoot,
  mcpMusicDirForApp,
  musicDirForApp,
  musicDirOnHost,
  slskdApiKey,
} from './env.js';

export interface TestUser {
  username: string;
  password: string;
}

/** The seeded admin with the shipped password, which signs in only to choose a new one. */
export const shippedAdmin: TestUser = { username: 'admin', password: 'admin' };

/** The seeded admin once setup.app.ts chose its password. */
export const admin: TestUser = { username: 'admin', password: appAdminPassword };

export function uniqueUser(prefix = 'user'): TestUser {
  return {
    username: `${prefix}-${randomUUID().slice(0, 8)}`,
    password: `pw-${randomUUID().slice(0, 12)}`,
  };
}

export async function registerUser(request: APIRequestContext, user: TestUser): Promise<void> {
  const response = await request.post('/api/auth/register', {
    data: { username: user.username, password: user.password },
  });
  expect(response.ok(), `register ${user.username}: ${response.status()}`).toBeTruthy();
}

/**
 * Logs in through the API and plants the auth cookie into the browser
 * context. The login UI itself is covered by auth.spec.ts.
 */
export async function loginViaApi(context: BrowserContext, user: TestUser): Promise<void> {
  const response = await context.request.post('/api/auth/login', {
    data: { username: user.username, password: user.password },
  });
  expect(response.ok(), `login ${user.username}: ${response.status()}`).toBeTruthy();
}

/**
 * Changes the password of the user logged in to the context through the API,
 * from `current` (the one the login form takes) to `password`.
 */
export async function changePassword(
  context: BrowserContext,
  current: string,
  password: string,
): Promise<void> {
  const response = await context.request.post('/api/users/password', {
    data: { current_password: current, password },
  });
  expect(response.ok(), `change password: ${response.status()}`).toBeTruthy();
}

/**
 * Signs `context` in to the seeded admin with the shipped password, then
 * chooses `password`, as the first sign-in of a fresh install does. The
 * context keeps the session the change renews.
 */
export async function chooseAdminPassword(context: BrowserContext, password: string): Promise<void> {
  await loginViaApi(context, shippedAdmin);
  await changePassword(context, shippedAdmin.password, password);
}

/**
 * Creates a per-test music folder for the user and returns its app-side path.
 * The name is made unique so retries and reruns never hit another user's folder.
 */
export async function createFolder(
  context: BrowserContext,
  name: string,
): Promise<{ name: string; path: string }> {
  const unique = `${name}-${randomUUID().slice(0, 8)}`;
  const path = `${musicDirForApp}/${unique}`;
  const response = await context.request.post('/api/folders', {
    data: { name: unique, path },
  });
  expect(response.ok(), `create folder ${unique}: ${response.status()}`).toBeTruthy();
  return { name: unique, path };
}

/**
 * Makes {musicDirOnHost}/{prefix}-{8 hex} and the seeds' parent directories on
 * the host, copies each .fixtures/audio/{audioFileName(seed)} to its
 * relativePath, then creates the folder at {mcpMusicDirForApp}/{name} for the
 * user logged in to `context`. The runner creates the directories, so it can
 * seed them; the app runs as root and can write into them.
 */
export async function seededFolder(
  context: BrowserContext,
  prefix: string,
  seeds: (Pick<PeerShare, 'recordingMbid' | 'format'> & { relativePath: string })[],
): Promise<{ name: string; path: string; hostPath: string }> {
  const name = `${prefix}-${randomUUID().slice(0, 8)}`;
  const hostPath = join(musicDirOnHost, name);
  mkdirSync(hostPath);
  for (const seed of seeds) {
    const target = join(hostPath, seed.relativePath);
    mkdirSync(dirname(target), { recursive: true });
    copyFileSync(join(e2eRoot, '.fixtures', 'audio', audioFileName(seed)), target);
  }
  const path = `${mcpMusicDirForApp}/${name}`;
  const response = await context.request.post('/api/folders', { data: { name, path } });
  expect(response.ok(), `create folder ${name}: ${response.status()}`).toBeTruthy();
  return { name, path, hostPath };
}

/**
 * Clicks the confirm button of the ConfirmModal showing `message`: the last
 * button of the innermost element holding the message.
 */
export async function confirmIn(page: Page, message: string): Promise<void> {
  const holder = page.locator('div').filter({ has: page.getByText(message, { exact: true }) }).last();
  await holder.getByRole('button').last().click();
}

/**
 * Points the app at slskd through Settings > Config and allows `musicDir` as a
 * folder root, as the logged-in admin of `page`. slskd config is app-wide
 * (app_config table), not per-user.
 */
export async function configureInstance(page: Page, slskdUrl: string, musicDir: string): Promise<void> {
  await openSettings(page);
  await page.getByRole('button', { name: 'Config' }).click();

  await page.getByPlaceholder('http://localhost:5030').fill(slskdUrl);
  await page.getByPlaceholder('Enter slskd API key').fill(slskdApiKey);
  await page.getByRole('button', { name: 'Save Configuration' }).click();

  await expect(page.getByText('Configuration saved')).toBeVisible();

  // Folders may only be created under a root the admin allows.
  const roots = await page.request.post('/api/folders/roots', {
    data: { path: musicDir },
  });
  expect(roots.ok(), `allow folder root: ${roots.status()}`).toBeTruthy();
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
  await page.getByRole('button', { name: 'SEARCH', exact: true }).filter({ visible: true }).click();
  await expect(page.getByText('Search Results')).toBeVisible({ timeout: 45_000 });
}

export async function openDownloadsPanel(page: Page): Promise<void> {
  await page.getByRole('button', { name: 'Downloads', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Active Transfers' })).toBeVisible();
}

/** The transfer row in the downloads panel, located by its remote path. */
export function transferRow(page: Page, remotePath: string): Locator {
  return page.locator('div.group').filter({ has: page.getByTitle(remotePath, { exact: true }) });
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
