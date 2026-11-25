// The Christmas playlist flow through MCP, as an app plays it: a user signs in,
// checks which carols her library has, asks for the rest in one batch with a
// playlist, and every failure the stubs inject (a peer that refuses, a dropped
// transfer, a busy peer, only filtered results, a file beets cannot match,
// Navidrome down, and Soulful restarted mid-download and mid-index-wait) is
// recovered by following the error's code, retry advice, facts and next alone.
// Then she deletes and replaces tracks once she allowed the app to, and the
// tracks another listener rated, starred or kept in a playlist stay.

import { randomUUID } from 'node:crypto';
import { existsSync, statSync } from 'node:fs';
import { basename, join } from 'node:path';
import { setTimeout as sleep } from 'node:timers/promises';
import { expect, test } from '@playwright/test';
import { fenwickCarols, wytheChapelCarols, type FixtureRelease, type FixtureTrack } from '../fixtures/dataset.js';
import { confirmIn, loginViaUi, openSettings, seededFolder, uniqueUser } from '../helpers/app.js';
import { resetStubs, setNavidromeDown } from '../helpers/control.js';
import { mcpAdminPassword, mcpMusicDirForApp, navidromeAdminPassword, navidromeAdminUser } from '../helpers/env.js';
import { libraryItems, listFilesRecursive } from '../helpers/files.js';
import {
  followerView,
  importDecision,
  nextStep,
  type ErrorObject,
  type ImportChoices,
  type JobDoc,
  type NextCall,
  type RetryAdvice,
} from '../helpers/follower.js';
import { connectAs, rawPost, type ToolAnswer } from '../helpers/mcp.js';
import { createNavidromeUser, streamAs, subsonicAs } from '../helpers/navidrome.js';
import { restartMcpInstance } from '../helpers/stack.js';

const F = fenwickCarols,
  W = wytheChapelCarols;
const t = (r: FixtureRelease, n: number) => r.tracks[n - 1] as FixtureTrack;
const ref = (r: FixtureRelease, n: number) => ({ artist: r.artist, title: t(r, n).title });

/** list[i], failing with the list when it is shorter. */
function nth<T>(list: readonly T[], i: number): T {
  const item = list[i];
  if (item === undefined) throw new Error(`no item ${i} in ${JSON.stringify(list)}`);
  return item;
}

/** The wanted playlist, in order. */
const WANTED: { artist: string; title: string; recording_mbid?: string; duration_s?: number }[] = [
  ref(F, 1),
  ref(F, 2),
  ref(F, 3),
  ref(F, 4),
  ref(F, 5),
  ref(F, 6),
  ref(F, 7),
  ref(W, 1),
];

/** What each injected failure must look like the first time its job needs action. */
const EXPECTED: Record<
  string,
  {
    state: string;
    code: string;
    stage: string;
    retry: RetryAdvice;
    raw?: string;
    source?: string;
    offers: { tool: string; mode?: string };
  }
> = {
  'Hark the Herald Angels Sing': {
    state: 'failed',
    code: 'transfer_rejected_not_shared',
    stage: 'transfer',
    retry: 'other_source',
    raw: 'Transfer rejected: File not shared.',
    source: 'carol_rejects',
    offers: { tool: 'retry_job', mode: 'next_source' },
  },
  'Good King Wenceslas': {
    state: 'failed',
    code: 'transfer_connection_failed',
    stage: 'transfer',
    retry: 'other_source',
    raw: 'Transfer failed: Read error: Remote connection closed',
    source: 'carol_drops',
    offers: { tool: 'retry_job', mode: 'next_source' },
  },
  'In the Bleak Midwinter': {
    state: 'failed',
    code: 'transfer_rejected_peer_busy',
    stage: 'transfer',
    retry: 'same_source_later',
    raw: 'Transfer rejected: Too many files',
    source: 'carol_busy',
    offers: { tool: 'retry_job', mode: 'same_source' },
  },
  'Deck the Halls': {
    state: 'failed',
    code: 'search_all_filtered',
    stage: 'source_search',
    retry: 'other_query',
    offers: { tool: 'retry_job', mode: 'new_search' },
  },
  'The Holly and the Ivy': {
    state: 'needs_decision',
    code: 'import_no_match',
    stage: 'import',
    retry: 'needs_decision',
    offers: { tool: 'get_import_choices' },
  },
};

/** Which one Soulful answers depends on whether its Navidrome client's breaker opened. */
const NAVIDROME_DOWN = ['navidrome_unreachable', 'navidrome_circuit_open'];

