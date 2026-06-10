// MusicBrainz /ws/2 double. Both consumers speak the JSON web service:
// - soulbeet (musicbrainz_rs): recording / release-group search with the
//   entities inlined, plus release lookup for tracklists
// - beets 2.x (musicbrainz plugin): recording search for candidate ids,
//   then per-id recording lookups
// Everything is served from the shared fixture dataset.

import type { Server } from 'node:http';
import {
  findRecording,
  findRelease,
  releases,
  type FixtureRelease,
  type FixtureTrack,
} from '../../fixtures/dataset.js';
import { createStubServer, sendJson } from './router.js';

// ---- Lucene query handling -------------------------------------------------

/**
 * Parses the Lucene subset the two clients emit: `field:"phrase"` pairs
 * (musicbrainz_rs) and `field:(escaped terms)` groups (beets), joined with
 * AND or whitespace.
 */
function parseLucene(query: string): { fields: Map<string, string>; freeText: string[] } {
  const fields = new Map<string, string>();
  const fieldPattern = /([a-z]+):(?:"([^"]*)"|\(([^)]*)\)|(\S+))/gi;
  let match: RegExpExecArray | null;
  const consumed: string[] = [];
  while ((match = fieldPattern.exec(query)) !== null) {
    const key = (match[1] ?? '').toLowerCase();
    const value = match[2] ?? match[3] ?? match[4] ?? '';
    // beets escapes Lucene metacharacters with backslashes; undo that.
    fields.set(key, value.replace(/\\(.)/g, '$1').toLowerCase().trim());
    consumed.push(match[0] as string);
  }
  let rest = query;
  for (const piece of consumed) {
    rest = rest.replace(piece, ' ');
  }
  const freeText = rest
    .replace(/\bAND\b|\bOR\b|\bNOT\b/g, ' ')
    .toLowerCase()
    .split(/\s+/)
    .filter((t) => t.length > 0);
  return { fields, freeText };
}

function matchesText(haystack: string, needle: string | undefined): boolean {
  if (needle === undefined || needle.length === 0) {
    return true;
  }
  return haystack.toLowerCase().includes(needle);
}

interface RecordingHit {
  release: FixtureRelease;
  track: FixtureTrack;
}

function searchRecordings(query: string): RecordingHit[] {
  const { fields, freeText } = parseLucene(query);
  const title = fields.get('recording') ?? fields.get('recordingaccent');
  const artist = fields.get('artistname') ?? fields.get('artist');
  const free = freeText.join(' ');
  const hasCriteria = title !== undefined || artist !== undefined || free.length > 0;

  const hits: RecordingHit[] = [];
  for (const release of releases) {
    for (const track of release.tracks) {
      const titleOk = matchesText(track.title, title);
      const artistOk = matchesText(release.artist, artist);
      const freeOk =
        title === undefined && artist === undefined
          ? matchesText(`${release.artist} ${track.title}`, free)
          : true;
      if (hasCriteria && titleOk && artistOk && freeOk) {
        hits.push({ release, track });
      }
    }
  }
  return hits;
}

function searchReleaseGroups(query: string): FixtureRelease[] {
  const { fields, freeText } = parseLucene(query);
  const title =
    fields.get('releasegroup') ?? fields.get('releasegroupaccent') ?? fields.get('release');
  const artist = fields.get('artist') ?? fields.get('artistname');
  const free = freeText.join(' ');
  const hasCriteria = title !== undefined || artist !== undefined || free.length > 0;

  return releases.filter((release) => {
    const titleOk = matchesText(release.title, title);
    const artistOk = matchesText(release.artist, artist);
    const freeOk =
      title === undefined && artist === undefined
        ? matchesText(`${release.artist} ${release.title}`, free)
        : true;
    return hasCriteria && titleOk && artistOk && freeOk;
  });
}

// ---- JSON entity builders ----------------------------------------------------

function artistCreditJson(release: FixtureRelease): Array<Record<string, unknown>> {
  return [
    {
      name: release.artist,
      joinphrase: '',
      artist: {
        id: release.artistMbid,
        name: release.artist,
        'sort-name': release.artist,
        disambiguation: '',
        type: 'Group',
      },
    },
  ];
}

function releaseJson(release: FixtureRelease): Record<string, unknown> {
  return {
    id: release.releaseMbid,
    title: release.title,
    status: 'Official',
    date: release.date,
    country: 'XW',
    disambiguation: '',
    quality: 'normal',
    barcode: null,
    packaging: null,
    'status-id': '4e304316-386d-3409-af2e-78857eec5cfe',
    'release-events': [{ date: release.date, area: null }],
    'text-representation': { language: 'eng', script: 'Latn' },
    'artist-credit': artistCreditJson(release),
    'release-group': {
      id: release.releaseGroupMbid,
      title: release.title,
      'primary-type': release.primaryType,
      'first-release-date': release.date,
      'secondary-types': [],
      disambiguation: '',
    },
  };
}

