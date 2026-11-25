// archive.org double: the advanced search, an item's file list and metadata, and downloads
// that redirect to a data node serving the generated audio with Range, If-Range and ETag, as
// archive.org's answers were read on 2026-11-25.

import { createHash } from 'node:crypto';
import { readFileSync, statSync } from 'node:fs';
import type { Server, ServerResponse } from 'node:http';
import { join } from 'node:path';
import {
  archiveItems,
  type ArchiveFileFixture,
  type ArchiveItemFixture,
} from '../../fixtures/archive-dataset.js';
import { audioFileName } from '../../fixtures/dataset.js';
import { createStubServer, sendJson, sendText } from './router.js';

interface Served {
  bytes: Buffer;
  md5: string;
  etag: string;
}

/** `q` with Lucene's backslash escapes read back. */
function unescape(text: string): string {
  return text.replace(/\\(.)/g, '$1');
}

/** What a search query asks for, as the backend writes its queries. */
interface Asked {
  release?: string;
  creator?: string;
  /** Words the title holds, all of them. */
  titleWords?: string[];
  /** A phrase the title holds. */
  titlePhrase?: string;
}

function parseQuery(q: string): Asked {
  const asked: Asked = {};
  const release = /external-identifier:"urn:mb_release_id:([0-9a-f-]+)"/.exec(q);
  if (release) asked.release = release[1];
  const creator = /creator:\("((?:[^"\\]|\\.)*)"\)/.exec(q);
  if (creator?.[1] !== undefined) asked.creator = unescape(creator[1]);
  const phrase = /(?:title:|AND )\("((?:[^"\\]|\\.)*)"\)/.exec(q);
  if (phrase?.[1] !== undefined) asked.titlePhrase = unescape(phrase[1]);
  const words = /title:\(((?:[^"()\\]|\\.)+)\)/.exec(q);
  if (words?.[1] !== undefined) {
    asked.titleWords = words[1].split(' AND ').map((w) => unescape(w.trim()));
  }
  return asked;
}

function matches(item: ArchiveItemFixture, asked: Asked): boolean {
  if (asked.release !== undefined) return item.releaseMbid === asked.release;
  const lower = (s: string) => s.toLowerCase();
  if (asked.creator !== undefined && !lower(item.creator).includes(lower(asked.creator))) {
    return false;
  }
  const titles = [item.title, ...item.files.map((f) => f.title)].map(lower);
  if (asked.titlePhrase !== undefined) {
    const phrase = lower(asked.titlePhrase);
    return titles.some((t) => t.includes(phrase));
  }
  if (asked.titleWords !== undefined) {
    const words = asked.titleWords.map(lower);
    return titles.some((t) => words.every((w) => t.includes(w)));
  }
  // A query that names no title, as the health check's, finds every item.
  return asked.creator === undefined;
}

function formatsOf(item: ArchiveItemFixture): string[] {
  const labels = ['Archive BitTorrent', 'Metadata', 'Item Tile', ...item.files.map((f) => f.label)];
  return [...new Set(labels)];
}

function doc(item: ArchiveItemFixture): Record<string, unknown> {
  return {
    identifier: item.identifier,
    title: item.title,
    creator: item.creator,
    collection: item.collections,
    format: formatsOf(item),
    downloads: 120,
    mediatype: 'audio',
    ...(item.releaseMbid
      ? { 'external-identifier': [`urn:mb_release_id:${item.releaseMbid}`] }
      : {}),
  };
}

export function createArchiveServer(audioDir: string): Server {
  const served = new Map<string, Served>();
  function bytesOf(file: ArchiveFileFixture): Served {
    const path = join(audioDir, audioFileName(file));
    const known = served.get(path);
    if (known) return known;
    const bytes = readFileSync(path);
    const mtime = Math.floor(statSync(path).mtimeMs / 1000);
    const read = {
      bytes,
      md5: createHash('md5').update(bytes).digest('hex'),
      // As nginx writes it: the modification time and the size, in hex.
      etag: `"${mtime.toString(16)}-${bytes.length.toString(16)}"`,
    };
    served.set(path, read);
    return read;
  }

  function findFile(identifier: string, name: string) {
    const item = archiveItems.find((i) => i.identifier === identifier);
    return item?.files.find((f) => f.name === name);
  }

  function serveFile(
    res: ServerResponse,
    range: string | undefined,
    ifRange: string | undefined,
    file: ArchiveFileFixture,
  ): void {
    if (file.refused) {
      sendText(res, 403, '<!DOCTYPE html><title>Item not available</title>', 'text/html; charset=utf-8');
      return;
    }
    const { bytes, etag } = bytesOf(file);
    const contentType = file.format === 'flac' ? 'audio/flac' : 'application/octet-stream';
    const asked = /^bytes=(\d+)-(\d*)$/.exec(range ?? '');
    const sameVersion = ifRange === undefined || ifRange === etag;
    if (asked?.[1] !== undefined && sameVersion) {
      const start = Number(asked[1]);
      const end = asked[2] ? Math.min(Number(asked[2]), bytes.length - 1) : bytes.length - 1;
      if (start > end) {
        res.writeHead(416, { 'content-range': `bytes */${bytes.length}` });
        res.end();
        return;
      }
      res.writeHead(206, {
        'content-type': contentType,
        'content-length': end - start + 1,
        'content-range': `bytes ${start}-${end}/${bytes.length}`,
        etag,
        'accept-ranges': 'bytes',
      });
      res.end(bytes.subarray(start, end + 1));
      return;
    }
    res.writeHead(200, {
      'content-type': contentType,
      'content-length': bytes.length,
      etag,
      'accept-ranges': 'bytes',
    });
    res.end(bytes);
  }

  return createStubServer({
    name: 'archive',
    // A file name keeps its slashes, so these paths take any number of segments.
    intercept: ({ req, res, url }) => {
      const parts = url.pathname.split('/').map(decodeURIComponent);
      if (req.method === 'GET' && parts[1] === 'download' && parts.length >= 4) {
        const [identifier, ...name] = parts.slice(2);
        // archive.org sends a download on to the data node that holds the item.
        const node = [identifier ?? '', ...name].map(encodeURIComponent).join('/');
        res.writeHead(302, { location: `/0/items/${node}` });
        res.end();
        return true;
      }
      if (req.method === 'GET' && parts[1] === '0' && parts[2] === 'items' && parts.length >= 5) {
        const [identifier, ...name] = parts.slice(3);
        const file = findFile(identifier ?? '', name.join('/'));
        if (!file) {
          sendText(res, 404, '<html><title>404 Not Found</title></html>', 'text/html; charset=utf-8');
          return true;
        }
        const header = (h: string) => {
          const v = req.headers[h];
          return Array.isArray(v) ? v[0] : v;
        };
        serveFile(res, header('range'), header('if-range'), file);
        return true;
      }
      return false;
    },
    routes: [
      {
        method: 'GET',
        pattern: '/advancedsearch.php',
        handler: ({ res, url }) => {
          const q = url.searchParams.get('q') ?? '';
          const rows = Number(url.searchParams.get('rows') ?? '50');
          const found = archiveItems.filter((item) => matches(item, parseQuery(q)));
          console.log(`[archive] search "${q}" -> ${found.length}`);
          sendJson(res, 200, {
            responseHeader: { status: 0, QTime: 1, params: { query: q, rows } },
            response: { numFound: found.length, start: 0, docs: found.slice(0, rows).map(doc) },
          });
        },
      },
      {
        method: 'GET',
        pattern: '/metadata/:id/files',
        handler: ({ res, params }) => {
          const item = archiveItems.find((i) => i.identifier === params.id);
          if (!item) {
            sendJson(res, 200, {});
            return;
          }
          const result: Record<string, string>[] = item.files.flatMap((file) => {
            const { bytes, md5 } = bytesOf(file);
            const original = {
              name: file.name,
              source: 'original',
              format: file.label,
              size: String(bytes.length),
              md5,
              mtime: '1764000000',
              length: file.lengthSeconds.toFixed(2),
              title: file.title,
              creator: item.creator,
              artist: item.creator,
              album: item.title,
              track: String(file.track),
            };
            // archive.org's own conversion of every audio original, never offered.
            const derivative = {
              name: file.name.replace(/\.[^.]+$/, '.mp3'),
              source: 'derivative',
              format: 'VBR MP3',
              original: file.name,
              size: '480000',
              md5: createHash('md5').update(file.name).digest('hex'),
              length: '00:30',
              title: file.title,
            };
            return [original, derivative];
          });
          result.push({
            name: '__ia_thumb.jpg',
            source: 'original',
            format: 'Item Tile',
            size: '2601',
            md5: '67c2063f1cca44dc20c1e3cecebb09d4',
            mtime: '1764000000',
          });
          sendJson(res, 200, { result });
        },
      },
      {
        method: 'GET',
        pattern: '/metadata/:id/metadata',
        handler: ({ res, params }) => {
          const item = archiveItems.find((i) => i.identifier === params.id);
          if (!item) {
            sendJson(res, 200, { error: `Couldn't locate item '${params.id}'` });
            return;
          }
          sendJson(res, 200, {
            result: {
              identifier: item.identifier,
              mediatype: 'audio',
              collection: item.collections,
              title: item.title,
              creator: item.creator,
            },
          });
        },
      },
    ],
  });
}
