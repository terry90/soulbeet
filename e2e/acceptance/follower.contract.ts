// The follower's rules: what an app does with a job that needs action, from
// the job's code, retry advice, facts and next alone. Real codes, Soulful's
// own messages and slskd's real raw texts.

import { expect, test } from '@playwright/test';
import {
  SAME_SOURCE_WAIT_S,
  followerView,
  importDecision,
  nextStep,
  type ErrorObject,
  type JobDoc,
  type NextCall,
} from '../helpers/follower.js';

const retry = (mode: string, facts: Record<string, unknown> = {}): NextCall => ({
  tool: 'retry_job',
  args: { job_id: 'j1', mode },
  ...facts,
});
const dismiss: NextCall = { tool: 'dismiss_job', args: { job_id: 'j1' } };

function job(state: string, failure: Partial<ErrorObject> | null, next: NextCall[]): JobDoc {
  return {
    job_id: 'j1',
    batch_id: 'b1',
    state,
    updated_at: '2026-12-01T10:00:00Z',
    wanted: { artist: 'Fenwick Carol Singers', title: 'Hark the Herald Angels Sing' },
    failure: failure && {
      code: 'transfer_rejected_not_shared',
      stage: 'transfer',
      message: 'Transfer rejected by peer',
      cause: null,
      retry: 'other_source',
      retry_after_seconds: null,
      user_action: null,
      facts: {},
      next,
      ...failure,
    },
    next,
  };
}

test('the follower never sees a message or an upstream text', () => {
  const view = followerView(
    job('failed', { cause: { service: 'slskd', status: null, raw: 'Transfer rejected: File not shared.' } }, [dismiss]),
  );
  expect(view.failure).not.toHaveProperty('message');
  expect(view.failure).not.toHaveProperty('cause');
  expect(JSON.stringify(view)).not.toContain('Transfer rejected');
});

test('a refused or dropped transfer takes the next source', () => {
  const next = [retry('next_source', { untried_candidates: 1 }), retry('new_search'), dismiss];
  expect(nextStep(followerView(job('failed', { retry: 'other_source' }, next)))).toEqual({
    kind: 'call',
    tool: 'retry_job',
    args: { job_id: 'j1', mode: 'next_source' },
    afterSeconds: 0,
  });
});

test('a busy source is retried after the wait the failure gives', () => {
  const busy = (after: number | null) =>
    job(
      'failed',
      { code: 'transfer_rejected_peer_busy', retry: 'same_source_later', retry_after_seconds: after },
      [retry('same_source', { retry_after_seconds: after }), retry('new_search'), dismiss],
    );
  expect(nextStep(followerView(busy(30)))).toEqual({
    kind: 'call',
    tool: 'retry_job',
    args: { job_id: 'j1', mode: 'same_source' },
    afterSeconds: 30,
  });
  expect(nextStep(followerView(busy(null)))).toMatchObject({ kind: 'call', afterSeconds: SAME_SOURCE_WAIT_S });
});

test('results dropped only by the lossless filter are searched again without it', () => {
  const filtered = (notLossless: number) =>
    job(
      'failed',
      {
        code: 'search_all_filtered',
        stage: 'source_search',
        retry: 'other_query',
        facts: {
          query_text_sent: 'Fenwick Carol Singers Deck the Halls',
          dropped_by_filter: { format: 0, bitrate: 0, not_lossless: notLossless, queue_length: 0, excluded: 0 },
        },
      },
      [retry('new_search'), dismiss],
    );
  expect(nextStep(followerView(filtered(1)))).toEqual({
    kind: 'call',
    tool: 'retry_job',
    args: { job_id: 'j1', mode: 'new_search', lossless_only: false },
    afterSeconds: 0,
  });
  expect(nextStep(followerView(filtered(0)))).toMatchObject({ kind: 'stuck' });
});

test('an import beets could not match is imported as is only when it found no candidate', () => {
  const choices: NextCall = { tool: 'get_import_choices', args: { job_id: 'j1' } };
  const waiting = job('needs_decision', { code: 'import_no_match', stage: 'import', retry: 'needs_decision' }, [
    choices,
    { tool: 'resolve_import', args: { job_id: 'j1', choice: 'retry' } },
  ]);
  expect(nextStep(followerView(waiting))).toEqual({ kind: 'choose_import', choices });
  const allowed = ['retry', 'as_is', 'musicbrainz', 'discard'];
  const asIs = { tool: 'resolve_import', args: { job_id: 'j1', choice: 'as_is' } };
  expect(importDecision('j1', { candidates: [], allowed_choices: allowed })).toEqual(asIs);
  expect(
    importDecision('j1', {
      candidates: [{ data_source: 'MusicBrainz', id: 'd776ca5d-3ad7-4a56-8fa5-253c88fe7677', differs: [] }],
      allowed_choices: [...allowed, 'candidate'],
    }),
  ).toBeNull();
});

test('anything else is stuck', () => {
  for (const doc of [
    job('failed', { code: 'importer_broken', stage: 'import', retry: 'needs_user' }, [dismiss]),
    // No source left.
    job('failed', { code: 'transfer_rejected_not_shared', retry: 'other_source' }, [retry('new_search'), dismiss]),
    job('needs_source_choice', null, [{ tool: 'download_source', args: { job_id: 'j1', candidate_id: 's1:1' } }, dismiss]),
  ]) {
    expect(nextStep(followerView(doc)), JSON.stringify(doc)).toMatchObject({ kind: 'stuck' });
  }
});