function recordingJson(hit: RecordingHit, withReleases: boolean): Record<string, unknown> {
  return {
    id: hit.track.mbid,
    title: hit.track.title,
    length: hit.track.lengthMs,
    video: false,
    disambiguation: '',
    'first-release-date': hit.release.date,
    'artist-credit': artistCreditJson(hit.release),
    aliases: [],
    isrcs: [],
    relations: [],
    ...(withReleases ? { releases: [releaseJson(hit.release)] } : {}),
    rating: { 'votes-count': 12, value: hit.release.rating / 20 },
    score: 100,
  };
}

function releaseGroupJson(release: FixtureRelease): Record<string, unknown> {
  return {
    id: release.releaseGroupMbid,
    title: release.title,
    'primary-type': release.primaryType,
    'secondary-types': [],
    'first-release-date': release.date,
    disambiguation: '',
    'artist-credit': artistCreditJson(release),
    releases: [releaseJson(release)],
    rating: { 'votes-count': 12, value: release.rating / 20 },
    score: 100,
  };
}

function releaseWithRecordingsJson(release: FixtureRelease): Record<string, unknown> {
  return {
    ...releaseJson(release),
    'label-info': [],
    media: [
      {
        position: 1,
        format: 'Digital Media',
        title: '',
        'track-count': release.tracks.length,
        'track-offset': 0,
        tracks: release.tracks.map((track) => ({
          id: `00000000-0000-4000-8000-${track.mbid.slice(0, 12)}`,
          position: track.position,
          number: String(track.position),
          title: track.title,
          length: track.lengthMs,
          'artist-credit': artistCreditJson(release),
          recording: {
            id: track.mbid,
            title: track.title,
            length: track.lengthMs,
            video: false,
            disambiguation: '',
            'first-release-date': release.date,
            'artist-credit': artistCreditJson(release),
            aliases: [],
            isrcs: [],
            relations: [],
          },
        })),
      },
    ],
  };
}

// ---- server -----------------------------------------------------------------

export function createMusicBrainzServer(): Server {
  return createStubServer({
    name: 'musicbrainz',
    routes: [
      {
        method: 'GET',
        pattern: '/ws/2/recording',
        handler: ({ res, url }) => {
          const query = url.searchParams.get('query') ?? '';
          const limitParam = Number(url.searchParams.get('limit') ?? '25');
          const limit = Number.isFinite(limitParam) && limitParam > 0 ? limitParam : 25;
          const hits = searchRecordings(query)
            .sort((a, b) => b.release.rating - a.release.rating)
            .slice(0, limit);
          console.log(`[mb] recording search "${query}" -> ${hits.length}`);
          sendJson(res, 200, {
            created: new Date().toISOString(),
            count: hits.length,
            offset: 0,
            recordings: hits.map((hit) => recordingJson(hit, true)),
          });
        },
      },
      {
        method: 'GET',
        pattern: '/ws/2/recording/:id',
        handler: ({ res, params }) => {
          const hit = findRecording(params.id as string);
          console.log(`[mb] recording lookup ${params.id} -> ${hit ? 'hit' : 'miss'}`);
          if (!hit) {
            sendJson(res, 404, { error: 'Not Found' });
            return;
          }
          sendJson(res, 200, recordingJson(hit, true));
        },
      },
      {
        method: 'GET',
        pattern: '/ws/2/release-group',
        handler: ({ res, url }) => {
          const query = url.searchParams.get('query') ?? '';
          const found = searchReleaseGroups(query).sort((a, b) => b.rating - a.rating);
          console.log(`[mb] release-group search "${query}" -> ${found.length}`);
          sendJson(res, 200, {
            created: new Date().toISOString(),
            count: found.length,
            offset: 0,
            'release-groups': found.map((r) => releaseGroupJson(r)),
          });
        },
      },
      {
        method: 'GET',
        pattern: '/ws/2/release/:id',
        handler: ({ res, params }) => {
          const release = findRelease(params.id as string);
          console.log(`[mb] release lookup ${params.id} -> ${release ? 'hit' : 'miss'}`);
          if (!release) {
            sendJson(res, 404, { error: 'Not Found' });
            return;
          }
          sendJson(res, 200, releaseWithRecordingsJson(release));
        },
      },
    ],
  });
}
