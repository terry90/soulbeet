// Discovery dataset: what the ListenBrainz, Labs and MusicBrainz genre stubs
// know about the artists neighbouring Static Harbor (glassAtlas). All ids are
// fixed literals so runs are reproducible.

import { glassAtlas } from './dataset.js';

export interface StubArtist {
  mbid: string;
  name: string;
  /** Distinct listeners, as the popularity endpoint reports them */
  users: number;
  tags: { tag: string; count: number }[];
  recordings: { mbid: string; title: string; listens: number; firstReleaseDate: string }[];
}

export const lowtideChoir: StubArtist = {
  mbid: '7c1d2e3f-4a5b-4c6d-8e7f-9a0b1c2d3e4f',
  name: 'Lowtide Choir',
  users: 1200,
  tags: [{ tag: 'dream pop', count: 3 }],
  recordings: [
    { mbid: '8d2e3f4a-5b6c-4d7e-9f8a-0b1c2d3e4f5a', title: 'Salt Glass', listens: 900, firstReleaseDate: '2024-03-01' },
    { mbid: '9e3f4a5b-6c7d-4e8f-8a9b-1c2d3e4f5a6b', title: 'Low Bells', listens: 400, firstReleaseDate: '2023-06-10' },
  ],
};

export const cinderAndSalt: StubArtist = {
  mbid: '0f4a5b6c-7d8e-4f9a-9b0c-2d3e4f5a6b7c',
  name: 'Cinder and Salt',
  users: 800,
  tags: [{ tag: 'shoegaze', count: 2 }],
  recordings: [
    { mbid: '1a5b6c7d-8e9f-4a0b-8c1d-3e4f5a6b7c8d', title: 'Ember Tide', listens: 700, firstReleaseDate: '2025-09-12' },
  ],
};

/** Labs neighbours by seed artist MBID. */
export const similarArtists: Record<string, { artist: StubArtist; score: number }[]> = {
  [glassAtlas.artistMbid]: [
    { artist: lowtideChoir, score: 120 },
    { artist: cinderAndSalt, score: 80 },
  ],
};

export const artistTags: Record<string, { tag: string; count: number }[]> = {
  [glassAtlas.artistMbid]: [{ tag: 'dream pop', count: 5 }],
};

export const genres = ['dream pop', 'shoegaze', 'indie rock'];

export const tagSimilarity: Record<string, { similar_tag: string; count: number }[]> = {
  'dream pop': [
    { similar_tag: 'shoegaze', count: 40 },
    { similar_tag: '2010s', count: 9 },
  ],
};

export const sitewide: StubArtist[] = [lowtideChoir, cinderAndSalt];

/** Every artist the stubs can describe, by MBID. */
export const artistsByMbid: Map<string, StubArtist> = new Map(
  [lowtideChoir, cinderAndSalt].map((artist) => [artist.mbid, artist]),
);
