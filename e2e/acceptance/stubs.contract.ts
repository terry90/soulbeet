import { randomUUID } from 'node:crypto';
import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { setTimeout as sleep } from 'node:timers/promises';
import { expect, test } from '@playwright/test';
import { removeStubDownloads, resetStubs, setNavidromeDown, setPeerBehavior } from '../helpers/control.js';
import { downloadsDirOnHost, navidromeProxyUrl, slskdApiKey, slskdControlUrl } from '../helpers/env.js';

const DESTINATION = 'stub-contract';
const api = (path: string, init: RequestInit = {}) =>
  fetch(`${slskdControlUrl}/api/v0${path}`, { ...init, headers: { 'x-api-key': slskdApiKey, 'content-type': 'application/json' } });
const enqueue = (username: string, filename: string, id = randomUUID()) => api('/transfers/downloads/batches', {
  method: 'POST', body: JSON.stringify({ id, username, files: [{ filename, size: 1 }], options: { destination: DESTINATION } }) });
const status = async (username: string) => {
  const r = await api(`/users/${username}/status`);
  return { status: r.status, body: await r.json() };
};
interface Row { id: string; filename: string; state: string; exception?: string; attempts: number; removed: boolean; batchId: string }
/** Every row of the user's transfers for the file, removed ones too. */
async function rows(username: string, filename: string): Promise<Row[]> {
  const r = await api(`/transfers/downloads/${username}`);
  if (r.status !== 200) return [];
  const user = (await r.json()) as { directories: { files: Row[] }[] };
  return user.directories.flatMap((d) => d.files).filter((f) => f.filename === filename);
}
/** The live (not removed) row of the file once its state starts with "Completed"; polls every 200 ms, up to 15 s. */
async function finalState(username: string, filename: string): Promise<Row> {
  for (const end = Date.now() + 15_000; Date.now() < end; await sleep(200)) {
    const row = (await rows(username, filename)).find((f) => !f.removed);
    if (row?.state.startsWith('Completed')) return row;
  }
  throw new Error(`${filename} from ${username} did not complete`);
}
const fenwick = (n: string) => `Music\\FLAC\\Fenwick Carol Singers\\Carols at Fenwick\\${n}.flac`;

// A run cut off before afterAll left the stubs as it had them (carol_busy's one rejection used up).
test.beforeAll(async () => {
  await resetStubs();
});

test.afterAll(async () => {
  await resetStubs();
  await removeStubDownloads(DESTINATION);
});

test('a peer that refuses answers with slskd\'s rejection text', async () => {
  const file = fenwick('03 - Hark the Herald Angels Sing');
  const batchId = randomUUID();
  expect((await enqueue('carol_rejects', file, batchId)).status).toBe(201);
  expect(await finalState('carol_rejects', file)).toMatchObject(
    { state: 'Completed, Rejected', exception: 'Transfer rejected: File not shared.', attempts: 1 });
  for (const row of await rows('carol_rejects', file)) {
    expect(row.batchId).toBe(batchId);
  }
});

test('a dropped connection fails after slskd\'s three attempts', async () => {
  const file = fenwick('05 - Good King Wenceslas');
  expect((await enqueue('carol_drops', file)).status).toBe(201);
  expect(await finalState('carol_drops', file)).toMatchObject(
    { state: 'Completed, Errored', exception: 'Transfer failed: Read error: Remote connection closed', attempts: 3 });
});

test('a full queue refuses once, then the same file comes', async () => {
  const file = fenwick('06 - In the Bleak Midwinter');
  await enqueue('carol_busy', file);
  expect(await finalState('carol_busy', file)).toMatchObject({ state: 'Completed, Rejected', exception: 'Transfer rejected: Too many files' });
  expect((await enqueue('carol_busy', file)).status).toBe(201);   // the first one finished (SlskdState.isFinished)
  expect((await finalState('carol_busy', file)).state).toBe('Completed, Succeeded');
  expect(existsSync(join(downloadsDirOnHost, DESTINATION, '06 - In the Bleak Midwinter.flac'))).toBe(true);
});

test('a slow peer holds a transfer in progress', async () => {
  const file = 'Music\\FLAC\\Wythe Chapel Choir\\Wythe Chapel Carols\\01 - The Holly and the Ivy.flac';
  expect((await enqueue('carol_slow', file)).status).toBe(201);
  const live = async () => (await rows('carol_slow', file)).find((f) => !f.removed);
  await expect.poll(async () => (await live())?.state, { intervals: [200] }).toBe('InProgress');
  const row = await live();
  expect((await api(`/transfers/downloads/carol_slow/${row?.id}`, { method: 'DELETE' })).status).toBe(204);
  expect((await live())?.state).toBe('Completed, Cancelled');
});

test('user status reads presence as slskd names it, 200 for every user', async () => {
  expect(await status('carol_busy')).toEqual({ status: 200, body: { isPrivileged: false, presence: 'Online' } });
  await setPeerBehavior('carol_busy', 'offline');
  expect(await status('carol_busy')).toEqual({ status: 200, body: { isPrivileged: false, presence: 'Offline' } });
  expect(await status('nobody_shares_this')).toEqual({ status: 200, body: { isPrivileged: false, presence: 'Offline' } });
});

test('the Navidrome proxy refuses connections while down', async () => {
  await expect(fetch(navidromeProxyUrl)).resolves.toBeInstanceOf(Response);
  await setNavidromeDown(true);
  try {
    const refused = await fetch(navidromeProxyUrl).then(() => null, (e: Error) => e);
    // The proxy's listener is closed, so a container on the stack's network is
    // refused. This test dials the published port, where the container
    // runtime's forwarder may accept and then reset instead.
    expect(['ECONNREFUSED', 'ECONNRESET']).toContain((refused?.cause as NodeJS.ErrnoException | undefined)?.code);
  } finally {
    await setNavidromeDown(false);
  }
  await expect(fetch(navidromeProxyUrl)).resolves.toBeInstanceOf(Response);
});

test('the stub refuses to remove anything but one directory name', async () => {
  // "." and ".." (even as %2e) never reach the handler over HTTP: fetch resolves them before
  // sending, and the stub's router resolves them again when it parses the URL (router.ts). So
  // only names with a separator can be sent, and those are what is tested.
  for (const bad of ['a%2Fb', 'a%5Cb', '%2F']) {
    const r = await fetch(`${slskdControlUrl}/_control/downloads/${bad}`, { method: 'DELETE' });
    expect(r.status, bad).toBe(400);
  }
});
