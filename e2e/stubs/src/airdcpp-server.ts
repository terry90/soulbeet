// AirDC++ Web Client double: the Web API v1 subset the Direct Connect backend uses, behind Basic
// auth, answering as AirDC++ 2.14.0 did in the live session of 2026-11-26 (NMDC-shaped results,
// at most 5 per peer, folders with size 0, partial file lists, file bundles written as .dctmp
// and renamed), plus a /_control namespace the spec reads.

import { mkdirSync } from 'node:fs';
import type { Server } from 'node:http';
import { airdcppPassword, airdcppUser, dcHubUrl } from '../../fixtures/airdcpp-dataset.js';
import { AirdcppState } from './airdcpp-state.js';
import { createStubServer, sendEmpty, sendJson, sendText } from './router.js';

const PERMISSIONS = [
  'search',
  'download',
  'transfers',
  'queue_view',
  'queue_edit',
  'hubs_view',
  'settings_view',
  'share_view',
  'filelists_view',
  'filelists_edit',
  'filesystem_view',
  'filesystem_edit',
];

export function createAirdcppServer(state: AirdcppState): Server {
  const expected = `Basic ${Buffer.from(`${airdcppUser}:${airdcppPassword}`).toString('base64')}`;
  // What the double saves into, as it names it: the shared downloads folder.
  const root = `${state.downloadsDir.replace(/\/$/, '')}/`;
  const searches = new Map<number, { owner: string; results: Record<string, unknown>[] }>();
  let nextSearch = 1;
  return createStubServer({
    name: 'airdcpp',
    intercept: ({ req, res, url }) => {
      if (url.pathname.startsWith('/_control')) {
        return false;
      }
      if (req.headers.authorization !== expected) {
        sendText(res, 401, 'Invalid username or password');
        return true;
      }
      return false;
    },
    routes: [
      {
        method: 'GET',
        pattern: '/api/v1/sessions/self',
        handler: ({ res }) =>
          sendJson(res, 200, {
            id: 1,
            type: 'basic_auth',
            user: { id: airdcppUser, username: airdcppUser, permissions: PERMISSIONS },
          }),
      },
      {
        method: 'GET',
        pattern: '/api/v1/system/system_info',
        handler: ({ res }) =>
          sendJson(res, 200, {
            api_version: 1,
            api_feature_level: 10,
            client_version: 'AirDC++w 2.14.0 x86_64',
            language: 'en-US',
          }),
      },
      {
        method: 'POST',
        pattern: '/api/v1/settings/get',
        handler: ({ res }) =>
          sendJson(res, 200, {
            download_directory: root,
            language_file: '',
            connection_mode_v4: 2,
            use_partial_sharing: false,
            min_search_interval: 5,
          }),
      },
      {
        method: 'GET',
        pattern: '/api/v1/hubs',
        handler: ({ res }) =>
          sendJson(res, 200, [
            { id: 1, hub_url: dcHubUrl, connect_state: { id: 'connected', str: 'Connected' } },
          ]),
      },
      { method: 'GET', pattern: '/api/v1/share/stats', handler: ({ res }) => sendEmpty(res, 204) },
      {
        method: 'POST',
        pattern: '/api/v1/filesystem/directory',
        handler: ({ res, body }) => {
          const path = String((body as { path?: string })?.path ?? '');
          if (!path.startsWith(root)) {
            sendJson(res, 500, { message: 'Failed to create directory: No such file or directory' });
            return;
          }
          mkdirSync(path, { recursive: true });
          sendEmpty(res, 204);
        },
      },
      {
        method: 'POST',
        pattern: '/api/v1/search',
        handler: ({ res, body }) => {
          const id = nextSearch++;
          // As AirDC++ names the owner of a Basic auth session's instance.
          const suffix = (body as { owner_suffix?: string })?.owner_suffix;
          const owner = suffix ? `basic_auth:${suffix}` : 'basic_auth';
          searches.set(id, { owner, results: [] });
          sendJson(res, 200, { id, expires_in: 600000, owner });
        },
      },
      {
        method: 'GET',
        pattern: '/api/v1/search/:id',
        handler: ({ res, params }) => {
          const search = searches.get(Number(params.id));
          if (!search) {
            sendJson(res, 404, { message: `Entity ${params.id} was not found` });
            return;
          }
          sendJson(res, 200, { id: Number(params.id), expires_in: 600000, owner: search.owner });
        },
      },
      {
        method: 'POST',
        pattern: '/api/v1/search/:id/hub_search',
        handler: ({ res, params, body }) => {
          const pattern = String((body as { query?: { pattern?: string } })?.query?.pattern ?? '');
          if (!pattern) {
            sendJson(res, 422, { code: 'missing_field', field: 'query', message: 'Field missing' });
            return;
          }
          const search = searches.get(Number(params.id));
          if (!search) {
            sendJson(res, 404, { message: `Entity ${params.id} was not found` });
            return;
          }
          search.results = state.search(pattern);
          sendJson(res, 200, {
            queue_time: 0,
            queued_count: 1,
            search_id: String(state.searches),
            query: { pattern, file_type: 'any' },
          });
        },
      },
      {
        method: 'GET',
        pattern: '/api/v1/search/:id/results/:result/children',
        // Before the results route, which the same path count matches.
        handler: ({ res }) => sendJson(res, 200, []),
      },
      {
        method: 'GET',
        pattern: '/api/v1/search/:id/results/:start/:count',
        handler: ({ res, params }) => sendJson(res, 200, searches.get(Number(params.id))?.results ?? []),
      },
      {
        method: 'DELETE',
        pattern: '/api/v1/search/:id',
        handler: ({ res, params }) => {
          searches.delete(Number(params.id));
          sendEmpty(res, 204);
        },
      },
      {
        method: 'GET',
        pattern: '/api/v1/filelists/:cid',
        handler: ({ res, params }) => {
          const list = state.lists.get(params.cid!);
          const peer = state.peer(params.cid!);
          if (!list || !peer) {
            sendJson(res, 404, { message: `Entity ${params.cid} was not found` });
            return;
          }
          list.looks += 1;
          const loaded = list.looks > 1;
          sendJson(res, 200, {
            id: peer.cid,
            partial_list: true,
            state: loaded
              ? { id: 'loaded', str: 'Loaded' }
              : { id: 'download_pending', str: 'Download pending' },
            location: loaded ? { path: list.directory, name: list.directory } : null,
            user: AirdcppState.user(peer),
          });
        },
      },
      {
        method: 'POST',
        pattern: '/api/v1/filelists',
        handler: ({ res, body }) => {
          const asked = body as { user?: { cid?: string }; directory?: string };
          const peer = state.peer(asked?.user?.cid ?? '');
          if (!peer) {
            sendJson(res, 400, { message: 'User not found' });
            return;
          }
          if (state.lists.has(peer.cid)) {
            sendJson(res, 409, { message: 'Filelist from this user is open already' });
            return;
          }
          state.lists.set(peer.cid, { directory: asked.directory ?? '/', looks: 0 });
          sendJson(res, 200, {
            id: peer.cid,
            state: { id: 'download_pending', str: 'Download pending' },
            user: AirdcppState.user(peer),
          });
        },
      },
      {
        method: 'GET',
        pattern: '/api/v1/filelists/:cid/items/:start/:count',
        handler: ({ res, params }) => {
          const list = state.lists.get(params.cid!);
          const peer = state.peer(params.cid!);
          if (!list || !peer) {
            sendJson(res, 404, { message: `Entity ${params.cid} was not found` });
            return;
          }
          sendJson(res, 200, { items: state.listItems(peer, list.directory) });
        },
      },
      {
        method: 'DELETE',
        pattern: '/api/v1/filelists/:cid',
        handler: ({ res, params }) => {
          state.lists.delete(params.cid!);
          sendEmpty(res, 204);
        },
      },
      {
        method: 'POST',
        pattern: '/api/v1/queue/bundles/file',
        handler: ({ res, body }) => {
          const asked = body as {
            target_directory?: string;
            target_name?: string;
            tth?: string;
            user?: { cid?: string };
          };
          // AirDC++ writes a %% in the folder back as one %.
          const folder = (asked.target_directory ?? '').replace(/%%/g, '%');
          const queued = state.queue(asked.user?.cid ?? '', asked.tth ?? '', `${folder}${asked.target_name ?? ''}`);
          if ('refused' in queued) {
            sendJson(res, 400, { message: queued.refused });
            return;
          }
          sendJson(res, 200, { id: queued.id, merged: false });
        },
      },
      {
        method: 'GET',
        pattern: '/api/v1/queue/bundles/:start/:count',
        handler: ({ res, params }) => {
          const bundles = state.listedBundles();
          const start = Number(params.start);
          if (bundles.length > 0 && start >= bundles.length) {
            sendJson(res, 400, { message: 'Invalid range' });
            return;
          }
          sendJson(res, 200, bundles.slice(start, start + Number(params.count)));
        },
      },
      {
        method: 'GET',
        pattern: '/api/v1/queue/bundles/:id',
        handler: ({ res, params }) => {
          const bundle = state.listedBundles().find((b) => b.id === Number(params.id));
          if (!bundle) {
            sendJson(res, 404, { message: `Bundle ${params.id} was not found` });
            return;
          }
          sendJson(res, 200, bundle);
        },
      },
      {
        method: 'POST',
        pattern: '/api/v1/queue/check_path_queued',
        handler: ({ res, body }) => {
          const path = String((body as { path?: string })?.path ?? '');
          const bundle = state.listedBundles().find((b) => b.target === path);
          sendJson(res, 200, {
            bundle: bundle
              ? { id: bundle.id, completed: (bundle.status as { completed: boolean }).completed }
              : null,
          });
        },
      },
      {
        method: 'GET',
        pattern: '/api/v1/transfers',
        handler: ({ res }) => sendJson(res, 200, state.transfers()),
      },
      {
        method: 'POST',
        pattern: '/api/v1/queue/bundles/:id/remove',
        handler: ({ res, params, body }) => {
          if (state.remove(Number(params.id), body)) {
            sendEmpty(res, 204);
          } else {
            sendJson(res, 404, { message: `Bundle ${params.id} was not found` });
          }
        },
      },
      {
        method: 'POST',
        pattern: '/api/v1/queue/bundles/:id/search',
        handler: ({ res }) => sendJson(res, 200, { sent: 1 }),
      },
      {
        method: 'POST',
        pattern: '/api/v1/users/search_hinted_user',
        handler: ({ res, body }) => {
          const peer = state.peer((body as { user?: { cid?: string } })?.user?.cid ?? '');
          if (!peer || peer.behavior === 'offline') {
            sendJson(res, 400, { message: 'User not found' });
            return;
          }
          sendJson(res, 200, AirdcppState.user(peer));
        },
      },
      // ---- control (specs only) -------------------------------------------
      {
        method: 'GET',
        pattern: '/_control/state',
        handler: ({ res }) => sendJson(res, 200, state.snapshot()),
      },
      {
        method: 'POST',
        pattern: '/_control/reset',
        handler: ({ res }) => {
          state.reset();
          sendJson(res, 200, {});
        },
      },
    ],
  });
}
