// The YouTube Music double's catalogue: which releases of the shared discography YouTube Music
// holds, under which ids. No Soulseek peer shares them, so auto-download finds them only once it
// searches its lossy-only last resort. fixtures/generate.ts writes it beside the fake ytmusicapi
// and yt-dlp packages the app's helper runs on (fixtures/ytdlp-site-packages).

import { atticSignals, morseBloom, semaphoreGardens, type FixtureRelease } from './dataset.js';

export interface YtTrackFixture {
  /** 11 characters, as a YouTube video id. */
  videoId: string;
  title: string;
  trackNumber: number;
  duration_seconds: number;
}

export interface YtAlbumFixture {
  browseId: string;
  title: string;
  artist: string;
  year: string;
  tracks: YtTrackFixture[];
}

function album(release: FixtureRelease, browseId: string, idPrefix: string): YtAlbumFixture {
  return {
    browseId,
    title: release.title,
    artist: release.artist,
    year: release.date.slice(0, 4),
    tracks: release.tracks.map((track) => ({
      videoId: `${idPrefix}${String(track.position).padStart(11 - idPrefix.length, '0')}`,
      title: track.title,
      trackNumber: track.position,
      duration_seconds: Math.round(track.lengthMs / 1000),
    })),
  };
}

export const morseBloomAlbum = album(morseBloom, 'MPREb_e2eMorseBloom', 'e2eMorse');
export const semaphoreGardensAlbum = album(semaphoreGardens, 'MPREb_e2eSemaphore', 'e2eSemaph');

/** YouTube Music answers the search for this release with a rate limit. */
export const rateLimitedRelease = atticSignals;

/** The name a track of `album` lands under, which its download row shows. */
export function landedName(album: YtAlbumFixture, track: YtTrackFixture): string {
  return `${album.artist} - ${track.title} [${track.videoId}].opus`;
}

export function ytmusicCatalogue(): {
  albums: YtAlbumFixture[];
  rate_limited_queries: string[];
} {
  return {
    albums: [morseBloomAlbum, semaphoreGardensAlbum],
    rate_limited_queries: [`${rateLimitedRelease.artist} ${rateLimitedRelease.title}`],
  };
}
