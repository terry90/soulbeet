// Canonical test discography. Single source of truth for the MusicBrainz
// stub (metadata), the slskd stub (peer shares) and the specs (assertions).
// All MBIDs are fixed literals so runs are reproducible.

export interface FixtureTrack {
  /** MusicBrainz recording id */
  mbid: string;
  title: string;
  /** 1-based position on the release */
  position: number;
  lengthMs: number;
}

export interface FixtureRelease {
  releaseMbid: string;
  releaseGroupMbid: string;
  title: string;
  artist: string;
  artistMbid: string;
  /** YYYY-MM-DD */
  date: string;
  primaryType: 'Album' | 'Single';
  /** 0-100, drives result ordering in the app (sorted by rating) */
  rating: number;
  tracks: FixtureTrack[];
}

export type AudioFormat = 'flac' | 'mp3' | 'wma';

export interface PeerShare {
  /** Remote Soulseek path, backslash separated */
  remotePath: string;
  /** Recording this file is an encode of (audio + tags come from it) */
  recordingMbid: string;
  format: AudioFormat;
  /** Reported in search responses; null for lossless */
  bitRate: number | null;
  sampleRate: number | null;
  bitDepth: number | null;
}

export type PeerBehavior =
  | 'happy'
  | 'ghost'
  | 'flaky'
  | 'retryflap'
  | 'stall'
  | 'offline'
  | 'enqueueoffline'
  | 'partialenqueue';

export interface Peer {
  username: string;
  uploadSpeed: number;
  queueLength: number;
  hasFreeUploadSlot: boolean;
  defaultBehavior: PeerBehavior;
  shares: PeerShare[];
}

export const glassAtlas: FixtureRelease = {
  releaseMbid: '5f1b2c3d-9a4e-4b6f-8c1d-2e3f4a5b6c7d',
  releaseGroupMbid: '7a2b3c4d-5e6f-4a1b-9c8d-7e6f5a4b3c2d',
  title: 'Glass Atlas',
  artist: 'Static Harbor',
  artistMbid: '1c2d3e4f-5a6b-4c7d-8e9f-0a1b2c3d4e5f',
  date: '2019-05-17',
  primaryType: 'Album',
  rating: 92,
  tracks: [
    {
      mbid: 'a1b2c3d4-e5f6-4a7b-8c9d-0e1f2a3b4c5d',
      title: 'Northern Line',
      position: 1,
      lengthMs: 30000,
    },
    {
      mbid: 'b2c3d4e5-f6a7-4b8c-9d0e-1f2a3b4c5d6e',
      title: 'Paper Lanterns',
      position: 2,
      lengthMs: 30000,
    },
    {
      mbid: 'c3d4e5f6-a7b8-4c9d-0e1f-2a3b4c5d6e7f',
      title: 'Undertow',
      position: 3,
      lengthMs: 30000,
    },
  ],
};

export const driftwoodCodes: FixtureRelease = {
  releaseMbid: '6a2b3c4d-0b5f-4c7a-9d2e-3f4a5b6c7d8e',
  releaseGroupMbid: '8b3c4d5e-6f7a-4b2c-0d9e-8f7a6b5c4d3e',
  title: 'Driftwood Codes',
  artist: 'Static Harbor',
  artistMbid: glassAtlas.artistMbid,
  date: '2021-11-02',
  primaryType: 'Album',
  rating: 78,
  tracks: [
    {
      mbid: 'd4e5f6a7-b8c9-4d0e-1f2a-3b4c5d6e7f8a',
      title: 'Tidal Memory',
      position: 1,
      lengthMs: 30000,
    },
    {
      mbid: 'e5f6a7b8-c9d0-4e1f-2a3b-4c5d6e7f8a9b',
      title: 'Rust and Salt',
      position: 2,
      lengthMs: 30000,
    },
    {
      mbid: 'f6a7b8c9-d0e1-4f2a-3b4c-5d6e7f8a9b0c',
      title: 'Lighthouse Arithmetic',
      position: 3,
      lengthMs: 30000,
    },
  ],
};

