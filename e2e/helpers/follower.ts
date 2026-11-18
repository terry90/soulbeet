// A script standing in for an app that follows Soulful's advice: from a job
// that needs action it reads only the failure's code, retry advice, wait,
// facts and the job's next calls, never a message or an upstream text, and
// either names the next call or says it is stuck. Rules it does not have
// need judgement, which a script cannot stand in for.

export type RetryAdvice =
  | 'same_source_later'
  | 'other_source'
  | 'other_query'
  | 'needs_decision'
  | 'needs_user'
  | 'not_retryable'
  | 'per_item';

/** A call a document offers: the tool, its arguments and the deciding facts beside them. */
export interface NextCall {
  tool: string;
  args: Record<string, unknown>;
  [fact: string]: unknown;
}

export interface ErrorObject {
  code: string;
  stage: string;
  message: string;
  cause: { service: string; status: number | null; raw: string } | null;
  retry: RetryAdvice;
  retry_after_seconds: number | null;
  user_action: { code: string; what: string; where: string } | null;
  facts: Record<string, unknown>;
  tried?: Record<string, unknown>[];
  next: NextCall[];
}

/** A job as get_batch's needs_action and get_jobs' full detail show it. */
export interface JobDoc {
  job_id: string;
  batch_id: string;
  state: string;
  updated_at: string;
  wanted: { artist: string; title: string };
  failure: ErrorObject | null;
  next: NextCall[];
  [field: string]: unknown;
}

/** What the follower may read: no message, no cause text. */
export type Advice = Omit<ErrorObject, 'message' | 'cause'>;

export interface FollowerView {
  job_id: string;
  state: string;
  failure: Advice | null;
  next: NextCall[];
}

/** get_import_choices' answer, as far as the follower reads it. */
export interface ImportChoices {
  candidates: { data_source: string; id: string; differs: string[] }[];
  allowed_choices: string[];
}

export type Step =
  | { kind: 'call'; tool: string; args: Record<string, unknown>; afterSeconds: number }
  | { kind: 'choose_import'; choices: NextCall }
  | { kind: 'stuck'; why: string };

/** A new object from job_id, state, next and the failure without message and cause. */
export function followerView(job: JobDoc): FollowerView {
  const f = job.failure;
  const failure: Advice | null =
    f === null
      ? null
      : {
          code: f.code,
          stage: f.stage,
          retry: f.retry,
          retry_after_seconds: f.retry_after_seconds,
          user_action: f.user_action,
          facts: f.facts,
          ...(f.tried === undefined ? {} : { tried: f.tried }),
          next: f.next,
        };
  return { job_id: job.job_id, state: job.state, failure, next: job.next };
}

/** Waited before a same_source retry when the failure gives no retry_after_seconds. */
export const SAME_SOURCE_WAIT_S = 5;

export function nextStep(job: FollowerView): Step {
  const entry = (tool: string, mode?: string) =>
    job.next.find((n) => n.tool === tool && (mode === undefined || n.args.mode === mode));
  const stuck = (why: string): Step => ({ kind: 'stuck', why });
  const call = (n: NextCall | undefined, afterSeconds: number, extra: Record<string, unknown> = {}): Step =>
    n
      ? { kind: 'call', tool: n.tool, args: { ...n.args, ...extra }, afterSeconds }
      : stuck(`next has no entry for ${job.failure?.retry}`);
  if (job.state === 'needs_decision') {
    const choices = entry('get_import_choices');
    return choices ? { kind: 'choose_import', choices } : stuck('needs_decision without get_import_choices');
  }
  const f = job.failure;
  if (job.state !== 'failed' || f === null) return stuck(`no rule for state ${job.state}`);
  switch (f.retry) {
    case 'other_source':
      return call(entry('retry_job', 'next_source'), 0);
    case 'same_source_later':
      return call(entry('retry_job', 'same_source'), f.retry_after_seconds ?? SAME_SOURCE_WAIT_S);
    case 'other_query': {
      const dropped = (f.facts.dropped_by_filter as { not_lossless?: unknown } | null | undefined)?.not_lossless;
      return typeof dropped === 'number' && dropped > 0
        ? call(entry('retry_job', 'new_search'), 0, { lossless_only: false })
        : stuck('other words for the search need judgement');
    }
    default:
      return stuck(`retry advice ${f.retry}`);
  }
}

/** resolve_import as_is when beets found no candidate and as_is is allowed; null otherwise (choosing among candidates needs judgement). */
export function importDecision(jobId: string, choices: ImportChoices): NextCall | null {
  return choices.candidates.length === 0 && choices.allowed_choices.includes('as_is')
    ? { tool: 'resolve_import', args: { job_id: jobId, choice: 'as_is' } }
    : null;
}
