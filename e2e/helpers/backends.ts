// Turns a download backend on or off through Settings > Config > Connections, as an admin
// does. The page must be signed in as an admin.

import type { Locator, Page } from '@playwright/test';
import { expect } from '@playwright/test';
import { openSettings } from './app.js';

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

async function isOn(toggle: Locator): Promise<boolean> {
  return /\bbg-beet-leaf\b/.test((await toggle.getAttribute('class')) ?? '');
}

/**
 * Turns the backend `name` on, confirming as the settings ask, and waits until its status
 * line says it is online. A backend already on is left as it is.
 */
export async function enableBackend(page: Page, name: string): Promise<void> {
  await openConnections(page, name);
  const toggle = backendSwitch(page, name);
  if (!(await isOn(toggle))) {
    await toggle.click();
    await page.getByRole('button', { name: `Turn on ${name}`, exact: true }).click();
    await expect(toggle).toHaveClass(/\bbg-beet-leaf\b/);
  }
  await expect(backendBlock(page, name).getByText('Online', { exact: true })).toBeVisible({
    timeout: 30_000,
  });
}

/** Turns the backend `name` off; one already off is left as it is. */
export async function disableBackend(page: Page, name: string): Promise<void> {
  await openConnections(page, name);
  const toggle = backendSwitch(page, name);
  if (await isOn(toggle)) {
    await toggle.click();
  }
  await expect(toggle).toHaveClass(/\bbg-gray-600\b/);
  await expect(backendBlock(page, name).getByText('Online', { exact: true })).toHaveCount(0);
}
