// HTTP surface of the slskd double: the API v0 subset soulbeet talks to,
// plus a /_control namespace the specs use to script peer behavior.

import type { Server } from 'node:http';
import type { PeerBehavior } from '../../fixtures/dataset.js';
import { createStubServer, sendEmpty, sendJson, sendText } from './router.js';
import { parseGuid, type BatchEnqueueBody, type SlskdState } from './slskd-state.js';

const VALID_BEHAVIORS: PeerBehavior[] = [
  'happy',
  'ghost',
  'flaky',
  'retryflap',
  'stall',
  'offline',
  'enqueueoffline',
  'partialenqueue',
];

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
      {
        // The part of slskd's options soulbeet reads: the download retry
        // limit, slskd's default (Core/Options.cs:997).
        method: 'GET',
        pattern: '/api/v0/options',
        handler: ({ res }) =>
          sendJson(res, 200, {
            transfers: {
              download: {
                retry: { attempts: 3, delay: 5000, maxDelay: 60000, partial: 'resume' },
              },
            },
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
          sendJson(res, 200, state.searchResponses(record));
        },
      },
      {
        method: 'PUT',
        pattern: '/api/v0/searches/:id',
        handler: ({ res, params }) => {
          sendEmpty(res, state.cancelSearch(params.id as string) ? 200 : 404);
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
        pattern: '/api/v0/transfers/downloads/batches',
        handler: async ({ res, body }) => {
          const result = await state.enqueueBatch((body ?? {}) as BatchEnqueueBody);
          // slskd's [Produces("application/json")] serializes string bodies
          // (offline / validation messages) as JSON strings too.
          sendJson(res, result.status, result.body);
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
        // slskd 0.26 serves per-peer transfers here, and answers 404 when it
        // holds none for that peer. Soulbeet polls this rather than the whole
        // list, which grows with the instance's entire history (#78).
        method: 'GET',
        pattern: '/api/v0/transfers/downloads/:username',
        handler: ({ res, params }) => {
          const user = state.downloadsForUserJson(params.username as string);
          if (user === null) {
            sendEmpty(res, 404);
            return;
          }
          sendJson(res, 200, user);
        },
      },
      {
        // slskd looks the transfer up by id alone and answers it whatever
        // its removed flag (TransfersController.cs:549-573,
        // DownloadService.cs:688-704).
        method: 'GET',
        pattern: '/api/v0/transfers/downloads/:username/:id',
        handler: ({ res, params }) => {
          const id = parseGuid(params.id as string);
          if (id === null) {
            sendEmpty(res, 400);
            return;
          }
          const transfer = state.downloadJson(id);
          if (transfer === null) {
            sendEmpty(res, 404);
            return;
          }
          sendJson(res, 200, transfer);
        },
      },
      {
        method: 'DELETE',
        pattern: '/api/v0/transfers/downloads/:username/:id',
        handler: ({ res, params, url }) => {
          const remove = url.searchParams.get('remove') === 'true';
          const id = parseGuid(params.id as string);
          if (id === null) {
            sendEmpty(res, 400);
            return;
          }
          state.cancel(id, remove);
          sendEmpty(res, 204);
        },
      },
      {
        method: 'GET',
        pattern: '/api/v0/transfers/downloads/:username/:id/position',
        handler: ({ res, params }) => {
          const id = parseGuid(params.id as string);
          if (id === null) {
            sendEmpty(res, 400);
            return;
          }
          const result = state.placeInQueue(id);
          if (result.status === 404) {
            sendEmpty(res, 404);
            return;
          }
          // [Produces("application/json")] writes the 500's text as a JSON string.
          sendJson(res, result.status, result.body);
        },
      },

      // ---- users ----------------------------------------------------------
      {
        method: 'GET',
        pattern: '/api/v0/users/:username/status',
        handler: ({ res, params }) =>
          sendJson(res, 200, state.userStatusJson(params.username as string)),
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
