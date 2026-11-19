// Deezer double: no artist is known, so every search is empty and every
// radio is Deezer's HTTP 200 "no data" error (code 800).

import type { Server } from 'node:http';
import { createStubServer, sendJson } from './router.js';

export function createDeezerServer(): Server {
  return createStubServer({
    name: 'deezer',
    routes: [
      {
        method: 'GET',
        pattern: '/search/artist',
        handler: ({ res }) => {
          sendJson(res, 200, { data: [], total: 0 });
        },
      },
      {
        method: 'GET',
        pattern: '/artist/:id/radio',
        handler: ({ res }) => {
          sendJson(res, 200, { error: { type: 'DataException', message: 'no data', code: 800 } });
        },
      },
    ],
  });
}
