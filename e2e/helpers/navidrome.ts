// Talks to the stack's Navidrome directly (never through the stubs' proxy)
// for test setup the app has no API for.

import type { TestUser } from './app.js';
import { navidromeAdminPassword, navidromeAdminUser, navidromeUrl } from './env.js';

/** POST /auth/login as the stack's Navidrome admin (navidromeAdminUser), its token. */
async function adminToken(): Promise<string> {
  const response = await fetch(`${navidromeUrl}/auth/login`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ username: navidromeAdminUser, password: navidromeAdminPassword }),
  });
  if (response.status !== 200) {
    throw new Error(`Navidrome admin login: ${response.status} ${await response.text()}`);
  }
  return ((await response.json()) as { token: string }).token;
}

/**
 * Creates a regular Navidrome user through the native API. Navidrome gives a
 * new regular user the libraries marked for new users.
 */
export async function createNavidromeUser(user: TestUser): Promise<void> {
  const response = await fetch(`${navidromeUrl}/api/user`, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      'x-nd-authorization': `Bearer ${await adminToken()}`,
    },
    body: JSON.stringify({ userName: user.username, name: user.username, password: user.password, isAdmin: false }),
  });
  if (response.status !== 200) {
    throw new Error(`create Navidrome user ${user.username}: ${response.status} ${await response.text()}`);
  }
}