export const senders: FixtureRelease = {
  releaseMbid: '3a4b5c6d-1c2d-4e3f-8a9b-4c5d6e7f8a9c',
  releaseGroupMbid: '4b5c6d7e-2d3e-4f4a-9b0c-5d6e7f8a9b0d',
  title: 'Senders',
  artist: 'Vela Norte',
  artistMbid: '2e3f4a5b-6c7d-4e8f-9a0b-1c2d3e4f5a6b',
  date: '2020-09-25',
  primaryType: 'Album',
  rating: 88,
  tracks: [
    {
      mbid: 'b8c9d0e1-f2a3-4b4c-5d6e-7f8a9b0c1d2e',
      title: 'Quiet Antenna',
      position: 1,
      lengthMs: 30000,
    },
    {
      mbid: 'c9d0e1f2-a3b4-4c5d-6e7f-8a9b0c1d2e3f',
      title: 'Sodium Light',
      position: 2,
      lengthMs: 30000,
    },
    {
      mbid: 'd0e1f2a3-b4c5-4d6e-7f8a-9b0c1d2e3f4a',
      title: 'Half Signal',
      position: 3,
      lengthMs: 30000,
    },
  ],
};

export const foglight: FixtureRelease = {
  releaseMbid: '5c6d7e8f-3e4f-4a5b-0c1d-6e7f8a9b0c1e',
  releaseGroupMbid: '6d7e8f9a-4f5a-4b6c-1d2e-7f8a9b0c1d2f',
  title: 'Foglight',
  artist: 'Vela Norte',
  artistMbid: '2e3f4a5b-6c7d-4e8f-9a0b-1c2d3e4f5a6b',
  date: '2023-01-13',
  primaryType: 'Single',
  rating: 81,
  tracks: [
    {
      mbid: 'e1f2a3b4-c5d6-4e7f-8a9b-0c1d2e3f4a5b',
      title: 'Foglight',
      position: 1,
      lengthMs: 30000,
    },
  ],
};

export const maritime: FixtureRelease = {
  releaseMbid: '9c4d5e6f-7a8b-4c3d-1e0f-9a8b7c6d5e4f',
  releaseGroupMbid: '0d5e6f7a-8b9c-4d4e-2f1a-0b9c8d7e6f5a',
  title: 'Maritime',
  artist: 'Vela Norte',
  artistMbid: '2e3f4a5b-6c7d-4e8f-9a0b-1c2d3e4f5a6b',
  date: '2022-03-09',
  primaryType: 'Single',
  rating: 85,
  tracks: [
    {
      mbid: 'a7b8c9d0-e1f2-4a3b-4c5d-6e7f8a9b0c1d',
      title: 'Maritime',
      position: 1,
      lengthMs: 30000,
    },
  ],
};

// Releases only imports.spec.ts downloads, one or two per test: the app's
// transfer registry is process-wide, so no other spec may touch their files.
const marrowChoirMbid = '3f4a5b6c-7d8e-4f9a-8b1c-2d3e4f5a6b7c';

export const copperSeason: FixtureRelease = {
  releaseMbid: '1a7b8c9d-2e3f-4a4b-9c5d-6e7f8a9b0c11',
  releaseGroupMbid: '2b8c9d0e-3f4a-4b5c-8d6e-7f8a9b0c1d22',
  title: 'Copper Season',
  artist: 'Marrow Choir',
  artistMbid: marrowChoirMbid,
  date: '2018-04-06',
  primaryType: 'Album',
  rating: 83,
  tracks: [
    {
      mbid: '3c9d0e1f-4a5b-4c6d-9e7f-8a9b0c1d2e33',
      title: 'Kiln Weather',
      position: 1,
      lengthMs: 30000,
    },
    {
      mbid: '4d0e1f2a-5b6c-4d7e-8f8a-9b0c1d2e3f44',
      title: 'Brass Orchard',
      position: 2,
      lengthMs: 30000,
    },
    {
      mbid: '5e1f2a3b-6c7d-4e8f-9a9b-0c1d2e3f4a55',
      title: 'Low Furnace',
      position: 3,
      lengthMs: 30000,
    },
  ],
};