interface Entry {
  index: number;
  ok: boolean;
  error?: ErrorObject;
  [field: string]: unknown;
}
interface Items {
  ok: boolean;
  results: Entry[];
  error?: ErrorObject;
  [field: string]: unknown;
}
interface Batch {
  summary: Record<string, number>;
  needs_action: JobDoc[];
  cursor: number;
  all_terminal: boolean;
  playlist: { playlist_id: string; settled: number; waiting: number; failure: unknown; hidden_entries_dropped: number };
  [field: string]: unknown;
}
/**
 * What a job needs action for: its failure, or, for a job waiting for an import decision, the
 * warning its slot carries (the slot did not fail, so the job's failure is null).
 */
function actionFor(job: JobDoc): ErrorObject | null {
  if (job.state !== 'needs_decision') return job.failure;
  expect(job.failure, JSON.stringify(job)).toBeNull();
  return (job.slots as { warning: ErrorObject | null }[]).find((slot) => slot.warning !== null)?.warning ?? null;
}
const matched = (e: Entry) => (e.matches as { song_id: string; match: { kind: string } }[] | undefined)?.[0];

test('the Christmas playlist is filled through every injected failure by following each error', async ({ browser }) => {
  test.setTimeout(20 * 60_000);
  await resetStubs();
  // Navidrome's library is shared by every user and folder, so a second run would find this one's carols.
  const fenwick = await subsonicAs({ username: navidromeAdminUser, password: navidromeAdminPassword }, 'search3', {
    query: 'Fenwick',
    songCount: '1',
  });
  if (((fenwick.searchResult3 as { song?: unknown[] }).song ?? []).length > 0) {
    throw new Error(
      'the acceptance flow needs a fresh stack: docker compose -p soulful-mcp-e2e ... down -v and an empty e2e/.runtime',
    );
  }

  const { alice, bob, contextA, pageA, a } = await test.step('users', async () => {
    const alice = uniqueUser('xmas-a'),
      bob = uniqueUser('xmas-b');
    await createNavidromeUser(alice);
    await createNavidromeUser(bob);
    const contextA = await browser.newContext();
    const pageA = await contextA.newPage();
    // Her first sign-in makes her account (REGISTRATION defaults to navidrome).
    const a = await connectAs(pageA, alice);
    return { alice, bob, contextA, pageA, a };
  });

  const { folderId, folder } = await test.step('library', async () => {
    const folder = await seededFolder(
      contextA,
      'xmas',
      [1, 4].map((n) => ({
        recordingMbid: t(F, n).mbid,
        format: 'flac' as const,
        relativePath: `${F.artist}/${F.title}/${t(F, n).title}.flac`,
      })),
    );
    const status = (await a.ok('get_status')) as { folders: { folder_id: string; name: string; path: string }[] };
    const listed = status.folders.find((f) => f.name === folder.name);
    expect(listed?.path).toBe(`${mcpMusicDirForApp}/${folder.name}`);
    return { folderId: listed?.folder_id as string, folder };
  });

  const { silentId, coventryId } = await test.step('owned tracks show up', async () => {
    // Navidrome scans every 15 s (compose.e2e.yml ND_SCANSCHEDULE).
    const owned = [nth(WANTED, 0), nth(WANTED, 3)];
    await expect
      .poll(
        async () =>
          ((await a.ok('find_in_library', { references: owned })) as Items).results.map((e) => matched(e)?.match.kind),
        { timeout: 90_000, intervals: [5_000] },
      )
      .toEqual(['exact', 'exact']);
    const ids = ((await a.ok('find_in_library', { references: owned })) as Items).results.map(
      (e) => matched(e)?.song_id as string,
    );
    return { silentId: nth(ids, 0), coventryId: nth(ids, 1) };
  });

  await test.step('pin a recording', async () => {
    const found = (await a.ok('search_music', { kind: 'recording', query: 'O Holy Night', artist: F.artist })) as {
      recordings: { recording_mbid: string; duration_s: number }[];
    };
    expect(found.recordings).toContainEqual(expect.objectContaining({ recording_mbid: t(F, 2).mbid, duration_s: 30 }));
    WANTED[1] = { ...nth(WANTED, 1), recording_mbid: t(F, 2).mbid, duration_s: 30 };
  });

  await test.step('check the list', async () => {
    const checked = (await a.ok('find_in_library', { references: WANTED })) as Items;
    expect(checked.results.map((e) => matched(e)?.song_id ?? null)).toEqual([
      silentId,
      null,
      null,
      coventryId,
      null,
      null,
      null,
      null,
    ]);
  });

  const { batchId, playlistId, jobOf } = await test.step('ensure', async () => {
    const ensureArgs = {
      items: WANTED,
      folder_id: folderId,
      lossless_only: true,
      failover: 'off',
      label: 'Christmas traditional',
      playlist: { create: { name: 'Christmas traditional' } },
      request_key: randomUUID(),
    };
    const ensured = (await a.ok('ensure_in_library', ensureArgs)) as Items & {
      batch_id: string;
      next: NextCall[];
      playlist: { ok: boolean; playlist_id: string; song_ids: string[] };
    };
    expect(ensured.results.map((e) => e.status)).toEqual([
      'in_library',
      'job_created',
      'job_created',
      'in_library',
      'job_created',
      'job_created',
      'job_created',
      'job_created',
    ]);
    expect([nth(ensured.results, 0).song_ids, nth(ensured.results, 3).song_ids]).toEqual([[silentId], [coventryId]]);
    expect(ensured.playlist).toMatchObject({ ok: true, song_ids: [silentId, coventryId] });
    expect(nth(ensured.next, 0).tool).toBe('get_batch');
    // The same request_key answers the same document.
    expect(await a.ok('ensure_in_library', ensureArgs)).toEqual(ensured);
    expect(
      ((await a.ok('list_batches', { label: 'Christmas traditional' })) as { batches: unknown[] }).batches,
    ).toHaveLength(1);
    const jobOf = (title: string) =>
      nth(ensured.results, WANTED.findIndex((w) => w.title === title)).job_id as string;
    return { batchId: ensured.batch_id, playlistId: ensured.playlist.playlist_id, jobOf };
  });
  let cursor = 0;

  try {
    const entryError = await test.step('Navidrome down', async () => {
      await setNavidromeDown(true);
      // Until O Holy Night waits for Navidrome to list its file and the slow peer's transfer is the
      // only one running (the other peers' failures come within seconds).
      const inState = async (state: string) =>
        ((await a.ok('get_jobs', { batch_id: batchId, states: [state] })) as { jobs: JobDoc[] }).jobs;
      let indexing: JobDoc[] = [],
        downloading: JobDoc[] = [];
      for (const end = Date.now() + 150_000; ; ) {
        expect(Date.now(), 'O Holy Night reached indexing while only Holly downloads').toBeLessThan(end);
        const b = (await a.ok('get_batch', {
          batch_id: batchId,
          since: cursor,
          wait_seconds: 30,
          wait_for: 'any_change',
        })) as Batch;
        cursor = b.cursor;
        if ((b.summary.indexing ?? 0) >= 1 && (b.summary.downloading ?? 0) >= 1) {
          indexing = await inState('indexing');
          downloading = await inState('downloading');
          const only = (jobs: JobDoc[], title: string) => jobs.length === 1 && jobs[0]?.wanted.title === title;
          if (only(indexing, 'O Holy Night') && only(downloading, 'The Holly and the Ivy')) break;
        }
      }
      const holyId = nth(indexing, 0).job_id,
        hollyId = nth(downloading, 0).job_id;
      expect([holyId, hollyId]).toEqual([jobOf('O Holy Night'), jobOf('The Holly and the Ivy')]);

      // Soulful restarts with one job mid-index-wait and one mid-download; the open chat keeps working.
      await restartMcpInstance();
      const after = ((await a.ok('get_jobs', { job_ids: [holyId, hollyId], detail: 'full' })) as { jobs: JobDoc[] })
        .jobs;
      const stateOf = (id: string) => after.find((j) => j.job_id === id)?.state;
      expect(stateOf(holyId)).toBe('indexing');
      expect(['downloading', 'importing', 'needs_decision']).toContain(stateOf(hollyId));

      const down = (await a.ok('get_status')) as { navidrome: { connected: boolean; failure: ErrorObject | null } };
      expect(down.navidrome.connected).toBe(false);
      expect(NAVIDROME_DOWN).toContain(down.navidrome.failure?.code);
      expect(down.navidrome.failure?.retry).toBe('same_source_later');
      const lookup = await a.call('find_in_library', { references: [nth(WANTED, 0)] });
      expect(lookup.isError).toBe(true);
      const partial = lookup.doc as Items;
      expect(partial.error?.code).toBe('partial');
      const entryError = nth(partial.results, 0).error as ErrorObject;
      expect(NAVIDROME_DOWN).toContain(entryError.code);
      expect([entryError.retry, entryError.next]).toEqual([
        'same_source_later',
        [{ tool: 'find_in_library', args: { references: [nth(WANTED, 0)] } }],
      ]);
      // The job keeps waiting for Navidrome: the server's 20 s wait sees no change.
      const holy = (await a.ok('get_jobs', {
        job_ids: [holyId],
        detail: 'full',
        wait_seconds: 20,
        wait_for: 'any_change',
      })) as { jobs: JobDoc[] };
      expect(nth(holy.jobs, 0).state).toBe('indexing');
      return entryError;
    });

    await test.step('Navidrome back', async () => {
      await setNavidromeDown(false);
      // The entry's next, as given; the breaker reopens 60 s after its last failure.
      const again = nth(entryError.next, 0);
      const answers: ToolAnswer[] = [];
      await expect
        .poll(
          async () => {
            const answer = await a.call(again.tool, again.args);
            answers.push(answer);
            return answer.isError;
          },
          { intervals: [10_000], timeout: 130_000 },
        )
        .toBe(false);
      const back = nth(answers, answers.length - 1);
      expect(matched(nth((back.doc as Items).results, 0))).toMatchObject({ song_id: silentId, match: { kind: 'exact' } });
    });
  } finally {
    await setNavidromeDown(false);
  }

  await test.step('follow the advice', async () => {
    const seen = new Set<string>();
    // Job id to the failure codes acted on.
    const acted = new Map<string, string[]>();
    for (let round = 0; ; round++) {
      expect(round, 'get_batch rounds').toBeLessThan(240);
      const b = (await a.ok('get_batch', {
        batch_id: batchId,
        since: cursor,
        wait_seconds: 30,
        wait_for: 'needs_action',
      })) as Batch;
      cursor = b.cursor;
      if (b.needs_action.length === 0 && b.all_terminal) break;
      for (const job of b.needs_action) {
        const failure = actionFor(job);
        const code = failure?.code ?? job.state;
        const before = acted.get(job.job_id) ?? [];
        expect(before, `acted twice on ${code}: ${JSON.stringify(job)}`).not.toContain(code);
        if (!seen.has(job.wanted.title)) {
          seen.add(job.wanted.title);
          const want = EXPECTED[job.wanted.title];
          if (want === undefined) throw new Error(`no failure was injected for ${JSON.stringify(job)}`);
          expect({ state: job.state, code, stage: failure?.stage, retry: failure?.retry }).toEqual({
            state: want.state,
            code: want.code,
            stage: want.stage,
            retry: want.retry,
          });
          if (want.raw) expect(failure?.cause?.raw).toBe(want.raw);
          if (want.source) expect(failure?.facts.source).toBe(want.source);
          expect(
            job.next.some(
              (n) => n.tool === want.offers.tool && (want.offers.mode === undefined || n.args.mode === want.offers.mode),
            ),
          ).toBe(true);
          if (job.wanted.title === 'Deck the Halls') {
            expect(failure?.facts.dropped_by_filter).toMatchObject({ not_lossless: expect.any(Number) });
            expect((failure?.facts.dropped_by_filter as { not_lossless: number }).not_lossless).toBeGreaterThanOrEqual(
              1,
            );
            expect(failure?.facts.query_text_sent).toEqual(expect.stringMatching(/\S/));
          }
        }
        const step = nextStep(followerView(job));
        if (step.kind === 'stuck') throw new Error(`${step.why}: ${JSON.stringify(job)}`);
        if (step.kind === 'call') {
          // The wait the advice gives is what is under test.
          await sleep(step.afterSeconds * 1000);
          await a.ok(step.tool, step.args);
        } else {
          const choices = (await a.ok(step.choices.tool, step.choices.args)) as unknown as ImportChoices;
          expect(choices.allowed_choices).toContain('as_is');
          // The MusicBrainz stub does not know Wythe Chapel Choir.
          expect(choices.candidates).toEqual([]);
          const decision = importDecision(job.job_id, choices);
          if (decision === null) throw new Error(`no import rule: ${JSON.stringify(choices)}`);
          await a.ok(decision.tool, decision.args);
        }
        acted.set(job.job_id, [...before, code]);
      }
    }
    expect([...seen].sort()).toEqual(Object.keys(EXPECTED).sort());
  });

  const songOf = await test.step('the playlist is complete', async () => {
    const done = (await a.ok('get_batch', { batch_id: batchId })) as Batch;
    expect(done.summary).toEqual(expect.objectContaining({ done: 6 }));
    expect(Object.entries(done.summary).filter(([state, n]) => state !== 'done' && n > 0)).toEqual([]);
    // The playlist runner places finished jobs in a round of its own, 2 s after a change, at the
    // latest 60 s after its last round.
    await expect
      .poll(async () => ((await a.ok('get_batch', { batch_id: batchId })) as Batch).playlist, { timeout: 90_000 })
      .toEqual({ playlist_id: playlistId, settled: 8, waiting: 0, failure: null, hidden_entries_dropped: 0 });
    const list = (await a.ok('get_playlist', { playlist_id: playlistId })) as {
      entries: { position: number; song_id: string; title: string }[];
    };
    const jobs = ((await a.ok('get_jobs', { batch_id: batchId, detail: 'full' })) as { jobs: JobDoc[] }).jobs;
    const imports = jobs.map((j) => ({ title: j.wanted.title, state: j.state, results: j.results }));
    expect(list.entries.map((e) => [e.position, e.title]), JSON.stringify(imports)).toEqual(
      WANTED.map((w, i) => [i, w.title]),
    );
    expect(new Set(list.entries.map((e) => e.song_id)).size).toBe(8);
    for (const e of list.entries) {
      const s = await streamAs(alice, e.song_id);
      expect([s.status, s.contentType?.startsWith('audio/')], e.title).toEqual([200, true]);
    }
    const songOf = (title: string) => list.entries.find((e) => e.title === title)?.song_id as string;
    // The tracks she owned keep their songs.
    expect([songOf('Silent Night'), songOf('Coventry Carol')]).toEqual([silentId, coventryId]);
    const full = (title: string) =>
      jobs.find((j) => j.wanted.title === title) as JobDoc & {
        tried: { code: string; source: string }[];
        slots: { current_attempt: { source: string } }[];
        results: { matches_wanted: { recording_mbid: string }; untagged: boolean }[];
      };
    for (const [title, code, source] of [
      ['Hark the Herald Angels Sing', 'transfer_rejected_not_shared', 'carol_rejects'],
      ['Good King Wenceslas', 'transfer_connection_failed', 'carol_drops'],
    ] as const) {
      expect(nth(full(title).tried, 0), title).toMatchObject({ code, source });
      expect(nth(full(title).slots, 0).current_attempt.source, title).toBe('carol_backup');
    }
    expect(nth(full('In the Bleak Midwinter').slots, 0).current_attempt.source).toBe('carol_busy');
    expect(nth(full('O Holy Night').results, 0).matches_wanted.recording_mbid).toBe('equal');
    expect(nth(full('The Holly and the Ivy').results, 0).untagged).toBe(true);
    // The restart cost the slow transfer no attempt.
    expect(nth(full('The Holly and the Ivy').slots, 0).current_attempt.source).toBe('carol_slow');
    expect(full('The Holly and the Ivy').tried).toEqual([]);
    return songOf;
  });

  await test.step('a job whose search finds nothing is left to judgement', async () => {
    // No peer shares this carol.
    const ensured = (await a.ok('ensure_in_library', {
      items: [{ artist: F.artist, title: "Once in Royal David's City" }],
      folder_id: folderId,
      failover: 'off',
      request_key: randomUUID(),
    })) as Items & { batch_id: string };
    const jobId = nth(ensured.results, 0).job_id as string;
    let job: JobDoc | undefined;
    for (let round = 0; job === undefined; round++) {
      expect(round, 'get_batch rounds').toBeLessThan(18);
      const b = (await a.ok('get_batch', {
        batch_id: ensured.batch_id,
        wait_seconds: 30,
        wait_for: 'needs_action',
      })) as Batch;
      job = b.needs_action.find((j) => j.job_id === jobId);
    }
    expect(job.failure).toMatchObject({ code: 'search_no_results', stage: 'source_search', retry: 'other_query' });
    expect(nextStep(followerView(job))).toMatchObject({ kind: 'stuck' });
    const dismiss = job.next.find((n) => n.tool === 'dismiss_job');
    expect(dismiss, JSON.stringify(job)).toBeDefined();
    expect((await a.ok(dismiss!.tool, dismiss!.args)).state).toBe('dismissed');
  });

  await test.step('another user sees none of it', async () => {
    // Bob signs in for the first time here, which makes his linked account.
    const b = await connectAs(await (await browser.newContext()).newPage(), bob);
    const holyJob = jobOf('O Holy Night');
    expect((await b.call('get_jobs', { job_ids: [holyJob] })).doc.error).toMatchObject({
      code: 'unknown_handle',
      message: 'Job not found',
      facts: { unknown_job_ids: [holyJob] },
      next: [],
    });
    expect((await b.call('get_batch', { batch_id: batchId })).doc.error).toMatchObject({
      code: 'unknown_handle',
      message: 'Batch not found',
      next: [{ tool: 'list_batches', args: {} }],
    });
    expect((await b.call('get_playlist', { playlist_id: playlistId })).doc.error).toMatchObject({
      code: 'playlist_not_found',
    });
  });

  await test.step('deletion keeps a track another user rated or starred', async () => {
    // Files are named as Soulful sees them, under her folder's path.
    const inFolder = (file: unknown) => typeof file === 'string' && file.startsWith(`${folder.path}/`);
    const holyId = songOf('O Holy Night'),
      bleakId = songOf('In the Bleak Midwinter'),
      harkId = songOf('Hark the Herald Angels Sing'),
      goodKingId = songOf('Good King Wenceslas');
    // On Navidrome itself, as a listener would.
    await subsonicAs(bob, 'setRating', { id: holyId, rating: '5' });

    // Without the switch.
    const off = {
      code: 'permission_not_granted',
      stage: 'delete',
      retry: 'needs_user',
      message: 'Managing library files (delete, move, re-tag) is turned off for this app',
      user_action: {
        code: 'enable_deletion',
        what: 'Allow managing library files for this app',
        where: 'Settings > Users > Connected apps',
      },
      next: [],
    };
    const needSwitch: [string, Record<string, unknown>][] = [
      ['preview_delete_tracks', { song_ids: [holyId, bleakId], reason: 'cleanup' }],
      ['delete_tracks', { confirmation_token: 'x' }],
      ['ensure_in_library', { items: [ref(F, 3)], folder_id: folderId, replace_existing: { confirmation_token: 'x' } }],
    ];
    for (const [tool, args] of needSwitch) {
      const answer = await a.call(tool, args);
      expect(answer.isError, tool).toBe(true);
      expect(answer.doc.error, tool).toMatchObject(off);
    }

    // Refused to every app, whatever its switch.
    const appsMayNot = async (when: string) => {
      const refused: [string, Record<string, unknown>, string][] = [
        ['set_rating', { song_id: coventryId, rating: 1 }, 'rate_in_navidrome'],
        ['update_settings', { auto_delete: true }, 'enable_auto_delete'],
        ['update_settings', { discovery_promote_threshold: 3 }, 'change_discovery_settings'],
        ['update_settings', { discovery_folder_id: folderId }, 'change_discovery_settings'],
      ];
      for (const [tool, args, userAction] of refused) {
        const answer = await a.call(tool, args);
        const error = answer.doc.error as ErrorObject | undefined;
        expect([answer.isError, error?.code, error?.user_action?.code], `${tool} ${JSON.stringify(args)} ${when}`).toEqual(
          [true, 'permission_not_granted', userAction],
        );
      }
    };
    await appsMayNot('with deletion off');

    // The switch, with its confirmation.
    await pageA.goto('/');
    await openSettings(pageA);
    await pageA.getByRole('button', { name: 'Users' }).click();
    const allow = pageA.getByRole('checkbox', { name: 'Allow managing library files (delete, move, re-tag)' });
    await allow.click();
    await expect(pageA.getByRole('button', { name: 'Allow managing files', exact: true })).toBeVisible();
    await confirmIn(
      pageA,
      'Let 127.0.0.1 manage library files (delete, move between folders, and re-tag)? Before deleting, moving or re-tagging tracks it has to ask you first. While this is on it can also run your rating sync, which deletes the songs you rated 1 star when auto-delete is on, and keep a discovery track, which replaces an earlier copy of the song. Deletions are limited to at most 100 tracks in any 24 hours, for all your connected apps together. Deleted files cannot be restored.',
    );
    // The box is disabled while it saves and then shows the stored value.
    await expect(allow).toBeChecked();
    expect(await a.ok('get_status')).toMatchObject({ deletion_enabled: true });
    await appsMayNot('with deletion on');

    // Only the owner has the switch: an admin sees her app, and can disconnect it, but not allow it to delete.
    const adminContext = await browser.newContext();
    try {
      const adminPage = await adminContext.newPage();
      await loginViaUi(adminPage, { username: 'admin', password: mcpAdminPassword });
      await openSettings(adminPage);
      await adminPage.getByRole('button', { name: 'Users' }).click();
      const aliceApp = adminPage.getByRole('listitem').filter({ hasText: `(${alice.username})` });
      await expect(aliceApp).toContainText('127.0.0.1');
      await expect(aliceApp.getByRole('button', { name: 'Disconnect' })).toBeVisible();
      await expect(
        adminPage.getByRole('checkbox', { name: 'Allow managing library files (delete, move, re-tag)' }),
      ).toHaveCount(0);
    } finally {
      await adminContext.close();
    }

    // One blocked, one allowed: a partial answer whose token covers the allowed song only.
    const preview = await a.call('preview_delete_tracks', { song_ids: [holyId, bleakId], reason: 'cleanup' });
    expect(preview.isError).toBe(true);
    const p = preview.doc as Items & { confirmation_token: string; will_delete: string[]; next: NextCall[] };
    expect(p.error).toMatchObject({ code: 'partial', retry: 'per_item' });
    expect(p).toMatchObject({
      reason: 'cleanup',
      expires_at: expect.any(String),
      daily_limit: { limit: 100, used_last_24h: 0 },
    });
    const blocked = nth(p.results, 0);
    expect(blocked).toMatchObject({
      index: 0,
      ok: false,
      error: {
        code: 'delete_blocked_by_other_user',
        stage: 'delete',
        retry: 'not_retryable',
        message: 'Another listener rated, starred or kept this track in a playlist',
        facts: { song_id: holyId, verdict: 'blocked', song: { title: 'O Holy Night' } },
        // Asking again cannot help: only the other listener can change it.
        next: [],
      },
    });
    // It names no one and no file.
    expect(JSON.stringify(blocked)).not.toContain(bob.username);
    for (const text of [blocked.error?.message, blocked.error?.cause?.raw ?? '']) expect(text).not.toContain(folder.path);
    const allowed = nth(p.results, 1);
    // Her own playlist is listed; it does not block.
    expect(allowed).toMatchObject({
      index: 1,
      ok: true,
      song_id: bleakId,
      verdict: 'allowed',
      song: { title: 'In the Bleak Midwinter' },
      playlists: [{ playlist_id: playlistId, name: 'Christmas traditional', smart: false }],
      playlists_omitted: 0,
    });
    expect(inFolder(allowed.file), String(allowed.file)).toBe(true);
    expect(p.will_delete).toEqual([bleakId]);
    expect(p.next).toEqual([
      {
        tool: 'delete_tracks',
        args: { confirmation_token: p.confirmation_token },
        requires_user_confirmation: true,
      },
    ]);

    // The user said yes: the preview's next, as given.
    const yes = nth(p.next, 0);
    const deleted = (await a.ok(yes.tool, yes.args)) as Items;
    expect(deleted.results).toEqual([
      expect.objectContaining({ index: 0, ok: true, song_id: bleakId, status: 'deleted', file: allowed.file }),
    ]);
    expect(listFilesRecursive(folder.hostPath).filter((f) => basename(f).includes('In the Bleak Midwinter'))).toEqual(
      [],
    );
    // delete_tracks queues the beets forget on the folder's import worker and does not wait for it.
    await expect
      .poll(() => {
        const titles = libraryItems(folder.name, mcpMusicDirForApp).map((i) => i.title);
        return [titles.includes('In the Bleak Midwinter'), titles.includes('O Holy Night')];
      })
      .toEqual([false, true]);

    // The same token again: used, and next points at the history, which names the app.
    const reused = await a.call(yes.tool, yes.args);
    expect(reused.isError).toBe(true);
    const used = reused.doc.error as ErrorObject;
    expect(used).toMatchObject({
      code: 'expired_handle',
      message: 'This confirmation token was already used',
      facts: { song_ids: [bleakId], reason: 'cleanup' },
      next: [{ tool: 'get_deletion_history', args: {} }],
    });
    const history = nth(used.next, 0);
    const rowOf = async (songId: string) =>
      ((await a.ok(history.tool, history.args)) as { deletions: Record<string, unknown>[] }).deletions.find(
        (d) => d.song_id === songId,
      );
    expect(await rowOf(bleakId)).toMatchObject({
      title: 'In the Bleak Midwinter',
      file: allowed.file,
      reason: 'cleanup',
      origin: 'mcp',
      app: '127.0.0.1',
    });

    // A replacement deletes only with a confirmed token. Hark was imported by beets into her
    // folder above, so beets sees the new copy as its duplicate.
    const replacing = (await a.ok('preview_delete_tracks', { song_ids: [harkId], reason: 'replaced' })) as Items & {
      confirmation_token: string;
    };
    expect(replacing).toMatchObject({ will_delete: [harkId], daily_limit: { limit: 100, used_last_24h: 1 } });
    const hark = nth(replacing.results, 0);
    expect(hark).toMatchObject({ song_id: harkId, verdict: 'allowed' });
    const harkFile = hark.file as string;
    expect(inFolder(harkFile), harkFile).toBe(true);
    const harkRelative = harkFile.slice(folder.path.length + 1);
    const harkOnHost = join(folder.hostPath, harkRelative);
    const oldInode = statSync(harkOnHost).ino;
    const ensured = (await a.ok('ensure_in_library', {
      items: [ref(F, 3)],
      folder_id: folderId,
      replace_existing: { confirmation_token: replacing.confirmation_token },
      request_key: randomUUID(),
    })) as Items & { batch_id: string };
    const replaceJob = nth(ensured.results, 0);
    expect(replaceJob.status).toBe('job_created');
    // One download of a seeded track and its import can outlast one 30 s wait; twenty-four bound
    // a slow CI runner at 12 minutes, under the test's 20.
    for (let round = 0; ; round++) {
      expect(round, 'get_batch rounds').toBeLessThan(24);
      const b = (await a.ok('get_batch', {
        batch_id: ensured.batch_id,
        wait_seconds: 30,
        wait_for: 'all_terminal',
      })) as Batch;
      if (b.all_terminal) break;
    }
    const job = nth(
      ((await a.ok('get_jobs', { job_ids: [replaceJob.job_id], detail: 'full' })) as { jobs: JobDoc[] }).jobs,
      0,
    );
    expect(job.state, JSON.stringify(job)).toBe('done');
    expect(nth(job.results as Record<string, unknown>[], 0)).toMatchObject({
      replaced: [{ files: [harkRelative], status: 'replaced' }],
      not_replaced: [],
    });
    // The new copy took the old one's place: one Hark file, not the one that was there.
    expect(listFilesRecursive(folder.hostPath).filter((f) => basename(f).includes('Hark the Herald Angels Sing'))).toEqual(
      [harkOnHost],
    );
    expect(statSync(harkOnHost).ino).not.toBe(oldInode);
    expect(libraryItems(folder.name, mcpMusicDirForApp).filter((i) => i.title === 'Hark the Herald Angels Sing')).toHaveLength(1);
    expect(await rowOf(harkId)).toMatchObject({
      title: 'Hark the Herald Angels Sing',
      reason: 'replaced',
      origin: 'mcp',
      app: '127.0.0.1',
    });

    // Another listener's static playlist keeps a track.
    await subsonicAs(bob, 'createPlaylist', { name: 'kept by bob', songId: goodKingId });
    const inPlaylist = await a.call('preview_delete_tracks', { song_ids: [goodKingId], reason: 'cleanup' });
    expect(inPlaylist.isError).toBe(true);
    const kept = inPlaylist.doc as Items & { confirmation_token: string | null; will_delete: string[] };
    // A preview of one blocked song is still partial, its one entry the blocked one.
    expect(kept.error).toMatchObject({ code: 'partial', retry: 'per_item' });
    expect(nth(kept.results, 0)).toMatchObject({ index: 0, ok: false });
    const keptError = nth(kept.results, 0).error;
    expect([keptError?.code, keptError?.message, kept.confirmation_token, kept.will_delete]).toEqual([
      'delete_blocked_by_other_user',
      'Another listener rated, starred or kept this track in a playlist',
      null,
      [],
    ]);

    // An album another user starred keeps each of its tracks.
    const coventry = (await subsonicAs(alice, 'getSong', { id: coventryId })) as { song: { albumId: string } };
    await subsonicAs(bob, 'star', { albumId: coventry.song.albumId });
    const starred = await a.call('preview_delete_tracks', { song_ids: [coventryId], reason: 'cleanup' });
    expect(starred.isError).toBe(true);
    const s = starred.doc as Items & { confirmation_token: string | null; will_delete: string[] };
    expect(s.error).toMatchObject({ code: 'partial', retry: 'per_item' });
    expect(nth(s.results, 0)).toMatchObject({ index: 0, ok: false });
    expect([nth(s.results, 0).error?.code, s.confirmation_token, s.will_delete]).toEqual([
      'delete_blocked_by_other_user',
      null,
      [],
    ]);
    expect(existsSync(join(folder.hostPath, F.artist, F.title, 'Coventry Carol.flac'))).toBe(true);
  });

  await test.step('disconnecting ends the connection', async () => {
    await pageA.goto('/');
    await openSettings(pageA);
    await pageA.getByRole('button', { name: 'Users' }).click();
    await pageA.getByRole('button', { name: 'Disconnect' }).click();
    await confirmIn(pageA, 'Disconnect 127.0.0.1? It will have to be connected again from the app.');
    // The SDK meets a 401 and cannot sign in without the page.
    await expect(a.call('get_status')).rejects.toThrow();
    const revoked = await rawPost(a.provider.accessToken(), { jsonrpc: '2.0', id: 3, method: 'tools/list' });
    expect(revoked.status).toBe(401);
    expect(revoked.headers.get('www-authenticate')).toContain(
      'error_description="The access token is unknown, expired or revoked"',
    );
  });
});
