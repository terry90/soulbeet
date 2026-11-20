// ListenBrainz and ListenBrainz Labs doubles, shaped as the captured
// responses behind lib/soulbeet/src/listenbrainz/fixtures. Both serve the
// discovery dataset; unknown artists and tags answer empty, as the real
// services do.

import type { Server } from 'node:http';
import {
  artistTags,
  artistsByMbid,
  similarArtists,
  sitewide,
  tagSimilarity,
} from '../../fixtures/discovery.js';
import { glassAtlas } from '../../fixtures/dataset.js';
import { createStubServer, sendJson, sendText } from './router.js';

/** The only Labs algorithm the app asks for; Labs retires the others. */
export const LABS_ALGORITHM =
  'session_based_days_7500_session_300_contribution_5_threshold_10_limit_100_filter_True_skip_30';

const TOKEN_REQUIRED = {
  code: 401,
  error:
    'Due to bad actors and AI scrapers causing undue traffic on our sites, you need to provide an Auth token for this endpoint. Sorry for this mess.',
};

function listensOf(mbid: string): number {
  const artist = artistsByMbid.get(mbid);
  return artist ? artist.recordings.reduce((sum, r) => sum + r.listens, 0) : 0;
}

export function createListenBrainzServer(): Server {
  return createStubServer({
    name: 'listenbrainz',
    routes: [
      {
        method: 'GET',
        pattern: '/1/lb-radio/artist/:mbid',
        handler: ({ res, params }) => {
          const mbid = params.mbid as string;
          const artist = artistsByMbid.get(mbid);
          if (!artist) {
            sendJson(res, 200, {});
            return;
          }
          sendJson(res, 200, {
            [mbid]: artist.recordings.map((r) => ({
              recording_mbid: r.mbid,
              similar_artist_mbid: mbid,
              similar_artist_name: artist.name,
              total_listen_count: r.listens,
            })),
          });
        },
      },
      {
        method: 'POST',
        pattern: '/1/metadata/recording/',
        handler: ({ res, body }) => {
          const asked = ((body ?? {}) as { recording_mbids?: string[] }).recording_mbids ?? [];
          const known: Record<string, unknown> = {};
          for (const artist of artistsByMbid.values()) {
            for (const r of artist.recordings) {
              if (asked.includes(r.mbid)) {
                known[r.mbid] = {
                  artist: { name: artist.name, artists: [{ artist_mbid: artist.mbid, name: artist.name }] },
                  recording: { name: r.title, first_release_date: r.firstReleaseDate, length: 180000 },
                };
              }
            }
          }
          sendJson(res, 200, known);
        },
      },
      {
        method: 'GET',
        pattern: '/1/metadata/artist/',
        handler: ({ res, url }) => {
          const asked = (url.searchParams.get('artist_mbids') ?? '').split(',').filter(Boolean);
          sendJson(
            res,
            200,
            asked
              .filter((mbid) => artistsByMbid.has(mbid) || mbid in artistTags)
              .map((mbid) => {
                const tags = artistsByMbid.get(mbid)?.tags ?? artistTags[mbid] ?? [];
                return {
                  artist_mbid: mbid,
                  name: artistsByMbid.get(mbid)?.name ?? glassAtlas.artist,
                  tag: { artist: tags.map((t) => ({ ...t, artist_mbid: mbid })) },
                };
              }),
          );
        },
      },
      {
        method: 'POST',
        pattern: '/1/popularity/artist',
        handler: ({ res, body }) => {
          const asked = ((body ?? {}) as { artist_mbids?: string[] }).artist_mbids ?? [];
          sendJson(
            res,
            200,
            asked
              .map((mbid) => artistsByMbid.get(mbid))
              .filter((artist) => artist !== undefined)
              .map((artist) => ({
                artist_mbid: artist.mbid,
                total_listen_count: listensOf(artist.mbid),
                total_user_count: artist.users,
              })),
          );
        },
      },
      {
        method: 'GET',
        pattern: '/1/stats/sitewide/artists',
        handler: ({ res }) => {
          sendJson(res, 200, {
            payload: {
              artists: sitewide.map((a) => ({
                artist_mbid: a.mbid,
                artist_name: a.name,
                listen_count: listensOf(a.mbid),
              })),
              count: sitewide.length,
              range: 'all_time',
            },
          });
        },
      },
      {
        method: 'GET',
        pattern: '/1/lb-radio/tags',
        handler: ({ res }) => {
          sendJson(res, 200, []);
        },
      },
      {
        method: 'GET',
        pattern: '/1/popularity/top-recordings-for-artist/:mbid',
        handler: ({ res }) => {
          sendJson(res, 401, TOKEN_REQUIRED);
        },
      },
    ],
  });
}

export function createLabsServer(): Server {
  return createStubServer({
    name: 'labs',
    routes: [
      {
        method: 'GET',
        pattern: '/similar-artists/json',
        handler: ({ res, url }) => {
          if (url.searchParams.get('algorithm') !== LABS_ALGORITHM) {
            sendText(
              res,
              400,
              '<!doctype html>\n<html lang=en>\n<title>400 Bad Request</title>\n<h1>Bad Request</h1>\n' +
                `<p>algorithm: value is not a valid enumeration member; permitted: &#39;${LABS_ALGORITHM}&#39;</p>\n`,
              'text/html; charset=utf-8',
            );
            return;
          }
          const seed = url.searchParams.get('artist_mbids') ?? '';
          sendJson(
            res,
            200,
            (similarArtists[seed] ?? []).map(({ artist, score }) => ({
              artist_mbid: artist.mbid,
              name: artist.name,
              score,
              reference_mbid: seed,
            })),
          );
        },
      },
      {
        method: 'GET',
        pattern: '/tag-similarity/json',
        handler: ({ res, url }) => {
          sendJson(res, 200, tagSimilarity[url.searchParams.get('tag') ?? ''] ?? []);
        },
      },
    ],
  });
}
