import { expect, test } from '@playwright/test';
import { registerUser, uniqueUser } from '../helpers/app.js';

test('signing in on the way to an app goes back to its request', async ({ page }) => {
  const user = uniqueUser('mcp');
  await registerUser(page.context().request, user);
  const target = '/oauth/authorize?client_id=dcr_e2e&state=a%26b';
  await page.goto('/login/return?return_to=' + encodeURIComponent(target));
  await page.getByPlaceholder('Enter username').fill(user.username);
  await page.getByPlaceholder('Enter password').fill(user.password);
  await page.getByRole('button', { name: 'AUTHENTICATE' }).click();
  await expect(page).toHaveURL(/\/oauth\/authorize\?client_id=dcr_e2e&state=a%26b$/);
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
