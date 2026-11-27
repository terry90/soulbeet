// Deezer public API double.
//
// Serves a focused fixture catalog (Blue Öyster Cult + compilation duplicates
// and karaoke spam) for unified search, album detail, and artist catalog E2E
// tests, while returning empty results for all other queries (such as the
// MusicBrainz-only dataset fixtures) so automatic Deezer -> MusicBrainz
// fallback and discovery radio "no data" (code 800) behavior are both exercised.

import type { Server } from 'node:http';
import { createStubServer, sendJson } from './router.js';

interface StubArtist {
  id: number;
  name: string;
  nb_fan: number;
  nb_album: number;
  picture_medium: string | null;
  picture_big: string | null;
}

interface StubAlbumTrack {
  id: number;
  title: string;
  duration: number;
  artist: { id: number; name: string; picture_medium: string | null };
}

interface StubAlbum {
  id: number;
  title: string;
  release_date: string;
  record_type: 'album' | 'ep' | 'single' | 'compile';
  cover_medium: string | null;
  cover_big: string | null;
  artist: { id: number; name: string; picture_medium: string | null };
  tracks: StubAlbumTrack[];
}

interface StubSearchTrack {
  id: number;
  title: string;
  title_short: string;
  duration: number;
  rank: number;
  artist: { id: number; name: string; picture_medium: string | null };
  album: { id: number; title: string; cover_medium: string | null; cover_big: string | null };
}

const BLUE_OYSTER_CULT: StubArtist = {
  id: 501,
  name: 'Blue Öyster Cult',
  nb_fan: 842_000,
  nb_album: 2,
  picture_medium: null,
  picture_big: null,
};

const KARAOKE_HITS_BAND: StubArtist = {
  id: 901,
  name: 'Karaoke Hits Band',
  nb_fan: 120,
  nb_album: 1,
  picture_medium: null,
  picture_big: null,
};

const SING_KING_KARAOKE: StubArtist = {
  id: 902,
  name: 'Sing King Karaoke',
  nb_fan: 450,
  nb_album: 1,
  picture_medium: null,
  picture_big: null,
};

const TRIBUTE_FARM: StubArtist = {
  id: 903,
  name: 'Blue Oyster Cult Tribute Band',
  nb_fan: 85,
  nb_album: 1,
  picture_medium: null,
  picture_big: null,
};

const BOC_REF = {
  id: BLUE_OYSTER_CULT.id,
  name: BLUE_OYSTER_CULT.name,
  picture_medium: null,
};

const AGENTS_OF_FORTUNE: StubAlbum = {
  id: 1001,
  title: 'Agents of Fortune',
  release_date: '1976-05-21',
  record_type: 'album',
  cover_medium: null,
  cover_big: null,
  artist: BOC_REF,
  tracks: [
    { id: 2001, title: "This Ain't the Summer of Love", duration: 141, artist: BOC_REF },
    { id: 2002, title: 'True Confessions', duration: 177, artist: BOC_REF },
    { id: 2003, title: "(Don't Fear) The Reaper", duration: 308, artist: BOC_REF },
    { id: 2004, title: 'E.T.I. (Extra Terrestrial Intelligence)', duration: 223, artist: BOC_REF },
  ],
};

const FIRE_OF_UNKNOWN_ORIGIN: StubAlbum = {
  id: 1002,
  title: 'Fire of Unknown Origin',
  release_date: '1981-06-01',
  record_type: 'album',
  cover_medium: null,
  cover_big: null,
  artist: BOC_REF,
  tracks: [
    { id: 2011, title: 'Fire of Unknown Origin', duration: 249, artist: BOC_REF },
    { id: 2012, title: "Burnin' for You", duration: 270, artist: BOC_REF },
  ],
};

const GREATEST_HITS: StubAlbum = {
  id: 1003,
  title: 'Greatest Hits',
  release_date: '1990-01-01',
  record_type: 'compile',
  cover_medium: null,
  cover_big: null,
  artist: BOC_REF,
  tracks: [
    { id: 3001, title: "(Don't Fear) The Reaper", duration: 308, artist: BOC_REF },
    { id: 3003, title: "Burnin' for You", duration: 270, artist: BOC_REF },
  ],
};

const ESSENTIAL_BOC: StubAlbum = {
  id: 1004,
  title: 'The Essential Blue Öyster Cult',
  release_date: '2003-04-01',
  record_type: 'compile',
  cover_medium: null,
  cover_big: null,
  artist: BOC_REF,
  tracks: [
    { id: 3002, title: "(Don't Fear) The Reaper (Remastered)", duration: 308, artist: BOC_REF },
  ],
};

