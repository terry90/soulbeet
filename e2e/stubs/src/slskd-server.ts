// HTTP surface of the slskd double: the API v0 subset soulbeet talks to,
// plus a /_control namespace the specs use to script peer behavior.

import type { Server } from 'node:http';
import type { PeerBehavior } from '../../fixtures/dataset.js';
import { createStubServer, sendEmpty, sendJson, sendText } from './router.js';
import type { SlskdState } from './slskd-state.js';

const VALID_BEHAVIORS: PeerBehavior[] = ['happy', 'ghost', 'flaky', 'stall', 'offline'];

export function createSlskdServer(state: SlskdState, apiKey: string): Server {
  return createStubServer({
    name: 'slskd',
    intercept: ({ req, res, url }) => {
      if (url.pathname.startsWith('/_control')) {
        return false;
      }
      if (state.outage) {
        sendText(res, 503, 'service unavailable');
        return true;
      }
      if (req.headers['x-api-key'] !== apiKey) {
        sendText(res, 401, 'unauthorized');
        return true;
      }
      return false;
    },
    routes: [
      // ---- session / application (health checks) -------------------------
      {
        method: 'GET',
        pattern: '/api/v0/session',
        handler: ({ res }) => sendJson(res, 200, {}),
      },
      {
        method: 'GET',
        pattern: '/api/v0/application',
        handler: ({ res }) =>
          sendJson(res, 200, {
            server: { address: 'vps.slsknet.org:2271', isConnected: true, isLoggedIn: true },
            version: { full: '0.99.0-e2e' },
          }),
      },

      // ---- searches -------------------------------------------------------
      {
        method: 'POST',
        pattern: '/api/v0/searches',
        handler: ({ res, body }) => {
          const request = body as { searchText?: string; id?: string };
          if (!request?.searchText) {
            sendText(res, 400, 'SearchText may not be null or empty');
            return;
          }
          const record = state.createSearch(request.searchText, request.id);
          sendJson(res, 200, state.searchJson(record));
        },
      },
      {
        method: 'GET',
        pattern: '/api/v0/searches/:id',
        handler: ({ res, params }) => {
          const record = state.getSearch(params.id as string);
          if (!record) {
            sendEmpty(res, 404);
            return;
          }
          sendJson(res, 200, state.searchJson(record));
        },
      },
      {
        method: 'GET',
        pattern: '/api/v0/searches/:id/responses',
        handler: ({ res, params }) => {
          const record = state.getSearch(params.id as string);
          if (!record) {
            sendEmpty(res, 404);
            return;
          }
          sendJson(res, 200, record.responses);
        },
      },
      {
        method: 'DELETE',
        pattern: '/api/v0/searches/:id',
        handler: ({ res, params }) => {
          sendEmpty(res, state.deleteSearch(params.id as string) ? 204 : 404);
        },
      },

      // ---- transfers ------------------------------------------------------
      {
        method: 'POST',
        pattern: '/api/v0/transfers/downloads/:username',
        handler: ({ res, params, body }) => {
          const files = body as Array<{ filename: string; size: number }>;
          if (!Array.isArray(files)) {
            sendText(res, 400, 'request body must be an array');
            return;
          }
          const result = state.enqueue(params.username as string, files);
          if (typeof result.body === 'string') {
            sendText(res, result.status, result.body);
          } else {
            sendJson(res, result.status, result.body);
          }
        },
      },
      {
        method: 'GET',
        pattern: '/api/v0/transfers/downloads',
        handler: ({ res, url }) => {
          const includeRemoved = url.searchParams.get('includeRemoved') === 'true';
          sendJson(res, 200, state.downloadsJson(includeRemoved));
        },
      },
      {
        method: 'DELETE',
        pattern: '/api/v0/transfers/downloads/:username/:id',
        handler: ({ res, params, url }) => {
          const remove = url.searchParams.get('remove') === 'true';
          const ok = state.cancel(params.username as string, params.id as string, remove);
          sendEmpty(res, ok ? 204 : 404);
        },
      },

      // ---- control API (specs only, not part of slskd) --------------------
      {
        method: 'POST',
        pattern: '/_control/reset',
        handler: ({ res }) => {
          state.reset();
          sendJson(res, 200, { ok: true });
        },
      },
      {
        method: 'POST',
        pattern: '/_control/peers/:username',
        handler: ({ res, params, body }) => {
          const behavior = (body as { behavior?: string })?.behavior as PeerBehavior | undefined;
          if (!behavior || !VALID_BEHAVIORS.includes(behavior)) {
            sendJson(res, 400, { error: `behavior must be one of ${VALID_BEHAVIORS.join(', ')}` });
            return;
          }
          if (!state.setBehavior(params.username as string, behavior)) {
            sendJson(res, 404, { error: `unknown peer ${params.username}` });
            return;
          }
          sendJson(res, 200, { ok: true });
        },
      },
      {
        method: 'POST',
        pattern: '/_control/outage',
        handler: ({ res, body }) => {
          state.outage = (body as { down?: boolean })?.down === true;
          sendJson(res, 200, { ok: true, outage: state.outage });
        },
      },
      {
        method: 'GET',
        pattern: '/_control/state',
        handler: ({ res }) => sendJson(res, 200, state.snapshot()),
      },
    ],
  });
}
