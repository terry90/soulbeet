import { randomUUID } from 'node:crypto';
import type { BrowserContext, Page } from '@playwright/test';
import { expect, test } from '@playwright/test';
import { loginViaApi, openSettings, registerUser, uniqueUser } from '../helpers/app.js';
import { baseUrl, navidromeEnabled, user90BaseUrl } from '../helpers/env.js';
import { createNavidromeUser } from '../helpers/navidrome.js';

// Single sign-on against the OIDC stub (stubs/src/oidc-server.ts), which
// checks the client secret, the registered redirect URI and PKCE before it
// issues a token. The app on 9765 runs with REGISTRATION=open, so a first
// sign-in under a new name creates an account, and leaves
// OIDC_LINK_BY_USERNAME at its default, so a first sign-in under the name of
// a plain user account that has no single sign-on link joins that account.

interface ProviderUser {
  username: string;
  subject: string;
  /** Claims the provider adds to the ID token, as typed on its form. */
  azp?: string;
  email?: string;
  /** JSON: true, false, or a string such as "true". */
  emailVerified?: string;
}

const appRequest = '/oauth/authorize?client_id=dcr_e2e&state=a%26b';
const backAtAppRequest = /\/oauth\/authorize\?client_id=dcr_e2e&state=a%26b$/;

/** Starts a sign-in: from the login page to the stub provider's form. */
async function startSso(page: Page): Promise<void> {
  await page.goto('/login');
  await page.getByRole('button', { name: 'Sign in with Stub ID' }).click();
  await expect(page.getByRole('button', { name: 'Approve' })).toBeVisible();
}

/** Starts at the login page and approves (or denies) at the stub provider. */
async function signInWithSso(
  page: Page,
  who: ProviderUser,
  decision: 'Approve' | 'Deny' = 'Approve',
): Promise<void> {
  await startSso(page);
  await approveAtProvider(page, who, decision);
}

async function approveAtProvider(
  page: Page,
  who: ProviderUser,
  decision: 'Approve' | 'Deny' = 'Approve',
): Promise<void> {
  await page.getByLabel('Username').fill(who.username);
  await page.getByLabel('Subject').fill(who.subject);
  if (who.azp) await page.getByLabel('Authorized party').fill(who.azp);
  if (who.email) await page.getByLabel('Email', { exact: true }).fill(who.email);
  if (who.emailVerified) await page.getByLabel('Email verified (JSON)').fill(who.emailVerified);
  await page.getByRole('button', { name: decision }).click();
}

async function currentUser(page: Page): Promise<{ username: string; user_id: string }> {
  const response = await page.request.get('/api/auth/me');
  expect(response.ok(), `me: ${response.status()}`).toBeTruthy();
  return response.json();
}

/** The login state cookies a sign-in leaves in the browser. */
async function flowCookies(context: BrowserContext): Promise<string[]> {
  return (await context.cookies()).map((c) => c.name).filter((name) => name.startsWith('oidc_flow_'));
}

/** Signs in with the password form of the page the browser is on. */
async function passwordSignInHere(page: Page, user: { username: string; password: string }): Promise<void> {
  await page.getByPlaceholder('Enter username').fill(user.username);
  await page.getByPlaceholder('Enter password').fill(user.password);
  await page.getByRole('button', { name: 'AUTHENTICATE' }).click();
}

async function openAccountSettings(page: Page): Promise<void> {
  await openSettings(page);
  await page.getByRole('button', { name: 'Users' }).click();
}

test('creates an account on first sign-in and keeps it when the username changes', async ({
  page,
}) => {
  const subject = randomUUID();
  const username = uniqueUser('sso').username;

  await signInWithSso(page, { username, subject });
  await expect(page).toHaveURL(/\/$/);
  await expect(page.getByRole('searchbox')).toBeVisible();
  const first = await currentUser(page);
  expect(first.username).toBe(username);
  // The login state was single use.
  expect(await flowCookies(page.context())).toEqual([]);
  if (navidromeEnabled) {
    // A new account has no Navidrome password yet: the banner says how to connect it.
    await expect(
      page.getByText('Not connected. Connect Navidrome to use discovery and rating sync.'),
    ).toBeVisible();
  }

  await page.getByRole('button', { name: 'Logout' }).click();
  await expect(page).toHaveURL(/\/login$/);

  // Renamed at the provider: still the same account, found by subject.
  await signInWithSso(page, { username: uniqueUser('renamed').username, subject });
  await expect(page).toHaveURL(/\/$/);
  expect(await currentUser(page)).toEqual(first);
});

