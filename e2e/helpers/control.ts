// Client for the stubs' /_control APIs: scripts slskd peer behavior and
// outages and how MusicBrainz answers beets per spec, and resets shared stub
// state between destructive tests.

import type { PeerBehavior } from '../fixtures/dataset.js';
import type { MbMode } from '../stubs/src/mb-server.js';
import { mbControlUrl, slskdControlUrl } from './env.js';

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
