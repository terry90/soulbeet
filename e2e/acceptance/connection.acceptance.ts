// Connecting an app: sign-in through the MCP SDK, the tool list, get_status,
// the challenge without a token, and how a long tool call keeps a proxy's
// connection alive.

import { expect, test } from '@playwright/test';
import { uniqueUser } from '../helpers/app.js';
import { mcpBaseUrl } from '../helpers/env.js';
import { INITIALIZE, connectAs, mcpUrl, rawPost } from '../helpers/mcp.js';
import { createNavidromeUser } from '../helpers/navidrome.js';

test('a user connects with the MCP SDK, sees every tool with its hints and their status', async ({ browser }) => {
  const user = uniqueUser('mcp-conn');
  await createNavidromeUser(user);
  const context = await browser.newContext();
  const mcp = await connectAs(await context.newPage(), user);
  try {
    const tools = await mcp.listTools();
    const names = tools.map((t) => t.name);
    expect(new Set(names).size).toBe(names.length);
    for (const t of tools) {
      expect(t.name, t.name).toMatch(/^[a-z][a-z0-9_]{0,63}$/);
      expect(t.title, t.name).toBeTruthy();
      for (const hint of ['readOnlyHint', 'destructiveHint', 'idempotentHint', 'openWorldHint'] as const) {
        expect(typeof t.annotations?.[hint], `${t.name} ${hint}`).toBe('boolean');
      }
      if (t.annotations?.readOnlyHint) expect(t.annotations.destructiveHint, t.name).toBe(false);
    }
    // The tools that may overwrite, remove or clear something; the Rust registry test owns the
    // full list.
    expect(tools.filter((t) => t.annotations?.destructiveHint).map((t) => t.name).sort()).toEqual([
      'add_playlist_tracks',
      'delete_playlist',
      'delete_tracks',
      'dismiss_job',
      'ensure_in_library',
      'fetch_artwork_and_lyrics',
      'move_tracks',
      'promote_discovery_track',
      'remove_discovery_track',
      'remove_playlist_tracks',
      'resolve_import',
      'retag_tracks',
      'set_playlist_tracks',
      'set_rating',
      'set_star',
      'sync_ratings',
      'update_playlist',
      'update_settings',
    ]);

    const status = await mcp.ok('get_status');
    expect(status).toMatchObject({
      ok: true,
      base_url: mcpBaseUrl,
      mcp_url: mcpUrl,
      user: { username: user.username, role: 'user' },
      download_backend: { backend: 'slskd', online: true },
      navidrome: { connected: true, admin: false },
      deletion_enabled: false,
    });
    // No linked account is a Navidrome admin: false once Navidrome refused the others, null
    // until then.
    expect(status.navidrome).not.toMatchObject({ can_trigger_scan: true });
  } finally {
    await mcp.close();
    await context.close();
  }
});

test('a call without a token gets the sign-in challenge', async () => {
  const r = await rawPost(null, INITIALIZE);
  expect(r.status).toBe(401);
  expect(r.headers.get('www-authenticate')).toBe(
    `Bearer error="invalid_token", error_description="No access token was sent", resource_metadata="${mcpBaseUrl}/.well-known/oauth-protected-resource/mcp", scope="soulful"`,
  );
});

/**
 * rawPost, reading the body as it streams: when the headers came, when the
 * first SSE comment line (":", a keep-alive) came, when the body ended, and the
 * JSON-RPC answer it carried (the last SSE "data:" event, or the whole body when
 * it is plain JSON).
 */
async function timedPost(
  token: string,
  headers: Record<string, string>,
  body: unknown,
): Promise<{
  headersAfterMs: number;
  firstKeepAliveAfterMs: number | null;
  endAfterMs: number;
  answer: unknown;
}> {
  const start = Date.now();
  const response = await rawPost(token, body, headers);
  const headersAfterMs = Date.now() - start;
  expect(response.status).toBe(200);
  if (response.body === null) throw new Error('the answer has no body');
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let firstKeepAliveAfterMs: number | null = null;
  let partial = '';
  let text = '';
  let lastData: string | null = null;
  for (let read = await reader.read(); !read.done; read = await reader.read()) {
    const chunk = decoder.decode(read.value, { stream: true });
    text += chunk;
    const lines = (partial + chunk).split('\n');
    partial = lines.pop() ?? '';
    if (firstKeepAliveAfterMs === null && lines.some((line) => line.startsWith(':'))) {
      firstKeepAliveAfterMs = Date.now() - start;
    }
    for (const line of lines) {
      if (line.startsWith('data:')) lastData = line.slice('data:'.length).trim();
    }
  }
  if (partial.startsWith('data:')) lastData = partial.slice('data:'.length).trim();
  const endAfterMs = Date.now() - start;
  const answer: unknown = JSON.parse(lastData ?? text);
  return { headersAfterMs, firstKeepAliveAfterMs, endAfterMs, answer };
}

test('a long wait sends headers at once and keep-alives for legacy clients, nothing until the answer for 2026-07-28 ones', async ({
  browser,
}) => {
  const user = uniqueUser('mcp-wait');
  await createNavidromeUser(user);
  const context = await browser.newContext();
  const mcp = await connectAs(await context.newPage(), user);
  try {
    // The user has no job, so the call waits the full 20 s.
    const call = { name: 'get_jobs', arguments: { states: ['searching'], wait_seconds: 20 } };

    const legacy = await timedPost(mcp.provider.accessToken(), {}, {
      jsonrpc: '2.0',
      id: 1,
      method: 'tools/call',
      params: call,
    });
    expect(legacy.headersAfterMs).toBeLessThan(2_000);
    // rmcp sends an SSE keep-alive every 15 s: the first one comes then, not at once.
    expect(legacy.firstKeepAliveAfterMs).toBeGreaterThanOrEqual(14_000);
    expect(legacy.firstKeepAliveAfterMs).toBeLessThan(17_000);
    expect(legacy.endAfterMs).toBeGreaterThanOrEqual(19_000);
    // The stream carried get_jobs' answer, not an error.
    expect(legacy.answer).toMatchObject({ jsonrpc: '2.0', id: 1, result: { isError: false } });

    const modern = await timedPost(
      mcp.provider.accessToken(),
      { 'mcp-protocol-version': '2026-07-28', 'mcp-method': 'tools/call', 'mcp-name': 'get_jobs' },
      {
        jsonrpc: '2.0',
        id: 2,
        method: 'tools/call',
        params: {
          ...call,
          _meta: {
            'io.modelcontextprotocol/protocolVersion': '2026-07-28',
            'io.modelcontextprotocol/clientInfo': { name: 'soulful-acceptance', version: '1' },
            'io.modelcontextprotocol/clientCapabilities': {},
          },
        },
      },
    );
    expect(modern.headersAfterMs).toBeGreaterThanOrEqual(19_000);
    expect(modern.answer).toMatchObject({ jsonrpc: '2.0', id: 2, result: { isError: false } });
  } finally {
    await mcp.close();
    await context.close();
  }
});