test('a first sign-in under the name of an existing account joins it', async ({ page }) => {
  const user = uniqueUser('sso-join');
  await registerUser(page.context().request, user);
  await loginViaApi(page.context(), user);
  const local = await currentUser(page);
  await page.context().clearCookies();

  // The same name in any letter case is the same account.
  const nameAtProvider = user.username.toUpperCase();
  await signInWithSso(page, { username: nameAtProvider, subject: randomUUID() });
  await expect(page).toHaveURL(/\/$/);
  expect((await currentUser(page)).user_id).toBe(local.user_id);
  expect(await flowCookies(page.context())).toEqual([]);
  await openAccountSettings(page);
  await expect(page.getByText(`Linked to ${nameAtProvider} at Stub ID.`)).toBeVisible();

  // The account keeps its password.
  await page.context().clearCookies();
  await loginViaApi(page.context(), user);
  expect((await currentUser(page)).user_id).toBe(local.user_id);
});

test('refuses another identity under the name of a linked account', async ({ page }) => {
  const username = uniqueUser('sso-linked').username;
  await signInWithSso(page, { username, subject: randomUUID() });
  await expect(page).toHaveURL(/\/$/);

  await page.context().clearCookies();
  await signInWithSso(page, { username, subject: randomUUID() });
  await expect(page).toHaveURL(/\/login\/error\/conflict$/);
  await expect(page.getByText('linked to another single sign-on identity')).toBeVisible();
});

test('links single sign-on from Settings, then signs in with it', async ({ page }) => {
  const user = uniqueUser('sso-link');
  await registerUser(page.context().request, user);
  await loginViaApi(page.context(), user);
  const local = await currentUser(page);
  await page.goto('/');
  await openAccountSettings(page);

  // A session left open is not enough: linking takes the account's password.
  const link = page.getByRole('button', { name: 'Link single sign-on' });
  await page.getByLabel('Password to link single sign-on').fill('not-the-password');
  await link.click();
  await expect(page.getByText('Wrong password.')).toBeVisible();
  await expect(page).toHaveURL(/\/settings$/);
  await page.getByLabel('Password to link single sign-on').fill(user.password);
  await link.click();

  // The provider is asked for a new sign-in, and Settings shows which
  // identity was linked so the user can check it.
  await expect(page.locator('input[name="prompt"]')).toHaveValue('login');
  // Whatever name the provider gives, the session decides the account.
  const subject = randomUUID();
  const nameAtProvider = uniqueUser('elsewhere').username;
  await approveAtProvider(page, { username: nameAtProvider, subject });
  await expect(page).toHaveURL(/\/settings$/);
  await expect(page.getByText('Single sign-on is linked to this account.')).toBeVisible();
  await expect(page.getByText(`Linked to ${nameAtProvider} at Stub ID.`)).toBeVisible();
  // The link signed the account out elsewhere, and this browser in again.
  expect((await currentUser(page)).user_id).toBe(local.user_id);
  expect(await flowCookies(page.context())).toEqual([]);

  await page.context().clearCookies();
  await signInWithSso(page, { username: uniqueUser('other-name').username, subject });
  await expect(page).toHaveURL(/\/$/);
  expect((await currentUser(page)).user_id).toBe(local.user_id);
});

test('an account made by single sign-on connects its Navidrome account', async ({ page }) => {
  test.skip(!navidromeEnabled, 'stack has no Navidrome (set E2E_NAVIDROME=1)');
  const user = uniqueUser('sso-navi');
  await createNavidromeUser(user);

  await signInWithSso(page, { username: user.username, subject: randomUUID() });
  await expect(page).toHaveURL(/\/$/);
  // The banner leads to Connect Navidrome in Settings > Users.
  const banner = page.getByText('Not connected. Connect Navidrome to use discovery and rating sync.');
  await expect(banner).toBeVisible();
  await page.getByRole('link', { name: 'Connect', exact: true }).click();
  await expect(page).toHaveURL(/\/settings$/);
  await page.getByLabel('Navidrome password').fill(user.password);
  await page.getByRole('button', { name: 'Connect Navidrome' }).click();
  await expect(page.getByText('Navidrome connected')).toBeVisible();
  await expect(banner).toBeHidden();

  const me = await (await page.request.get('/api/auth/me')).json();
  expect(me.navidrome_status).toBe('Connected');
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
  expect(await flowCookies(page.context())).toEqual([]);
});

