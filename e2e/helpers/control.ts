// Client for the stubs' /_control APIs: scripts slskd peer behavior and
// outages, how MusicBrainz answers beets per spec, and how the GD Studio
// double's API and tracks answer; reads what beets asked MusicBrainz for and
// the calls GD Studio answered, and resets shared stub state between
// destructive tests.

import type { PeerBehavior } from '../fixtures/dataset.js';
import type { ApiMode, TrackBehavior } from '../stubs/src/gdstudio-state.js';
import type { MbMode } from '../stubs/src/mb-server.js';
import { gdstudioControlUrl, mbControlUrl, slskdControlUrl } from './env.js';

/** A transfer as the slskd stub's state snapshot reports it. */
export interface StubTransfer {
  id: string;
  username: string;
  filename: string;
  state: string;
  removed: boolean;
}

export interface StubState {
  outage: boolean;
  /** Enqueues answered with "Skipped: Already in progress" (per file) or 429. */
  rejections: { alreadyInProgress: number; throttled: number };
  transfers: StubTransfer[];
}

async function post(path: string, body: unknown, base = slskdControlUrl): Promise<void> {
  const response = await fetch(`${base}${path}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  if (!response.ok) {
    throw new Error(`control ${path} failed: ${response.status} ${await response.text()}`);
  }
}

export async function resetStubs(): Promise<void> {
  await post('/_control/reset', {});
  await post('/_control/reset', {}, mbControlUrl);
}

/** How the MusicBrainz stub answers beets from now on; soulbeet is unaffected. */
export async function setMbMode(mode: MbMode, latencyMs = 0): Promise<void> {
  await post('/_control/mode', { mode, latencyMs }, mbControlUrl);
}

/** The paths beets asked the MusicBrainz stub for since the last reset, oldest first. */
export async function mbRequests(): Promise<string[]> {
  const response = await fetch(`${mbControlUrl}/_control/requests`);
  if (!response.ok) {
    throw new Error(`control requests failed: ${response.status}`);
  }
  return (await response.json()) as string[];
}

export async function setPeerBehavior(username: string, behavior: PeerBehavior): Promise<void> {
  await post(`/_control/peers/${username}`, { behavior });
}

export async function setOutage(down: boolean): Promise<void> {
  await post('/_control/outage', { down });
}

export async function stubState(): Promise<StubState> {
  const response = await fetch(`${slskdControlUrl}/_control/state`);
  if (!response.ok) {
    throw new Error(`control state failed: ${response.status}`);
  }
  return (await response.json()) as StubState;
}

/** Takes the stubs' Navidrome proxy down (connections refused) or brings it back. */
export async function setNavidromeDown(down: boolean): Promise<void> {
  await post('/_control/navidrome', { down });
}

/** Has the slskd stub remove a delivered batch destination directory (it wrote it, as root). */
export async function removeStubDownloads(destination: string): Promise<void> {
  const response = await fetch(
    `${slskdControlUrl}/_control/downloads/${encodeURIComponent(destination)}`,
    { method: 'DELETE' },
  );
  if (response.status !== 200) {
    throw new Error(`control downloads failed: ${response.status} ${await response.text()}`);
  }
}

/** How the GD Studio double's API answers every call from now on. */
export async function setGdStudioMode(mode: ApiMode): Promise<void> {
  await post('/_control/mode', { mode }, gdstudioControlUrl);
}

/** How the GD Studio double answers for one NetEase track from now on. */
export async function setNetEaseTrack(trackId: string, behavior: TrackBehavior): Promise<void> {
  await post(`/_control/tracks/${trackId}`, { behavior }, gdstudioControlUrl);
}

/** The API calls the GD Studio double answered since its last reset, and the most in 5 minutes. */
export async function gdstudioCalls(): Promise<{ total: number; maxInWindow: number }> {
  const response = await fetch(`${gdstudioControlUrl}/_control/calls`);
  if (!response.ok) {
    throw new Error(`control calls failed: ${response.status}`);
  }
  return (await response.json()) as { total: number; maxInWindow: number };
}

export async function resetGdStudio(): Promise<void> {
  await post('/_control/reset', {}, gdstudioControlUrl);
}
