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
  primaryType: 'Album' | 'Single' | 'EP';
  /** The release group's secondary types; none when left out */
  secondaryTypes?: ('Compilation' | 'Live' | 'Demo')[];
  /** Official when left out; null for a release no editor gave a status */
  status?: 'Official' | 'Bootleg' | null;
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
  | 'partialenqueue'
  | 'rejects'
  | 'busyonce'
  | 'slow';

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

export const cinderPsalm: FixtureRelease = {
  releaseMbid: '772d7563-eaf2-499f-9aa8-70e5b01762fa',
  releaseGroupMbid: 'c44092a6-38fd-4a42-857e-6e798723721d',
  title: 'Cinder Psalm',
  artist: 'Marrow Choir',
  artistMbid: marrowChoirMbid,
  date: '2024-02-09',
  primaryType: 'Single',
  rating: 74,
  tracks: [
    {
      mbid: 'e717a830-f577-4367-9ce4-5687211dbc04',
      title: 'Cinder Psalm',
      position: 1,
      lengthMs: 30000,
    },
  ],
};

export const ashLullaby: FixtureRelease = {
  releaseMbid: '809a4caf-4bb5-4e31-840a-f37409bf39cd',
  releaseGroupMbid: '4396bf03-3240-431d-8f21-d31167b509c1',
  title: 'Ash Lullaby',
  artist: 'Marrow Choir',
  artistMbid: marrowChoirMbid,
  date: '2024-06-21',
  primaryType: 'Single',
  rating: 73,
  tracks: [
    {
      mbid: '6481c6e1-8312-4c18-89fc-cc1aaedf6bfc',
      title: 'Ash Lullaby',
      position: 1,
      lengthMs: 30000,
    },
  ],
};

