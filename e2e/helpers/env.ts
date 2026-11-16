// Environment knobs shared by the config, helpers and specs. Defaults match
// the native stack (scripts/dev-stack.sh); CI overrides them for compose.

import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

export const e2eRoot = dirname(dirname(fileURLToPath(import.meta.url)));

/** Where the browser reaches the app. */
export const baseUrl = process.env.E2E_BASE_URL ?? 'http://127.0.0.1:9765';

/** The app instance with MCP on (compose.mcp.yml), as the suite reaches it. */
export const mcpBaseUrl = process.env.E2E_MCP_BASE_URL ?? 'http://127.0.0.1:9767';

/** The stubs' Navidrome proxy as the host reaches it; the app dials it inside compose. */
export const navidromeProxyUrl = process.env.E2E_NAVIDROME_PROXY_URL ?? 'http://127.0.0.1:4534';

/** A second app instance whose beets config is the one from issue #90. */
export const user90BaseUrl = process.env.E2E_USER90_BASE_URL ?? 'http://127.0.0.1:9766';

/** The issue #90 config itself, shared with the probe tests. */
export const user90BrokenConfig = join(
  dirname(e2eRoot),
  'lib',
  'soulbeet',
  'tests',
  'probe',
  'configs',
  'user90.yaml',
);

/** That instance's config directory on the host; the specs rewrite it at runtime. */
export const user90ConfigDirOnHost = join(e2eRoot, '.runtime', 'user90-config');

/** slskd URL as the APP must dial it (differs from the host view in compose). */
export const slskdUrlForApp = process.env.E2E_SLSKD_URL ?? 'http://127.0.0.1:5030';

export const slskdApiKey = process.env.E2E_SLSKD_API_KEY ?? 'e2e-slskd-api-key';

/** Stub control endpoint as seen from the host running the specs. */
export const slskdControlUrl = process.env.E2E_SLSKD_CONTROL_URL ?? 'http://127.0.0.1:5030';

/** MusicBrainz stub control endpoint as seen from the host running the specs. */
export const mbControlUrl = process.env.E2E_MB_CONTROL_URL ?? 'http://127.0.0.1:5050';

/** Music library root as the APP sees it (mount point in compose). */
export const musicDirForApp = process.env.E2E_MUSIC_DIR ?? join(e2eRoot, '.runtime', 'music');

/** The same music library root on the host, for file assertions. */
export const musicDirOnHost = join(e2eRoot, '.runtime', 'music');

/** The shared downloads directory on the host. */
export const downloadsDirOnHost = join(e2eRoot, '.runtime', 'downloads');

/** Where specs save screenshots of the screens they reach; unset saves none. */
export const shotsDir = process.env.E2E_SHOTS_DIR;

/** Navidrome-dependent specs only run when the stack includes one. */
export const navidromeEnabled = process.env.E2E_NAVIDROME === '1';

/** The admin account the first migration seeds. */
export const seededAdminId = '00000000-0000-0000-0000-000000000000';

export const navidromeAdminUser = 'admin';
export const navidromeAdminPassword = 'e2e-navidrome-admin';

/** Navidrome itself as the host reaches it, never through the stubs' proxy. */
export const navidromeUrl = process.env.E2E_NAVIDROME_URL ?? 'http://127.0.0.1:4533';

// The MCP instance runs only in compose (compose.mcp.yml), so what it dials
// and where it keeps music are constants of the suite.

/** The seeded admin's password once setup.mcp.ts changed it, which opens the MCP gate. */
export const mcpAdminPassword = 'e2e-mcp-admin-password';

/** slskd as the MCP instance dials it. */
export const mcpSlskdUrlForApp = 'http://stubs:5030';

/** The music root as the MCP instance sees it (the mount point). */
export const mcpMusicDirForApp = '/music';

/**
 * The compose command words for the MCP stack, run from e2e/ (for example
 * "docker compose -f compose.e2e.yml -f compose.mcp.yml"). Only the restart
 * helper reads it.
 */
export const mcpCompose = process.env.E2E_MCP_COMPOSE;