const ARTISTS: StubArtist[] = [
  BLUE_OYSTER_CULT,
  TRIBUTE_FARM,
  KARAOKE_HITS_BAND,
  SING_KING_KARAOKE,
];

const ALBUMS: StubAlbum[] = [
  AGENTS_OF_FORTUNE,
  FIRE_OF_UNKNOWN_ORIGIN,
  GREATEST_HITS,
  ESSENTIAL_BOC,
];

const SEARCH_TRACKS: StubSearchTrack[] = [
  // Canonical studio track
  {
    id: 2003,
    title: "(Don't Fear) The Reaper",
    title_short: "(Don't Fear) The Reaper",
    duration: 308,
    rank: 910_000,
    artist: BOC_REF,
    album: {
      id: AGENTS_OF_FORTUNE.id,
      title: AGENTS_OF_FORTUNE.title,
      cover_medium: null,
      cover_big: null,
    },
  },
  // Higher-ranked compilation duplicate: ranking must still pick the studio album
  {
    id: 3001,
    title: "(Don't Fear) The Reaper",
    title_short: "(Don't Fear) The Reaper",
    duration: 308,
    rank: 960_000,
    artist: BOC_REF,
    album: {
      id: GREATEST_HITS.id,
      title: GREATEST_HITS.title,
      cover_medium: null,
      cover_big: null,
    },
  },
  // Remastered compilation duplicate
  {
    id: 3002,
    title: "(Don't Fear) The Reaper (Remastered)",
    title_short: "(Don't Fear) The Reaper",
    duration: 308,
    rank: 890_000,
    artist: BOC_REF,
    album: {
      id: ESSENTIAL_BOC.id,
      title: ESSENTIAL_BOC.title,
      cover_medium: null,
      cover_big: null,
    },
  },
  // Karaoke / tribute spam that real Deezer returns for "Blue oyster the reaper"
  {
    id: 9001,
    title: "(Don't Fear) The Reaper (Karaoke Version)",
    title_short: "(Don't Fear) The Reaper",
    duration: 305,
    rank: 600_000,
    artist: { id: KARAOKE_HITS_BAND.id, name: KARAOKE_HITS_BAND.name, picture_medium: null },
    album: {
      id: 9101,
      title: 'Tribute to Blue Oyster Cult Karaoke',
      cover_medium: null,
      cover_big: null,
    },
  },
  {
    id: 9002,
    title: "Don't Fear The Reaper (In the Style of Blue Oyster Cult)",
    title_short: "Don't Fear The Reaper",
    duration: 302,
    rank: 550_000,
    artist: { id: SING_KING_KARAOKE.id, name: SING_KING_KARAOKE.name, picture_medium: null },
    album: {
      id: 9102,
      title: 'Classic Rock Karaoke',
      cover_medium: null,
      cover_big: null,
    },
  },
  // Additional Blue Öyster Cult studio tracks
  {
    id: 2012,
    title: "Burnin' for You",
    title_short: "Burnin' for You",
    duration: 270,
    rank: 820_000,
    artist: BOC_REF,
    album: {
      id: FIRE_OF_UNKNOWN_ORIGIN.id,
      title: FIRE_OF_UNKNOWN_ORIGIN.title,
      cover_medium: null,
      cover_big: null,
    },
  },
  {
    id: 2001,
    title: "This Ain't the Summer of Love",
    title_short: "This Ain't the Summer of Love",
    duration: 141,
    rank: 510_000,
    artist: BOC_REF,
    album: {
      id: AGENTS_OF_FORTUNE.id,
      title: AGENTS_OF_FORTUNE.title,
      cover_medium: null,
      cover_big: null,
    },
  },
];

const ARTIST_TOP_TRACKS = new Map<number, StubSearchTrack[]>([
  [
    BLUE_OYSTER_CULT.id,
    SEARCH_TRACKS.filter((t) => t.artist.id === BLUE_OYSTER_CULT.id && t.id < 3000),
  ],
]);

const ARTIST_ALBUMS = new Map<number, StubAlbum[]>([
  [BLUE_OYSTER_CULT.id, [AGENTS_OF_FORTUNE, FIRE_OF_UNKNOWN_ORIGIN]],
]);

function fold(text: string): string {
  return text
    .normalize('NFD')
    .replace(/[\u0300-\u036f]/g, '')
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, ' ')
    .trim();
}

