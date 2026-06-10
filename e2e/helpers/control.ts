// Client for the slskd stub's /_control API: scripts peer behavior and
// outages per spec, and resets shared stub state between destructive tests.

import type { PeerBehavior } from '../fixtures/dataset.js';
import { slskdControlUrl } from './env.js';

async function post(path: string, body: unknown): Promise<void> {
  const response = await fetch(`${slskdControlUrl}${path}`, {
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
}

export async function setPeerBehavior(username: string, behavior: PeerBehavior): Promise<void> {
  await post(`/_control/peers/${username}`, { behavior });
}

export async function setOutage(down: boolean): Promise<void> {
  await post('/_control/outage', { down });
}

export async function stubState(): Promise<Record<string, unknown>> {
  const response = await fetch(`${slskdControlUrl}/_control/state`);
  if (!response.ok) {
    throw new Error(`control state failed: ${response.status}`);
  }
  return (await response.json()) as Record<string, unknown>;
}
