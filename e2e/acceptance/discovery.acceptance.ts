// Discovery for a user with nothing but Navidrome: two plays of the Glass
// Atlas songs are enough for the catalog generator to suggest the artists
// ListenBrainz Labs names as neighbours of Static Harbor.

import { setTimeout as sleep } from 'node:timers/promises';
import { expect, test } from '@playwright/test';
import { glassAtlas } from '../fixtures/dataset.js';
import { lowtideChoir } from '../fixtures/discovery.js';
import { loginViaUi, seededFolder, uniqueUser } from '../helpers/app.js';
import { createNavidromeUser, subsonicAs } from '../helpers/navidrome.js';

interface Song {
  id: string;
  artist: string;
  title: string;
  playCount?: number;
}

/** The id of the Static Harbor song titled `title`, once Navidrome has scanned it. */
async function songId(user: ReturnType<typeof uniqueUser>, title: string): Promise<string> {
  for (let attempt = 0; attempt < 18; attempt++) {
    const answer = await subsonicAs(user, 'search3', { query: title, songCount: '5' });
    const songs = ((answer.searchResult3 as { song?: Song[] } | undefined)?.song ?? []).filter(
      (s) => s.artist === glassAtlas.artist && s.title === title,
    );
    if (songs[0]) return songs[0].id;
    await sleep(5_000);
  }
  throw new Error(`Navidrome never listed ${glassAtlas.artist} - ${title}`);
}

async function waitForPlay(user: ReturnType<typeof uniqueUser>, id: string): Promise<void> {
  await expect
    .poll(
      async () => {
        const answer = await subsonicAs(user, 'getSong', { id });
        return (answer.song as Song).playCount ?? 0;
      },
      { timeout: 60_000, intervals: [2_000] },
    )
    .toBeGreaterThanOrEqual(1);
}

test('a Navidrome-only user who plays music gets candidates', async ({ browser }) => {
  test.setTimeout(5 * 60_000);
  const user = uniqueUser('disc');
  await createNavidromeUser(user);

  const context = await browser.newContext();
  try {
    const page = await context.newPage();
    await loginViaUi(page, user);
    await seededFolder(
      context,
      'disc',
      glassAtlas.tracks.map((t) => ({
        recordingMbid: t.mbid,
        format: 'flac' as const,
        relativePath: `${glassAtlas.artist}/${glassAtlas.title}/${t.title}.flac`,
      })),
    );

    const played = [await songId(user, 'Northern Line'), await songId(user, 'Paper Lanterns')];
    for (const id of played) {
      await subsonicAs(user, 'scrobble', { id, submission: 'true' });
    }
    for (const id of played) {
      await waitForPlay(user, id);
    }

    const generated = await page.request.post('/api/discovery/generate-recommendations');
    expect(generated.status(), await generated.text()).toBe(200);
    expect(Number(await generated.json())).toBeGreaterThan(0);

    const reports = await page.request.get('/api/discovery/reports');
    expect(reports.status()).toBe(200);
    const newest = ((await reports.json()) as { text: string; created_at: string }[]).sort((a, b) =>
      b.created_at.localeCompare(a.created_at),
    )[0];
    expect(newest, 'a report was stored').toBeDefined();
    expect(newest?.text).toContain('Sources: Navidrome, ListenBrainz similar artists');
    expect(newest?.text).toContain(lowtideChoir.name);
  } finally {
    await context.close();
  }
});