export const ferryLights: FixtureRelease = {
  releaseMbid: '6f2a3b4c-7d8e-4f9a-8b0c-1d2e3f4a5b66',
  releaseGroupMbid: '7a3b4c5d-8e9f-4a0b-9c1d-2e3f4a5b6c77',
  title: 'Ferry Lights',
  artist: 'Marrow Choir',
  artistMbid: marrowChoirMbid,
  date: '2019-08-30',
  primaryType: 'Album',
  rating: 80,
  tracks: [
    {
      mbid: '8b4c5d6e-9f0a-4b1c-8d2e-3f4a5b6c7d88',
      title: 'Harbour Gate',
      position: 1,
      lengthMs: 30000,
    },
    {
      mbid: '9c5d6e7f-0a1b-4c2d-9e3f-4a5b6c7d8e99',
      title: 'Night Ferry',
      position: 2,
      lengthMs: 30000,
    },
  ],
};

export const saltRoads: FixtureRelease = {
  releaseMbid: '0d6e7f8a-1b2c-4d3e-8f4a-5b6c7d8e9f00',
  releaseGroupMbid: '1e7f8a9b-2c3d-4e4f-9a5b-6c7d8e9f0a01',
  title: 'Salt Roads',
  artist: 'Marrow Choir',
  artistMbid: marrowChoirMbid,
  date: '2020-02-14',
  primaryType: 'Album',
  rating: 79,
  tracks: [
    {
      mbid: '2f8a9b0c-3d4e-4f5a-8b6c-7d8e9f0a1b02',
      title: 'Dry Dock',
      position: 1,
      lengthMs: 30000,
    },
    {
      mbid: '3a9b0c1d-4e5f-4a6b-9c7d-8e9f0a1b2c03',
      title: 'Gull Static',
      position: 2,
      lengthMs: 30000,
    },
  ],
};

export const tinMoons: FixtureRelease = {
  releaseMbid: '4b0c1d2e-5f6a-4b7c-8d8e-9f0a1b2c3d04',
  releaseGroupMbid: '5c1d2e3f-6a7b-4c8d-9e9f-0a1b2c3d4e05',
  title: 'Tin Moons',
  artist: 'Marrow Choir',
  artistMbid: marrowChoirMbid,
  date: '2021-06-18',
  primaryType: 'Album',
  rating: 77,
  tracks: [
    {
      mbid: '6d2e3f4a-7b8c-4d9e-8f0a-1b2c3d4e5f06',
      title: 'Crescent Wire',
      position: 1,
      lengthMs: 30000,
    },
    {
      mbid: '7e3f4a5b-8c9d-4e0f-9a1b-2c3d4e5f6a07',
      title: 'Moon Ledger',
      position: 2,
      lengthMs: 30000,
    },
  ],
};

export const vesperBells: FixtureRelease = {
  releaseMbid: '8f4a5b6c-9d0e-4f1a-8b2c-3d4e5f6a7b08',
  releaseGroupMbid: '9a5b6c7d-0e1f-4a2b-9c3d-4e5f6a7b8c09',
  title: 'Vesper Bells',
  artist: 'Marrow Choir',
  artistMbid: marrowChoirMbid,
  date: '2022-10-07',
  primaryType: 'Single',
  rating: 76,
  tracks: [
    {
      mbid: '0b6c7d8e-1f2a-4b3c-8d4e-5f6a7b8c9d10',
      title: 'Vesper Bells',
      position: 1,
      lengthMs: 30000,
    },
  ],
};

export const quietRelay: FixtureRelease = {
  releaseMbid: '1c7d8e9f-2a3b-4c4d-9e5f-6a7b8c9d0e11',
  releaseGroupMbid: '2d8e9f0a-3b4c-4d5e-8f6a-7b8c9d0e1f12',
  title: 'Quiet Relay',
  artist: 'Marrow Choir',
  artistMbid: marrowChoirMbid,
  date: '2023-05-26',
  primaryType: 'Single',
  rating: 75,
  tracks: [
    {
      mbid: '3e9f0a1b-4c5d-4e6f-9a7b-8c9d0e1f2a13',
      title: 'Quiet Relay',
      position: 1,
      lengthMs: 30000,
    },
  ],
};

