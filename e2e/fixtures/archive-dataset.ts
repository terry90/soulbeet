// The Internet Archive double's items: which releases of the shared discography archive.org
// holds, as whose uploads, in which format. Neither Soulseek peer shares the first and the
// last, so auto-download can only find them on the archive.

import {
  driftwoodCodes,
  pylonSong,
  wirelessOrchards,
  type AudioFormat,
  type FixtureRelease,
  type PeerShare,
} from './dataset.js';

export interface ArchiveFileFixture {
  /** The file's name in its item. */
  name: string;
  /** Recording this file is an encode of (audio and tags come from it). */
  recordingMbid: string;
  format: AudioFormat;
  /** For the generated encode; null for lossless. */
  bitRate: number | null;
  /** archive.org's format label. */
  label: string;
  title: string;
  track: number;
  lengthSeconds: number;
  /** The download answers 403, as for a file restricted after it was listed. */
  refused?: boolean;
}

export interface ArchiveItemFixture {
  identifier: string;
  title: string;
  creator: string;
  collections: string[];
  /** The MusicBrainz release the item names, as `urn:mb_release_id:`. */
  releaseMbid?: string;
  files: ArchiveFileFixture[];
}

function files(
  release: FixtureRelease,
  format: AudioFormat,
  bitRate: number | null,
  label: string,
  refused = false,
): ArchiveFileFixture[] {
  return release.tracks.map((track) => ({
    name: `${String(track.position).padStart(2, '0')} ${track.title}.${format}`,
    recordingMbid: track.mbid,
    format,
    bitRate,
    label,
    title: track.title,
    track: track.position,
    lengthSeconds: track.lengthMs / 1000,
    refused,
  }));
}

/** A FLAC upload that names its MusicBrainz release: auto-download picks it. */
export const wirelessOrchardsItem: ArchiveItemFixture = {
  identifier: 'orchid-telegraph-wireless-orchards-flac',
  title: 'Wireless Orchards',
  creator: wirelessOrchards.artist,
  collections: ['opensource_audio', 'fav-listener1'],
  releaseMbid: wirelessOrchards.releaseMbid,
  files: files(wirelessOrchards, 'flac', null, 'Flac'),
};

/**
 * A low bitrate WMA upload of an album a Soulseek peer has in WMA too: no source scores high
 * enough, so the user picks one.
 */
export const driftwoodCodesItem: ArchiveItemFixture = {
  identifier: 'static-harbor-driftwood-codes-wma',
  title: 'Driftwood Codes',
  creator: driftwoodCodes.artist,
  collections: ['opensource_audio'],
  files: files(driftwoodCodes, 'wma', 96, 'Windows Media'),
};

/** A single whose only file archive.org refuses once asked for. */
export const pylonSongItem: ArchiveItemFixture = {
  identifier: 'orchid-telegraph-pylon-song',
  title: 'Pylon Song',
  creator: pylonSong.artist,
  collections: ['opensource_audio'],
  files: files(pylonSong, 'flac', null, 'Flac', true),
};

export const archiveItems: ArchiveItemFixture[] = [
  wirelessOrchardsItem,
  driftwoodCodesItem,
  pylonSongItem,
];

/** The generated encodes the archive's files are, as the fixture generator takes them. */
export function archiveAudioFiles(): Pick<PeerShare, 'recordingMbid' | 'format' | 'bitRate'>[] {
  return archiveItems.flatMap((item) =>
    item.files.map(({ recordingMbid, format, bitRate }) => ({ recordingMbid, format, bitRate })),
  );
}
