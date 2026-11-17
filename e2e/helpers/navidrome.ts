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

/** Subsonic authentication as `user`: the password sent hex encoded, JSON answers. */
function subsonicAuth(user: TestUser): URLSearchParams {
  return new URLSearchParams({
    u: user.username,
    p: `enc:${Buffer.from(user.password, 'utf8').toString('hex')}`,
    v: '1.16.1',
    c: 'soulful-e2e',
    f: 'json',
  });
}

/** GET {navidromeUrl}/rest/{method} as `user`; its subsonic-response, failing on any status but ok. */
export async function subsonicAs(
  user: TestUser,
  method: string,
  params: Record<string, string>,
): Promise<Record<string, unknown>> {
  const query = subsonicAuth(user);
  for (const [name, value] of Object.entries(params)) query.set(name, value);
  const response = await fetch(`${navidromeUrl}/rest/${method}?${query}`);
  // Read as text first: a proxy's or a server's error page is not JSON, and its status and body say why.
  const text = await response.text();
  if (response.status !== 200) {
    throw new Error(`Subsonic ${method} as ${user.username}: ${response.status} ${text}`);
  }
  const answer = (JSON.parse(text) as { 'subsonic-response': Record<string, unknown> })['subsonic-response'];
  if (answer.status !== 'ok') {
    throw new Error(`Subsonic ${method} as ${user.username}: ${JSON.stringify(answer)}`);
  }
  return answer;
}

/** GET {navidromeUrl}/rest/stream for the song as `user`; the body is discarded. */
export async function streamAs(user: TestUser, songId: string): Promise<{ status: number; contentType: string | null }> {
  const query = subsonicAuth(user);
  query.set('id', songId);
  const response = await fetch(`${navidromeUrl}/rest/stream?${query}`);
  await response.arrayBuffer();
  return { status: response.status, contentType: response.headers.get('content-type') };
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