export const importReleases: FixtureRelease[] = [
  copperSeason,
  ferryLights,
  saltRoads,
  tinMoons,
  vesperBells,
  quietRelay,
];

export const releases: FixtureRelease[] = [
  glassAtlas,
  driftwoodCodes,
  senders,
  foglight,
  maritime,
  ...importReleases,
];

export function findRelease(releaseMbid: string): FixtureRelease | undefined {
  return releases.find((r) => r.releaseMbid === releaseMbid);
}

export function findRecording(
  recordingMbid: string,
): { release: FixtureRelease; track: FixtureTrack } | undefined {
  for (const release of releases) {
    const track = release.tracks.find((t) => t.mbid === recordingMbid);
    if (track) {
      return { release, track };
    }
  }
  return undefined;
}

/** Filename of the generated audio fixture for a share, relative to the audio dir. */
export function audioFileName(share: PeerShare): string {
  return `${share.recordingMbid}.${share.format}`;
}

function albumShares(
  root: string,
  release: FixtureRelease,
  format: AudioFormat,
  bitRate: number | null,
  sampleRate: number | null,
  bitDepth: number | null,
): PeerShare[] {
  return release.tracks.map((track) => ({
    remotePath: `${root}\\${release.artist}\\${release.title}\\${String(track.position).padStart(2, '0')} - ${track.title}.${format}`,
    recordingMbid: track.mbid,
    format,
    bitRate,
    sampleRate,
    bitDepth,
  }));
}

// collector_01 is the canonical best source: complete FLAC rips, fast, free
// slot. Tests flip its behavior through the control API when they need the
// best-ranked source to misbehave.
export const peers: Peer[] = [
  {
    username: 'collector_01',
    uploadSpeed: 8000,
    queueLength: 0,
    hasFreeUploadSlot: true,
    defaultBehavior: 'happy',
    shares: [
      ...albumShares('Music\\FLAC', glassAtlas, 'flac', null, 44100, 16),
      ...albumShares('Music\\FLAC', senders, 'flac', null, 44100, 16),
      ...albumShares('Music\\FLAC', foglight, 'flac', null, 44100, 16),
      ...albumShares('Music\\FLAC', maritime, 'flac', null, 44100, 16),
      ...importReleases.flatMap((release) =>
        albumShares('Music\\FLAC', release, 'flac', null, 44100, 16),
      ),
    ],
  },
  // Lower-quality alternative for the same album: ranks below collector_01.
  {
    username: 'mp3_hoarder',
    uploadSpeed: 2500,
    queueLength: 1,
    hasFreeUploadSlot: true,
    defaultBehavior: 'happy',
    shares: albumShares('Music\\mp3', glassAtlas, 'mp3', 320, 44100, null),
  },
  // Only source for Driftwood Codes: a low-bitrate WMA rip. The name match is
  // good (the search has to find it at all) but the format score drags the
  // group below the 0.7 auto-select threshold, forcing the manual-pick
  // fallback the specs assert on.
  {
    username: 'cryptic_archivist',
    uploadSpeed: 300,
    queueLength: 14,
    hasFreeUploadSlot: false,
    defaultBehavior: 'happy',
    shares: driftwoodCodes.tracks.map((track) => ({
      remotePath: `shares\\Static Harbor - Driftwood Codes (2021) [WMA]\\${String(track.position).padStart(2, '0')} ${track.title.toLowerCase()}.wma`,
      recordingMbid: track.mbid,
      format: 'wma' as const,
      bitRate: 96,
      sampleRate: 44100,
      bitDepth: null,
    })),
  },
];

export function findPeer(username: string): Peer | undefined {
  return peers.find((p) => p.username === username);
}

/** Every distinct (recording, format) pair that needs a generated audio file. */
export function allAudioFiles(): PeerShare[] {
  const seen = new Set<string>();
  const result: PeerShare[] = [];
  for (const peer of peers) {
    for (const share of peer.shares) {
      const key = audioFileName(share);
      if (!seen.has(key)) {
        seen.add(key);
        result.push(share);
      }
    }
  }
  return result;
}
