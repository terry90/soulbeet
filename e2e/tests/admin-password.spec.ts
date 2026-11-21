// The seeded admin's shipped password signs in only to choose a new one. The
// test gives the seeded admin admin/admin again, so it runs while no other
// spec signs in as admin.

import { expect, test } from '@playwright/test';
import { admin, loginViaApi, shippedAdmin, uniqueUser } from '../helpers/app.js';

const SEEDED_ADMIN_ID = '00000000-0000-0000-0000-000000000000';

test('the shipped admin password signs in only to choose a new one', async ({ browser, page }) => {
  // The seeded admin sets neither its own password without the current one
  // nor the shipped one, so a second admin hands it the shipped one.
  const second = uniqueUser('second-admin');
  const helper = await browser.newContext();
  // Set while the seeded admin has the shipped password again.
  let shipped = false;
  try {
    await loginViaApi(helper, admin);
    const created = await helper.request.post('/api/users/create', { data: second });
    expect(created.ok(), `create ${second.username}: ${created.status()}`).toBeTruthy();
    const { id } = (await created.json()) as { id: string };
    const promoted = await helper.request.post('/api/users/role', { data: { user_id: id, role: 'admin' } });
    expect(promoted.ok(), `make ${second.username} admin: ${promoted.status()}`).toBeTruthy();
    await helper.clearCookies();
    await loginViaApi(helper, second);
    const reset = await helper.request.post('/api/users/password/set', {
      data: { user_id: SEEDED_ADMIN_ID, password: shippedAdmin.password },
    });
    expect(reset.ok(), `give admin the shipped password: ${reset.status()}`).toBeTruthy();
    shipped = true;

    // While the shipped password is in place the connection endpoints say
    // they are closed, not why.
    const closed = await helper.request.get('/.well-known/oauth-protected-resource/mcp');
    expect(closed.status()).toBe(503);
    expect(await closed.json()).toEqual({
      error: 'mcp_unavailable',
      message: 'MCP access stays off until the security checks in Settings pass',
    });
    expect(closed.headers()['cache-control']).toBe('no-store');

    await page.goto('/login');
    await page.getByPlaceholder('Enter username').fill(shippedAdmin.username);
    await page.getByPlaceholder('Enter password').fill(shippedAdmin.password);
    await page.getByRole('button', { name: 'AUTHENTICATE' }).click();

    await expect(page.getByRole('heading', { name: 'Choose a new password' })).toBeVisible();
    await expect(page.getByRole('link', { name: 'Settings' })).toHaveCount(0);
    // The server refuses the session everything else, not just the page.
    expect((await page.request.get('/api/users')).status()).toBe(403);

    await page.getByPlaceholder('Current password').fill(shippedAdmin.password);
    await page.getByPlaceholder('New password').fill(admin.password);
    await page.getByRole('button', { name: 'Change Password' }).click();

    await expect(page).toHaveURL(/\/$/);
    await expect(page.getByPlaceholder('Search artist, album or track...')).toBeVisible();
    shipped = false;
    expect((await page.request.get('/api/users')).ok()).toBeTruthy();
  } finally {
    // Later specs sign in with the suite's password, whatever failed above.
    if (shipped) {
      const restored = await helper.request.post('/api/users/password/set', {
        data: { user_id: SEEDED_ADMIN_ID, password: admin.password },
      });
      expect(restored.ok(), `restore the admin password: ${restored.status()}`).toBeTruthy();
    }
    await helper.close();
  }
});
