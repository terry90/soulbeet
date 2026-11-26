// GD Studio double: the API's `types=search` (an album's tracks, or tracks across albums) and
// `types=url` (a URL on this server named by the file's MD5, with the size in bytes and the real
// bitrate), and NetEase's file servers under /cdn with Range, as the real answers were read on
// 2026-11-25. /_control scripts an outage and a track whose URL names another MD5 than its
// file's, and reads the calls it answered.

import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import type { IncomingMessage, Server, ServerResponse } from 'node:http';
import { join } from 'node:path';
import { audioFileName } from '../../fixtures/dataset.js';
import { netEaseTracks, type NetEaseTrackFixture } from '../../fixtures/gdstudio-dataset.js';
import { createStubServer, sendEmpty, sendJson, sendText } from './router.js';
import type { ApiMode, GdStudioState, TrackBehavior } from './gdstudio-state.js';

interface Served {
  bytes: Buffer;
  md5: string;
}

/** NetEase's servers refuse a URL this long after it was named. */
const URL_LIFETIME_MS = 25 * 60 * 1000;

const MODES: ApiMode[] = ['up', 'outage'];
const BEHAVIORS: TrackBehavior[] = ['normal', 'wrongmd5'];

function row(track: NetEaseTrackFixture): Record<string, unknown> {
  return {
    id: track.id,
    name: track.name,
    artist: track.artist,
    album: track.album,
    pic_id: '109951170418971665',
    url_id: track.id,
    lyric_id: track.id,
    source: 'netease',
    from: 'music.gdstudio.xyz',
  };
}

/** Whether every word of `text` is in `haystack`, in any case. */
function holdsWords(haystack: string, text: string): boolean {
  const hay = haystack.toLowerCase();
  return text
    .toLowerCase()
    .split(/\s+/)
    .filter((w) => w.length > 0)
    .every((w) => hay.includes(w));
}

function header(req: IncomingMessage, name: string): string | undefined {
  const value = req.headers[name];
  return Array.isArray(value) ? value[0] : value;
}

