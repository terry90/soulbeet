// What the pictures need that the stub stack does not have, answered in the
// browser: a Navidrome (its health, the account's link and the dashboard's
// library), folders with tidy paths, release notes already read, and the
// outside world (fonts and the footer's version check). Anything else the
// page asks of another host is refused and listed, so a picture never
// depends on the network.

import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';
import type { Page, Route } from '@playwright/test';
import { baseUrl, e2eRoot } from '../helpers/env.js';

/** The folders the account shows, as a typical install mounts them. */
const LIBRARY = { id: 'shot-library', user_id: 'shot', name: 'Library', path: '/music/library', shared: false };
const FOLDERS = [LIBRARY, { id: 'shot-shared', user_id: 'shot', name: 'Shared', path: '/music/shared', shared: true }];

/** The dashboard's library, as Navidrome would count it. */
const LIBRARY_STATS = {
  total_tracks: 2384,
  rated_tracks: 612,
  unrated_tracks: 1772,
  average_rating: 3.7,
  rating_distribution: [18, 47, 166, 241, 140],
  total_albums: 214,
  total_artists: 131,
  genres: [
    ['Indie Rock', 412],
    ['Dream Pop', 288],
    ['Electronic', 251],
    ['Shoegaze', 174],
    ['Folk', 163],
    ['Post-Rock', 121],
    ['Ambient', 97],
    ['Jazz', 84],
    ['Synth-pop', 66],
    ['Soul', 52],
  ],
};

const fontPackage = dirname(createRequire(import.meta.url).resolve('@fontsource/space-grotesk/package.json'));
const FONT_HOST = 'https://fonts.gstatic.com/fontsource/space-grotesk';

/**
 * The stylesheet the app imports from Google Fonts (Space Grotesk at 300,
 * 500 and 700), served from the @fontsource package.
 */
function fontStylesheet(): string {
  return ['300', '500', '700']
    .map((weight) => readFileSync(join(fontPackage, `${weight}.css`), 'utf8'))
    .join('\n')
    .replace(/url\(\.\/files\//g, `url(${FONT_HOST}/`);
}

/** Changes the JSON object the app answers `route` with by `edit`; a null answer stays null. */
async function edited(route: Route, edit: (body: Record<string, unknown>) => Record<string, unknown>): Promise<void> {
  const response = await route.fetch();
  const body = (await response.json()) as Record<string, unknown> | null;
  await route.fulfill({ response, json: body === null ? null : edit(body) });
}

/**
 * Stages the page as described above. Call before the page loads; the
 * returned list fills with the outside URLs the page asked for.
 */
export async function stageApp(page: Page): Promise<string[]> {
  const appHost = new URL(baseUrl).host;
  const outside: string[] = [];
  // Routes run newest first, so this one only sees what the others leave.
  await page.route(
    (url) => url.host !== appHost,
    (route) => {
      outside.push(route.request().url());
      return route.abort('blockedbyclient');
    },
  );

  await page.route('https://fonts.googleapis.com/css2?*', (route) =>
    route.fulfill({ contentType: 'text/css', body: fontStylesheet() }),
  );
  await page.route(`${FONT_HOST}/*`, (route) =>
    route.fulfill({ path: join(fontPackage, 'files', new URL(route.request().url()).pathname.split('/').pop() as string) }),
  );
  // The footer compares its version with the one on GitHub: the same, here.
  await page.route('https://raw.githubusercontent.com/**/api/Cargo.toml', (route) =>
    route.fulfill({ contentType: 'text/plain', body: readFileSync(join(dirname(e2eRoot), 'api', 'Cargo.toml'), 'utf8') }),
  );

  // Release notes the account has not read would open over the pictures.
  await page.route('**/api/changelog/seen', (route) =>
    route.fulfill({ json: { major: 9999, minor: 0, patch: 0 } }),
  );
  // The downloader and beets report their real health; Navidrome answers.
  await page.route('**/api/system/health', (route) =>
    edited(route, (health) => ({ ...health, navidrome_online: true, navidrome_failure: null })),
  );
  await page.route('**/api/auth/me', (route) =>
    edited(route, (me) => ({ ...me, navidrome_status: 'Connected' })),
  );
  await page.route('**/api/navidrome/library-stats', (route) => route.fulfill({ json: LIBRARY_STATS }));
  await page.route('**/api/folders', (route) =>
    route.request().method() === 'GET' ? route.fulfill({ json: FOLDERS }) : route.fallback(),
  );
  // Downloads go to the library, which the search bar shows.
  await page.route('**/api/settings', (route) =>
    route.request().method() === 'GET'
      ? edited(route, (settings) => ({ ...settings, default_download_folder_id: LIBRARY.id }))
      : route.fallback(),
  );
  return outside;
}