test('refuses a token issued to another client', async ({ page }) => {
  await signInWithSso(page, {
    username: uniqueUser('sso-azp').username,
    subject: randomUUID(),
    azp: 'another-client',
  });
  await expect(page).toHaveURL(/\/login\/error\/failed$/);
  expect((await page.request.get('/api/auth/me')).status()).toBe(401);
});

test('an unverified email does not matter while the username comes from another claim', async ({
  page,
}) => {
  // The app reads preferred_username (OIDC_USERNAME_CLAIM's default).
  const username = uniqueUser('sso-mail').username;
  await signInWithSso(page, {
    username,
    subject: randomUUID(),
    email: `${username}@example.com`,
    emailVerified: 'false',
  });
  await expect(page).toHaveURL(/\/$/);
  expect((await currentUser(page)).username).toBe(username);
});

test('refuses a callback that this browser did not start', async ({ page }) => {
  // No login state cookie: a forged or replayed callback.
  await page.goto('/api/auth/oidc/callback?code=stolen&state=whatever');
  await expect(page).toHaveURL(/\/login\/error\/failed$/);
  await expect(page.getByText('Single sign-on failed')).toBeVisible();
});

test('a single sign-on on the way to an app goes back to its request', async ({ page }) => {
  await page.goto('/login/return?return_to=' + encodeURIComponent(appRequest));
  await page.getByRole('button', { name: 'Sign in with Stub ID' }).click();
  await approveAtProvider(page, { username: uniqueUser('sso-mcp').username, subject: randomUUID() });
  await expect(page).toHaveURL(backAtAppRequest);
});

test('a failed single sign-on on the way to an app still goes back to its request', async ({
  page,
  request,
}) => {
  await page.goto('/login/return?return_to=' + encodeURIComponent(appRequest));
  await page.getByRole('button', { name: 'Sign in with Stub ID' }).click();
  await approveAtProvider(page, { username: 'nobody', subject: randomUUID() }, 'Deny');
  await expect(page).toHaveURL(/\/login\/error\/denied\/return\?return_to=/);
  await expect(page.getByText('cancelled or refused by the provider')).toBeVisible();

  // The password form of the error page goes back to the request.
  const user = uniqueUser('sso-back');
  await registerUser(request, user);
  await passwordSignInHere(page, user);
  await expect(page).toHaveURL(backAtAppRequest);
});

test('a single sign-on tried again from the error page goes back to the app request', async ({
  page,
}) => {
  await page.goto('/login/return?return_to=' + encodeURIComponent(appRequest));
  await page.getByRole('button', { name: 'Sign in with Stub ID' }).click();
  await approveAtProvider(page, { username: 'nobody', subject: randomUUID() }, 'Deny');
  await expect(page).toHaveURL(/\/login\/error\/denied\/return\?return_to=/);

  await page.getByRole('button', { name: 'Sign in with Stub ID' }).click();
  await approveAtProvider(page, { username: uniqueUser('sso-again').username, subject: randomUUID() });
  await expect(page).toHaveURL(backAtAppRequest);
});

test('a return address off the authorize path is ignored', async ({ page }) => {
  await page.goto(
    '/api/auth/oidc/login?return_to=' + encodeURIComponent('//evil.example/oauth/authorize'),
  );
  await approveAtProvider(page, { username: uniqueUser('sso-evil').username, subject: randomUUID() });
  await expect(page).toHaveURL(new URL('/', baseUrl).href);
});

test('an instance without single sign-on offers only the password form', async ({ page }) => {
  // The issue #90 instance runs without OIDC_ISSUER_URL.
  const options = await page.request.get(new URL('/api/auth/options', user90BaseUrl).href);
  expect(options.ok(), `options: ${options.status()}`).toBeTruthy();
  expect(await options.json()).toEqual({ sso_provider: null });

  await page.goto(new URL('/login', user90BaseUrl).href);
  await expect(page.getByPlaceholder('Enter password')).toBeVisible();
  await expect(page.getByRole('button', { name: /^Sign in with/ })).toHaveCount(0);

  // Its sign-in route says single sign-on is off.
  await page.goto(new URL('/api/auth/oidc/login', user90BaseUrl).href);
  await expect(page).toHaveURL(new URL('/login/error/disabled', user90BaseUrl).href);
  await expect(page.getByText('Single sign-on is not configured on this server.')).toBeVisible();
});
