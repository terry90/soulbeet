import { randomUUID } from 'node:crypto';
import type { Page } from '@playwright/test';
import { expect, test } from '@playwright/test';
import { loginViaApi, registerUser, uniqueUser } from '../helpers/app.js';
import { navidromeEnabled } from '../helpers/env.js';

// Single sign-on against the OIDC stub (stubs/src/oidc-server.ts), which
// checks the client secret, redirect URI and PKCE before it issues a token.

/** Starts a sign-in: from the login page to the stub provider's form. */
async function startSso(page: Page): Promise<void> {
  await page.goto('/login');
  await page.getByRole('button', { name: 'Sign in with Stub ID' }).click();
  await expect(page.getByRole('button', { name: 'Approve' })).toBeVisible();
}

/** Starts at the login page and approves (or denies) at the stub provider. */
async function signInWithSso(
  page: Page,
  who: { username: string; subject: string },
  decision: 'Approve' | 'Deny' = 'Approve',
): Promise<void> {
  await startSso(page);
  await approveAtProvider(page, who, decision);
}

async function approveAtProvider(
  page: Page,
  who: { username: string; subject: string },
  decision: 'Approve' | 'Deny' = 'Approve',
): Promise<void> {
  await page.getByLabel('Username').fill(who.username);
  await page.getByLabel('Subject').fill(who.subject);
  await page.getByRole('button', { name: decision }).click();
}

async function currentUser(page: Page): Promise<{ username: string; user_id: string }> {
  const response = await page.request.get('/api/auth/me');
  expect(response.ok(), `me: ${response.status()}`).toBeTruthy();
  return response.json();
}

test('creates an account on first sign-in and keeps it when the username changes', async ({
  page,
}) => {
  const subject = randomUUID();
  const username = uniqueUser('sso').username;

  await signInWithSso(page, { username, subject });
  await expect(page).toHaveURL(/\/$/);
  await expect(page.getByPlaceholder('Search artist, album or track...')).toBeVisible();
  const first = await currentUser(page);
  expect(first.username).toBe(username);
  if (navidromeEnabled) {
    // A new account has no Navidrome password yet: the banner says how to connect it.
    await expect(page.getByText('Log in once with your Navidrome password')).toBeVisible();
  }

  await page.getByRole('button', { name: 'Logout' }).click();
  await expect(page).toHaveURL(/\/login$/);

  // Renamed at the provider: still the same account, found by subject.
  await signInWithSso(page, { username: uniqueUser('renamed').username, subject });
  await expect(page).toHaveURL(/\/$/);
  expect(await currentUser(page)).toEqual(first);
});

test('links an existing password account by username, once', async ({ page }) => {
  const user = uniqueUser('sso-link');
  await registerUser(page.context().request, user);
  await loginViaApi(page.context(), user);
  const local = await currentUser(page);
  await page.context().clearCookies();

  await signInWithSso(page, { username: user.username, subject: randomUUID() });
  await expect(page).toHaveURL(/\/$/);
  expect((await currentUser(page)).user_id).toBe(local.user_id);

  // Another identity with the same username does not get in.
  await page.context().clearCookies();
  await signInWithSso(page, { username: user.username, subject: randomUUID() });
  await expect(page).toHaveURL(/\/login\/error\/conflict$/);
  await expect(page.getByText('linked to another single sign-on identity')).toBeVisible();
});

test('links an account whose username differs only in case', async ({ page }) => {
  const user = uniqueUser('sso-case');
  await registerUser(page.context().request, user);
  await loginViaApi(page.context(), user);
  const local = await currentUser(page);
  await page.context().clearCookies();

  await signInWithSso(page, { username: user.username.toUpperCase(), subject: randomUUID() });
  await expect(page).toHaveURL(/\/$/);
  const signedIn = await currentUser(page);
  expect(signedIn.user_id).toBe(local.user_id);
  expect(signedIn.username).toBe(user.username);
});

test('a sign-in started in another tab does not break this one', async ({ context }) => {
  const first = await context.newPage();
  const second = await context.newPage();
  await startSso(first);
  await startSso(second);

  // The first tab finishes after the second one started.
  await approveAtProvider(first, { username: uniqueUser('sso-tab').username, subject: randomUUID() });
  await expect(first).toHaveURL(/\/$/);
});

test('shows the provider refusal on the login page', async ({ page }) => {
  await signInWithSso(page, { username: 'nobody', subject: randomUUID() }, 'Deny');
  await expect(page).toHaveURL(/\/login\/error\/denied$/);
  await expect(page.getByText('cancelled or refused by the provider')).toBeVisible();
});

test('refuses a callback that this browser did not start', async ({ page }) => {
  // No login state cookie: a forged or replayed callback.
  await page.goto('/api/auth/oidc/callback?code=stolen&state=whatever');
  await expect(page).toHaveURL(/\/login\/error\/failed$/);
  await expect(page.getByText('Single sign-on failed')).toBeVisible();
});

test('a single sign-on on the way to an app goes back to its request', async ({ page }) => {
  const target = '/oauth/authorize?client_id=dcr_e2e&state=a%26b';
  await page.goto('/login/return?return_to=' + encodeURIComponent(target));
  await page.getByRole('button', { name: 'Sign in with Stub ID' }).click();
  await approveAtProvider(page, { username: uniqueUser('sso-mcp').username, subject: randomUUID() });
  await expect(page).toHaveURL(/\/oauth\/authorize\?client_id=dcr_e2e&state=a%26b$/);
});

test('a return address off the authorize path is ignored', async ({ page }) => {
  await page.goto(
    '/api/auth/oidc/login?return_to=' + encodeURIComponent('//evil.example/oauth/authorize'),
  );
  await approveAtProvider(page, { username: uniqueUser('sso-evil').username, subject: randomUUID() });
  await expect(page).toHaveURL(/127\.0\.0\.1:9765\/$/);
});