export const thimbleHymn: FixtureRelease = {
  releaseMbid: '0dee90ee-ad20-43fe-b361-2ca303b39e5a',
  releaseGroupMbid: '69172ebc-b4f4-4a94-a327-b91246017454',
  title: 'Thimble Hymn',
  artist: 'Marrow Choir',
  artistMbid: marrowChoirMbid,
  date: '2024-09-13',
  primaryType: 'Single',
  rating: 72,
  tracks: [
    {
      mbid: '0438c5fa-d748-4563-ae15-4827942f5535',
      title: 'Thimble Hymn',
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
  cinderPsalm,
  ashLullaby,
  thimbleHymn,
];

// Releases MusicBrainz knows but no peer shares: other matches for an
// import's files, rated below them so the app lists the shared one first.

/** A second match beets finds for the Cinder Psalm file. */
export const cinderPsalmChoir: FixtureRelease = {
  releaseMbid: 'b4004bc3-20da-4552-8484-c8488f131702',
  releaseGroupMbid: '036412ea-d017-44d4-b431-aaae78c6d456',
  title: 'Cinder Psalm (Choir Version)',
  artist: 'Marrow Choir',
  artistMbid: marrowChoirMbid,
  date: '2024-03-01',
  primaryType: 'Single',
  rating: 61,
  tracks: [
    {
      mbid: '52d1af7a-a75d-4713-8d17-ceffb879a18d',
      title: 'Cinder Psalm (Choir Version)',
      position: 1,
      lengthMs: 30000,
    },
  ],
};

/** A recording beets does not find for any file: only a typed ID reaches it. */
export const bellwether: FixtureRelease = {
  releaseMbid: 'f5770937-37b4-47cc-852a-cb91215fb5f3',
  releaseGroupMbid: 'a8577401-ae9e-407b-bc6c-c01771d5ad53',
  title: 'Bellwether',
  artist: 'Marrow Choir',
  artistMbid: marrowChoirMbid,
  date: '2024-11-08',
  primaryType: 'Single',
  rating: 60,
  tracks: [
    {
      mbid: '3eda6a69-a825-407a-8412-a44ea9fe8c63',
      title: 'Bellwether',
      position: 1,
      lengthMs: 30000,
    },
  ],
};

// The release groups of the artist search.spec.ts lists by name alone: two
// studio albums and an EP, a live album, a compilation and a demo whose
// release has no status, plus a single and a bootleg the listing leaves out.
// The stub answers them by rating, which orders them unlike the app's
// listing. No peer shares them.
const orchidTelegraphMbid = 'ca04fdcb-6061-4416-a79f-2cda532c6132';

export const semaphoreGardens: FixtureRelease = {
  releaseMbid: '8d879ec8-3c08-4459-9750-1da6245f45f4',
  releaseGroupMbid: '8e5809d7-eedb-444b-9213-64c5c57c7ca2',
  title: 'Semaphore Gardens',
  artist: 'Orchid Telegraph',
  artistMbid: orchidTelegraphMbid,
  date: '2017-03-10',
  primaryType: 'Album',
  rating: 56,
  tracks: [
    {
      mbid: '472b4b60-ed0a-4c26-9ee5-fa5aa6c738bc',
      title: 'Flag Hill',
      position: 1,
      lengthMs: 30000,
    },
    {
      mbid: 'a0f4f6dd-3d80-4cd3-9b87-e929034424e8',
      title: 'Morning Code',
      position: 2,
      lengthMs: 30000,
    },
  ],
};

export const morseBloom: FixtureRelease = {
  releaseMbid: '2ea22535-13c3-4e72-9b72-8794e9d1148f',
  releaseGroupMbid: '7814c4d5-fec1-47af-9699-622f06e12b9d',
  title: 'Morse Bloom',
  artist: 'Orchid Telegraph',
  artistMbid: orchidTelegraphMbid,
  date: '2015-06-12',
  primaryType: 'EP',
  rating: 55,
  tracks: [
    {
      mbid: '6c55ff50-028d-46ef-be16-8aa4ff059e00',
      title: 'Dot Dash',
      position: 1,
      lengthMs: 30000,
    },
    {
      mbid: '27765ce2-2280-4a11-a573-10d3a4266764',
      title: 'Bloom Key',
      position: 2,
      lengthMs: 30000,
    },
  ],
};

export const wirelessOrchards: FixtureRelease = {
  releaseMbid: 'c4bec95a-22b8-4003-8c66-51c92e87a25a',
  releaseGroupMbid: '62bb1537-aefe-4777-9867-e4cfd359ef3b',
  title: 'Wireless Orchards',
  artist: 'Orchid Telegraph',
  artistMbid: orchidTelegraphMbid,
  date: '2014-09-05',
  primaryType: 'Album',
  rating: 54,
  tracks: [
    {
      mbid: '2a3a93a8-3888-4ed1-a811-11744c77c471',
      title: 'Antenna Rows',
      position: 1,
      lengthMs: 30000,
    },
    {
      mbid: '548df86b-d89b-44a8-b50b-9beb391c2a24',
      title: 'Crystal Set',
      position: 2,
      lengthMs: 30000,
    },
    {
      mbid: '12d67ac9-6739-4df3-999e-03860d2e5367',
      title: 'Long Wave Lullaby',
      position: 3,
      lengthMs: 30000,
    },
  ],
};

export const liveAtTheExchange: FixtureRelease = {
  releaseMbid: 'b28f4715-555a-4c36-a986-3a51aad853ca',
  releaseGroupMbid: '1a42ad1f-85d5-40e9-8946-9a2634f82476',
  title: 'Live at the Exchange',
  artist: 'Orchid Telegraph',
  artistMbid: orchidTelegraphMbid,
  date: '2018-11-23',
  primaryType: 'Album',
  secondaryTypes: ['Live'],
  rating: 58,
  tracks: [
    {
      mbid: '36462e1e-3c9d-4def-80c1-be41038f7a63',
      title: 'Switchboard Overture',
      position: 1,
      lengthMs: 30000,
    },
  ],
};

export const collectedDispatches: FixtureRelease = {
  releaseMbid: 'f6fe03e0-2658-4f6b-8e60-dd2def177d50',
  releaseGroupMbid: '19546376-97e2-4cea-bb82-2abb32965c44',
  title: 'Collected Dispatches',
  artist: 'Orchid Telegraph',
  artistMbid: orchidTelegraphMbid,
  date: '2020-04-03',
  primaryType: 'Album',
  secondaryTypes: ['Compilation'],
  rating: 57,
  tracks: [
    {
      mbid: '635c037d-9411-4ade-97dd-08be9c8badb8',
      title: 'Telegram Waltz',
      position: 1,
      lengthMs: 30000,
    },
  ],
};

export const pylonSong: FixtureRelease = {
  releaseMbid: 'ebf65879-d94d-40cb-ac62-4b6628d26050',
  releaseGroupMbid: '06df7bf4-26fb-4a76-8a4e-7434b231c80b',
  title: 'Pylon Song',
  artist: 'Orchid Telegraph',
  artistMbid: orchidTelegraphMbid,
  date: '2019-02-01',
  primaryType: 'Single',
  rating: 53,
  tracks: [
    {
      mbid: '60aa7d54-f7ad-4307-aa92-f7b62ba4eeea',
      title: 'Pylon Song',
      position: 1,
      lengthMs: 30000,
    },
  ],
};

export const liveAtTheAnnex: FixtureRelease = {
  releaseMbid: 'f56f430c-b617-43b8-9599-64a78b0a53f0',
  releaseGroupMbid: '58020d53-7bd9-492e-8f61-1414320305c6',
  title: 'Live at the Annex',
  artist: 'Orchid Telegraph',
  artistMbid: orchidTelegraphMbid,
  date: '2016-08-19',
  primaryType: 'Album',
  secondaryTypes: ['Live'],
  status: 'Bootleg',
  rating: 52,
  tracks: [
    {
      mbid: '6890a298-c06f-4e02-87b3-a4fb3aeed0af',
      title: 'Annex Hum',
      position: 1,
      lengthMs: 30000,
    },
  ],
};

export const atticSignals: FixtureRelease = {
  releaseMbid: '2f5e348f-b2c0-428a-8f79-0f75135140ff',
  releaseGroupMbid: '4563dfb9-ee37-439e-948a-0c7173f3c8f2',
  title: 'Attic Signals',
  artist: 'Orchid Telegraph',
  artistMbid: orchidTelegraphMbid,
  date: '2012-05-04',
  primaryType: 'Album',
  secondaryTypes: ['Demo'],
  status: null,
  rating: 51,
  tracks: [
    {
      mbid: 'c1a4a951-6451-49ab-a25c-9123eb17ca61',
      title: 'Loft Static',
      position: 1,
      lengthMs: 30000,
    },
    {
      mbid: '7163fd88-4e32-42ff-b0d2-041de655c184',
      title: 'Rafter Tone',
      position: 2,
      lengthMs: 30000,
    },
  ],
};

const orchidTelegraphReleases: FixtureRelease[] = [
  semaphoreGardens,
  morseBloom,
  wirelessOrchards,
  liveAtTheExchange,
  collectedDispatches,
  pylonSong,
  liveAtTheAnnex,
  atticSignals,
];

// Releases the MCP acceptance suite downloads: no other spec touches them.

export const fenwickCarols: FixtureRelease = {
  releaseMbid: 'd776ca5d-3ad7-4a56-8fa5-253c88fe7677',
  releaseGroupMbid: '859137ac-42d1-4444-a0db-64cdc25ef999',
  title: 'Carols at Fenwick',
  artist: 'Fenwick Carol Singers',
  artistMbid: 'a77a15cb-f05f-41cc-9dcf-da78c518b3b4',
  date: '2025-12-05',
  primaryType: 'Album',
  rating: 70,
  tracks: [
    {
      mbid: '262c9272-daeb-4dbf-8a0a-780598777ca7',
      title: 'Silent Night',
      position: 1,
      lengthMs: 30000,
    },
    {
      mbid: 'c1e197e3-921d-49bf-83bf-3ebf2e49d7f7',
      title: 'O Holy Night',
      position: 2,
      lengthMs: 30000,
    },
    {
      mbid: 'af06e47b-77be-4109-a2f3-e34fa947d67f',
      title: 'Hark the Herald Angels Sing',
      position: 3,
      lengthMs: 30000,
    },
    {
      mbid: '066f44e6-56c2-4eb0-9c5e-f231e1df996a',
      title: 'Coventry Carol',
      position: 4,
      lengthMs: 30000,
    },
    {
      mbid: 'ba64f619-93e6-486f-bcc2-978b64ef5955',
      title: 'Good King Wenceslas',
      position: 5,
      lengthMs: 30000,
    },
    {
      mbid: 'ad423f2f-c53c-4b88-97ba-2595a344ff97',
      title: 'In the Bleak Midwinter',
      position: 6,
      lengthMs: 30000,
    },
    {
      mbid: '611961eb-db5c-42f9-a963-af5b4777b0d0',
      title: 'Deck the Halls',
      position: 7,
      lengthMs: 30000,
    },
  ],
};

export const wytheChapelCarols: FixtureRelease = {
  releaseMbid: '2722af51-0666-487e-951f-ad4bbc81d073',
  releaseGroupMbid: '86ff828f-7243-4340-911f-c6d18fdcd52c',
  title: 'Wythe Chapel Carols',
  artist: 'Wythe Chapel Choir',
  artistMbid: '16910708-a305-4cb6-883b-c05be367a483',
  date: '2024-12-13',
  primaryType: 'Single',
  rating: 60,
  tracks: [
    {
      mbid: 'b18dc001-1434-4e54-80f1-ee075ca66507',
      title: 'The Holly and the Ivy',
      position: 1,
      lengthMs: 30000,
    },
  ],
};

// What the README pictures search for (e2e/screenshots): an album search for
// "Night Garden" finds five albums and EPs of two artists. No spec searches
// for them and no peer shares them.
const hollowPinesMbid = 'f2fc621f-63a2-410b-9660-524eb4c49032';

/** A track of the pictures' releases, `length` as "m:ss". */
function shotTrack(mbid: string, position: number, title: string, length: string): FixtureTrack {
  const [minutes, seconds] = length.split(':').map(Number) as [number, number];
  return { mbid, title, position, lengthMs: (minutes * 60 + seconds) * 1000 };
}

export const nightGarden: FixtureRelease = {
  releaseMbid: '30710de2-0045-4112-8c9d-c098f1a8ab28',
  releaseGroupMbid: 'd63cac23-3538-42c1-8acc-a341ecbfb64d',
  title: 'Night Garden',
  artist: 'Hollow Pines',
  artistMbid: hollowPinesMbid,
  date: '2016-04-22',
  primaryType: 'Album',
  rating: 95,
  tracks: [
    shotTrack('b85b83d4-7ff0-47de-b1da-237dbd6f3678', 1, 'Moth Lantern', '4:12'),
    shotTrack('42503ab4-f4d9-4162-92a6-56b588d5f9ef', 2, 'Cold Frame', '3:47'),
    shotTrack('33fc0cae-1580-4b13-9fbf-db6ebe949148', 3, 'Wisteria', '5:03'),
    shotTrack('187d83bd-8727-4df5-a767-ff97d5009174', 4, 'Greenhouse Radio', '3:58'),
    shotTrack('724c8be3-e0dd-432d-b9a7-87ec87c9c1d6', 5, 'Under the Glass Roof', '4:36'),
    shotTrack('b6c69c28-d972-4acf-920b-f1100a256af0', 6, 'Pollen Count', '2:54'),
    shotTrack('9ddd1d38-66fb-48ca-83ff-da837ed6f27d', 7, 'Sleepwalkers', '4:21'),
    shotTrack('19d83ba1-c815-4599-89f9-480defd57257', 8, 'Lamplit', '3:33'),
    shotTrack('20c93314-086d-4aa2-8214-8be61527df9f', 9, 'Fernwood', '4:48'),
    shotTrack('eb0749c9-2c53-4eae-86a3-0d87ca22a9ae', 10, 'Dew Point', '6:15'),
  ],
};

const nightGardenSessions: FixtureRelease = {
  releaseMbid: 'a7191aea-39fa-44cb-b411-a49ee6bac5d7',
  releaseGroupMbid: '05da56d9-5f69-4a0a-9de6-e22565d7bf02',
  title: 'Night Garden Sessions',
  artist: 'Hollow Pines',
  artistMbid: hollowPinesMbid,
  date: '2017-02-10',
  primaryType: 'EP',
  rating: 90,
  tracks: [
    shotTrack('a4a1fddb-7fcc-4324-bef7-efafa8795d36', 1, 'Moth Lantern (Acoustic)', '4:05'),
    shotTrack('f5ab2363-1b3a-4452-b046-e6bd4e0d55c9', 2, 'Orchard Wall', '3:29'),
    shotTrack('2af052eb-2463-4358-88b2-b97e83626f07', 3, 'Ivy Hours', '5:12'),
  ],
};

const nightGardenLive: FixtureRelease = {
  releaseMbid: '0d1d0664-cdf1-4295-a13e-a117be866a6c',
  releaseGroupMbid: 'ec64c898-8376-4d72-b35f-16d4470fb99a',
  title: 'Night Garden: Live in Lisbon',
  artist: 'Hollow Pines',
  artistMbid: hollowPinesMbid,
  date: '2018-10-05',
  primaryType: 'Album',
  secondaryTypes: ['Live'],
  rating: 87,
  tracks: [
    shotTrack('3eaa1298-2889-473b-aef4-84cf33d8221c', 1, 'Moth Lantern (Live)', '4:44'),
    shotTrack('73a80e9c-d171-4bf6-818e-94c77d77cb2a', 2, 'Wisteria (Live)', '5:51'),
    shotTrack('ddffdaf6-c53a-4ec3-8c2d-a8c94aadc004', 3, 'Dew Point (Live)', '7:02'),
  ],
};

const theNightGarden: FixtureRelease = {
  releaseMbid: '96c8a36b-edae-456b-a0bc-3afd56a097f1',
  releaseGroupMbid: 'be0d137a-4a6b-407a-9597-8b305ae2a43d',
  title: 'The Night Garden',
  artist: 'Mira Kestrel',
  artistMbid: '8c508bea-6b6e-44ee-b7dc-067160108a05',
  date: '2009-06-15',
  primaryType: 'Album',
  rating: 84,
  tracks: [
    shotTrack('57a542f0-7677-4f04-8e06-f2ead956049b', 1, 'Blue Hour', '3:41'),
    shotTrack('5d56d739-bcb9-4e89-8529-1a9f758e4031', 2, 'Lanternfish', '4:27'),
    shotTrack('7ddab6b0-5c6f-426c-8c4e-01c6677c63ed', 3, 'Nocturne for Ferns', '6:08'),
  ],
};

const songsFromTheNightGarden: FixtureRelease = {
  releaseMbid: 'eddf9d73-536f-429e-82d1-00a14d304034',
  releaseGroupMbid: 'cfe59c8a-7c75-4064-876a-64c141f1d6b1',
  title: 'Songs from the Night Garden',
  artist: 'Hollow Pines',
  artistMbid: hollowPinesMbid,
  date: '2021-11-19',
  primaryType: 'Album',
  secondaryTypes: ['Compilation'],
  rating: 82,
  tracks: [
    shotTrack('9ffc870c-56e1-454d-8d1a-38182dc44ccd', 1, 'Wisteria (2021 Remaster)', '5:01'),
    shotTrack('1f6a3993-f20e-4b5f-bb77-0dfe31ed7e7d', 2, 'Greenhouse Radio (Single Edit)', '3:30'),
    shotTrack('025ebebb-0bd5-4d89-a116-49baac5fca93', 3, 'Moonflower', '4:16'),
  ],
};

const screenshotReleases: FixtureRelease[] = [
  nightGarden,
  nightGardenSessions,
  nightGardenLive,
  theNightGarden,
  songsFromTheNightGarden,
];

/** Releases peers share but the MusicBrainz stub does not know: beets finds no candidate for their files. */
export const releasesMusicBrainzLacks: FixtureRelease[] = [wytheChapelCarols];

export const releases: FixtureRelease[] = [
  glassAtlas,
  driftwoodCodes,
  senders,
  foglight,
  maritime,
  ...importReleases,
  cinderPsalmChoir,
  bellwether,
  ...orchidTelegraphReleases,
  fenwickCarols,
  ...screenshotReleases,
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

/** findRecording over `releases` and `releasesMusicBrainzLacks`: the tags a generated file carries. */
export function taggedRecording(
  recordingMbid: string,
): { release: FixtureRelease; track: FixtureTrack } | undefined {
  for (const release of [...releases, ...releasesMusicBrainzLacks]) {
    const track = release.tracks.find((t) => t.mbid === recordingMbid);
    if (track) {
      return { release, track };
    }
  }
  return undefined;
}

/** Filename of the generated audio fixture for a recording in a format, relative to the audio dir. */
export function audioFileName(share: Pick<PeerShare, 'recordingMbid' | 'format'>): string {
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

/** The shares of a release's tracks at the given positions. */
function carolShares(
  root: string,
  release: FixtureRelease,
  positions: number[],
  format: AudioFormat,
  bitRate: number | null,
  sampleRate: number | null,
  bitDepth: number | null,
): PeerShare[] {
  const wanted = new Set(
    release.tracks.filter((track) => positions.includes(track.position)).map((track) => track.mbid),
  );
  return albumShares(root, release, format, bitRate, sampleRate, bitDepth).filter((share) =>
    wanted.has(share.recordingMbid),
  );
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
  // The MCP acceptance suite's Christmas peers (see e2e/acceptance). The failing
  // ones score above carol_backup, so the automatic pick takes them first.
  {
    username: 'carol_happy',
    uploadSpeed: 8000,
    queueLength: 0,
    hasFreeUploadSlot: true,
    defaultBehavior: 'happy',
    shares: carolShares('Music\\FLAC', fenwickCarols, [1, 2, 4], 'flac', null, 44100, 16),
  },
  {
    username: 'carol_slow',
    uploadSpeed: 8000,
    queueLength: 0,
    hasFreeUploadSlot: true,
    defaultBehavior: 'slow',
    shares: albumShares('Music\\FLAC', wytheChapelCarols, 'flac', null, 44100, 16),
  },
  {
    username: 'carol_rejects',
    uploadSpeed: 8000,
    queueLength: 0,
    hasFreeUploadSlot: true,
    defaultBehavior: 'rejects',
    shares: carolShares('Music\\FLAC', fenwickCarols, [3], 'flac', null, 44100, 16),
  },
  {
    username: 'carol_drops',
    uploadSpeed: 8000,
    queueLength: 0,
    hasFreeUploadSlot: true,
    defaultBehavior: 'flaky',
    shares: carolShares('Music\\FLAC', fenwickCarols, [5], 'flac', null, 44100, 16),
  },
  {
    username: 'carol_busy',
    uploadSpeed: 8000,
    queueLength: 0,
    hasFreeUploadSlot: true,
    defaultBehavior: 'busyonce',
    shares: carolShares('Music\\FLAC', fenwickCarols, [6], 'flac', null, 44100, 16),
  },
  {
    username: 'carol_backup',
    uploadSpeed: 900,
    queueLength: 14,
    hasFreeUploadSlot: false,
    defaultBehavior: 'happy',
    shares: carolShares('Music\\FLAC', fenwickCarols, [3, 5], 'flac', null, 44100, 16),
  },
  {
    username: 'carol_mp3',
    uploadSpeed: 2500,
    queueLength: 1,
    hasFreeUploadSlot: true,
    defaultBehavior: 'happy',
    shares: carolShares('Music\\mp3', fenwickCarols, [7], 'mp3', 320, 44100, null),
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