function queryTokens(raw: string): string[] {
  const folded = fold(raw);
  return folded.length === 0 ? [] : folded.split(/\s+/);
}

function matchesAllTokens(haystack: string, tokens: string[]): boolean {
  if (tokens.length === 0) {
    return false;
  }
  const folded = fold(haystack);
  return tokens.every((token) => folded.includes(token));
}

function requestedLimit(url: URL, fallback = 25): number {
  const raw = Number(url.searchParams.get('limit') ?? String(fallback));
  return Number.isFinite(raw) && raw > 0 ? raw : fallback;
}

function noDataError() {
  return { error: { type: 'DataException', message: 'no data', code: 800 } };
}

export function createDeezerServer(): Server {
  return createStubServer({
    name: 'deezer',
    routes: [
      {
        method: 'GET',
        pattern: '/search/track',
        handler: ({ res, url }) => {
          const tokens = queryTokens(url.searchParams.get('q') ?? '');
          const limit = requestedLimit(url, 50);
          const hits = SEARCH_TRACKS.filter((track) =>
            matchesAllTokens(`${track.artist.name} ${track.title} ${track.album.title}`, tokens),
          ).slice(0, limit);
          sendJson(res, 200, { data: hits, total: hits.length });
        },
      },
      {
        method: 'GET',
        pattern: '/search/album',
        handler: ({ res, url }) => {
          const tokens = queryTokens(url.searchParams.get('q') ?? '');
          const limit = requestedLimit(url, 25);
          const hits = ALBUMS.filter((album) =>
            matchesAllTokens(`${album.artist.name} ${album.title}`, tokens),
          )
            .slice(0, limit)
            .map((album) => ({
              id: album.id,
              title: album.title,
              cover_medium: album.cover_medium,
              cover_big: album.cover_big,
              nb_tracks: album.tracks.length,
              record_type: album.record_type,
              artist: album.artist,
            }));
          sendJson(res, 200, { data: hits, total: hits.length });
        },
      },
      {
        method: 'GET',
        pattern: '/search/artist',
        handler: ({ res, url }) => {
          const tokens = queryTokens(url.searchParams.get('q') ?? '');
          const limit = requestedLimit(url, 15);
          const hits = ARTISTS.filter((artist) => matchesAllTokens(artist.name, tokens)).slice(
            0,
            limit,
          );
          sendJson(res, 200, { data: hits, total: hits.length });
        },
      },
      {
        method: 'GET',
        pattern: '/album/:id',
        handler: ({ res, params }) => {
          const id = Number(params.id);
          const album = ALBUMS.find((a) => a.id === id);
          if (!album) {
            sendJson(res, 200, noDataError());
            return;
          }
          sendJson(res, 200, {
            id: album.id,
            title: album.title,
            cover_medium: album.cover_medium,
            cover_big: album.cover_big,
            release_date: album.release_date,
            record_type: album.record_type,
            nb_tracks: album.tracks.length,
            artist: album.artist,
            tracks: { data: album.tracks },
          });
        },
      },
      {
        method: 'GET',
        pattern: '/artist/:id',
        handler: ({ res, params }) => {
          const id = Number(params.id);
          const artist = ARTISTS.find((a) => a.id === id);
          if (!artist) {
            sendJson(res, 200, noDataError());
            return;
          }
          sendJson(res, 200, artist);
        },
      },
      {
        method: 'GET',
        pattern: '/artist/:id/top',
        handler: ({ res, params, url }) => {
          const id = Number(params.id);
          const limit = requestedLimit(url, 25);
          const tracks = (ARTIST_TOP_TRACKS.get(id) ?? []).slice(0, limit);
          sendJson(res, 200, { data: tracks, total: tracks.length });
        },
      },
      {
        method: 'GET',
        pattern: '/artist/:id/albums',
        handler: ({ res, params, url }) => {
          const id = Number(params.id);
          const limit = requestedLimit(url, 100);
          const albums = (ARTIST_ALBUMS.get(id) ?? []).slice(0, limit).map((album) => ({
            id: album.id,
            title: album.title,
            cover_medium: album.cover_medium,
            cover_big: album.cover_big,
            release_date: album.release_date,
            record_type: album.record_type,
          }));
          sendJson(res, 200, { data: albums, total: albums.length });
        },
      },
      {
        method: 'GET',
        pattern: '/artist/:id/radio',
        handler: ({ res }) => {
          sendJson(res, 200, noDataError());
        },
      },
    ],
  });
}