export function createGdStudioServer(audioDir: string, state: GdStudioState): Server {
  const served = new Map<string, Served>();
  function bytesOf(track: NetEaseTrackFixture): Served {
    const known = served.get(track.id);
    if (known) return known;
    const bytes = readFileSync(join(audioDir, audioFileName(track)));
    const read = { bytes, md5: createHash('md5').update(bytes).digest('hex') };
    served.set(track.id, read);
    return read;
  }

  function search(res: ServerResponse, url: URL): void {
    const source = url.searchParams.get('source') ?? 'netease';
    const name = url.searchParams.get('name') ?? '';
    const count = Number(url.searchParams.get('count') ?? '20');
    let found: NetEaseTrackFixture[];
    if (source === 'netease_album') {
      // The tracks of the first album the text names, in album order.
      const album = netEaseTracks.find((t) => holdsWords(`${t.artist.join(' ')} ${t.album}`, name));
      found = album ? netEaseTracks.filter((t) => t.album === album.album) : [];
    } else {
      found = netEaseTracks.filter((t) =>
        holdsWords(`${t.artist.join(' ')} ${t.name} ${t.album}`, name),
      );
    }
    console.log(`[gdstudio] search ${source} "${name}" -> ${found.length}`);
    sendJson(res, 200, found.slice(0, count).map(row));
  }

  function locate(req: IncomingMessage, res: ServerResponse, url: URL): void {
    const track = netEaseTracks.find((t) => t.id === url.searchParams.get('id'));
    // GD Studio answers an empty URL for a track NetEase does not have.
    if (!track) {
      sendJson(res, 200, { url: '', br: 0, size: 0, from: 'music.gdstudio.xyz' });
      return;
    }
    const behavior = state.behaviorOf(track.id);
    const { bytes, md5 } = bytesOf(track);
    const named = behavior === 'wrongmd5' ? createHash('md5').update(md5).digest('hex') : md5;
    const kbps =
      track.bitRate ?? Math.round((bytes.length * 8) / track.lengthSeconds / 1000);
    const host = header(req, 'host') ?? '127.0.0.1:5063';
    const expires = Date.now() + URL_LIFETIME_MS;
    sendJson(res, 200, {
      url: `http://${host}/cdn/${expires}/${track.id}/${named}.${track.format}`,
      br: kbps,
      size: bytes.length,
      from: 'music.gdstudio.xyz',
    });
  }

  function api(req: IncomingMessage, res: ServerResponse, url: URL): void {
    state.called();
    switch (state.mode) {
      case 'outage':
        sendText(res, 503, '<html><title>503 Service Unavailable</title></html>', 'text/html');
        return;
      case 'up':
        break;
    }
    switch (url.searchParams.get('types')) {
      case 'search':
        search(res, url);
        return;
      case 'url':
        locate(req, res, url);
        return;
      default:
        sendJson(res, 400, { detail: 'Invalid request.' });
    }
  }

  function file(req: IncomingMessage, res: ServerResponse, parts: string[]): void {
    const [expires, trackId] = parts;
    const track = netEaseTracks.find((t) => t.id === trackId);
    if (!track) {
      sendText(res, 404, '<html><title>404 Not Found</title></html>', 'text/html');
      return;
    }
    if (Date.now() > Number(expires)) {
      sendText(res, 403, '<html><title>403 Forbidden</title></html>', 'text/html');
      return;
    }
    const { bytes, md5 } = bytesOf(track);
    const behavior = state.behaviorOf(track.id);
    const asked = /^bytes=(\d+)-(\d*)$/.exec(header(req, 'range') ?? '');
    const start = asked?.[1] !== undefined ? Number(asked[1]) : 0;
    const end = asked?.[2] ? Math.min(Number(asked[2]), bytes.length - 1) : bytes.length - 1;
    if (start > end) {
      res.writeHead(416, { 'content-range': `bytes */${bytes.length}` });
      res.end();
      return;
    }
    const heads: Record<string, string | number> = {
      // As NetEase's servers send it, FLAC too.
      'content-type': 'audio/mpeg; charset=UTF-8',
      'content-length': end - start + 1,
      'accept-ranges': 'bytes',
    };
    // Older objects come without it.
    if (behavior === 'normal') heads['x-nos-meta-origin-md5'] = md5;
    if (asked) heads['content-range'] = `bytes ${start}-${end}/${bytes.length}`;
    res.writeHead(asked ? 206 : 200, heads);
    res.end(bytes.subarray(start, end + 1));
  }

  return createStubServer({
    name: 'gdstudio',
    intercept: ({ req, res, url }) => {
      if (req.method === 'GET' && url.pathname === '/api.php') {
        api(req, res, url);
        return true;
      }
      const parts = url.pathname.split('/').slice(1);
      if (req.method === 'GET' && parts[0] === 'cdn' && parts.length === 4) {
        file(req, res, parts.slice(1));
        return true;
      }
      return false;
    },
    routes: [
      {
        method: 'POST',
        pattern: '/_control/mode',
        handler: ({ res, body }) => {
          const mode = (body as { mode?: ApiMode } | undefined)?.mode;
          if (!mode || !MODES.includes(mode)) {
            sendJson(res, 400, { error: `mode is one of ${MODES.join(', ')}` });
            return;
          }
          state.mode = mode;
          sendEmpty(res, 204);
        },
      },
      {
        method: 'POST',
        pattern: '/_control/tracks/:id',
        handler: ({ res, params, body }) => {
          const behavior = (body as { behavior?: TrackBehavior } | undefined)?.behavior;
          if (!behavior || !BEHAVIORS.includes(behavior)) {
            sendJson(res, 400, { error: `behavior is one of ${BEHAVIORS.join(', ')}` });
            return;
          }
          state.setBehavior(params.id ?? '', behavior);
          sendEmpty(res, 204);
        },
      },
      {
        method: 'GET',
        pattern: '/_control/calls',
        handler: ({ res }) => sendJson(res, 200, state.callStats()),
      },
      {
        method: 'POST',
        pattern: '/_control/reset',
        handler: ({ res }) => {
          state.reset();
          sendEmpty(res, 204);
        },
      },
    ],
  });
}
