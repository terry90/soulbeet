// Turns a download backend on or off through Settings > Config > Connections, as an admin
// does. The page must be signed in as an admin.

import type { Browser, BrowserContext, Locator, Page } from '@playwright/test';
import { expect } from '@playwright/test';
import { admin, loginViaApi, openSettings } from './app.js';
import { navidromeAdminPassword, navidromeAdminUser, navidromeEnabled } from './env.js';

/**
 * Signs `context` in as the seeded admin. Once navidrome.spec.ts has signed it in through
 * Navidrome, the account is linked to Navidrome's admin and only Navidrome's password opens it.
 */
export async function signInAdmin(context: BrowserContext): Promise<void> {
  const local = await context.request.post('/api/auth/login', { data: admin });
  if (local.ok()) {
    return;
  }
  expect(navidromeEnabled, `admin sign-in: ${local.status()}`).toBe(true);
  await loginViaApi(context, { username: navidromeAdminUser, password: navidromeAdminPassword });
}

/** Runs `act` on a page of a new context signed in as the admin. */
export async function asAdmin(browser: Browser, act: (page: Page) => Promise<void>): Promise<void> {
  const context = await browser.newContext();
  try {
    await signInAdmin(context);
    const page = await context.newPage();
    await page.goto('/');
    await act(page);
  } finally {
    await context.close();
  }
}

/** The backend's name over its switch. */
function heading(page: Page, name: string): Locator {
  return page.getByRole('heading', { name, exact: true });
}

/** The switch next to the backend's name: the one button of the line they share. */
function backendSwitch(page: Page, name: string): Locator {
  return heading(page, name).locator('xpath=..').getByRole('button');
}

/** The backend's whole block: its name, switch, help lines and, while on, its status. */
function backendBlock(page: Page, name: string): Locator {
  return page.locator('div.space-y-2').filter({ has: heading(page, name) }).last();
}

async function openConnections(page: Page, name: string): Promise<void> {
  await openSettings(page);
  await page.getByRole('button', { name: 'Config' }).click();
  await expect(heading(page, name)).toBeVisible();
}

const ON = /\bbg-beet-leaf\b/;
const OFF = /\bbg-gray-600\b/;

async function isOn(toggle: Locator): Promise<boolean> {
  return ON.test((await toggle.getAttribute('class')) ?? '');
}

/**
 * Runs `click`, which saves the switch of `name`, and waits for the server to answer the save.
 * The switch flips before the save is sent, so its look alone proves nothing.
 */
async function saved(page: Page, name: string, click: () => Promise<void>): Promise<void> {
  const answer = page.waitForResponse(
    (r) => new URL(r.url()).pathname === '/api/backends/switch' && r.request().method() === 'POST',
  );
  await click();
  const response = await answer;
  expect(response.ok(), `save the ${name} switch: ${response.status()}`).toBeTruthy();
}

/** Reloads Settings > Config, which reads the switches from the server, and checks `name`'s. */
async function storedAs(page: Page, name: string, state: RegExp): Promise<void> {
  await page.reload();
  await page.getByRole('button', { name: 'Config' }).click();
  await expect(backendSwitch(page, name)).toHaveClass(state);
}

/**
 * Turns the backend `name` on, confirming as the settings ask, and waits until the server
 * stored it and its status line says it is online. A backend already on is left as it is.
 */
export async function enableBackend(page: Page, name: string): Promise<void> {
  await openConnections(page, name);
  const toggle = backendSwitch(page, name);
  if (!(await isOn(toggle))) {
    await toggle.click();
    await saved(page, name, () =>
      page.getByRole('button', { name: `Turn on ${name}`, exact: true }).click(),
    );
  }
  await storedAs(page, name, ON);
  await expect(backendBlock(page, name).getByText('Online', { exact: true })).toBeVisible({
    timeout: 30_000,
  });
}

/** Turns the backend `name` off and waits until the server stored it; one already off is left. */
export async function disableBackend(page: Page, name: string): Promise<void> {
  await openConnections(page, name);
  const toggle = backendSwitch(page, name);
  if (await isOn(toggle)) {
    await saved(page, name, () => toggle.click());
  }
  await storedAs(page, name, OFF);
  await expect(backendBlock(page, name).getByText('Online', { exact: true })).toHaveCount(0);
}
