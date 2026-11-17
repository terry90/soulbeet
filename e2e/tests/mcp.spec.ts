import { expect, test } from '@playwright/test';
import { registerUser, uniqueUser } from '../helpers/app.js';

test('the connection endpoints say they are closed, not why', async ({ request }) => {
  const r = await request.get('/.well-known/oauth-protected-resource/mcp');
  expect(r.status()).toBe(503);
  const body = await r.json();
  expect(body).toEqual({
    error: 'mcp_unavailable',
    message: 'Claude (MCP) access stays off until the security checks in Settings pass',
  });
  expect(r.headers()['cache-control']).toBe('no-store');
});

test('signing in on the way to an app goes back to its request', async ({ page }) => {
  const user = uniqueUser('mcp');
  await registerUser(page.context().request, user);
  const target = '/oauth/authorize?client_id=dcr_e2e&state=a%26b';
  await page.goto('/login/return?return_to=' + encodeURIComponent(target));
  await page.getByPlaceholder('Enter username').fill(user.username);
  await page.getByPlaceholder('Enter password').fill(user.password);
  await page.getByRole('button', { name: 'AUTHENTICATE' }).click();
  await expect(page).toHaveURL(/\/oauth\/authorize\?client_id=dcr_e2e&state=a%26b$/);
  await expect(page.locator('body')).toContainText('mcp_unavailable');
});

test('a return address off the authorize path is ignored', async ({ page }) => {
  const user = uniqueUser('mcp-evil');
  await registerUser(page.context().request, user);
  await page.goto('/login/return?return_to=' + encodeURIComponent('//evil.example/oauth/authorize'));
  await page.getByPlaceholder('Enter username').fill(user.username);
  await page.getByPlaceholder('Enter password').fill(user.password);
  await page.getByRole('button', { name: 'AUTHENTICATE' }).click();
  await expect(page).toHaveURL(/\/$/);
});
