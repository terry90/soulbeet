// The GD Studio double's NetEase catalogue: which releases of the shared discography NetEase
// holds, under which track ids, in which format. No Soulseek peer shares them, so
// auto-download can only find them on NetEase.

import {
  morseBloom,
  semaphoreGardens,
  type AudioFormat,
  type FixtureRelease,
  type PeerShare,
} from './dataset.js';

export interface NetEaseTrackFixture {
  /** NetEase's track id. */
  id: string;
  name: string;
  artist: string[];
  album: string;
  /** Recording this file is an encode of (audio and tags come from it). */
  recordingMbid: string;
  format: AudioFormat;
  /** For the generated encode; null for lossless. */
  bitRate: number | null;
  lengthSeconds: number;
}

function tracks(
  release: FixtureRelease,
  firstId: number,
  format: AudioFormat,
  bitRate: number | null,
): NetEaseTrackFixture[] {
  return release.tracks.map((track, i) => ({
    id: String(firstId + i),
    name: track.title,
    artist: [release.artist],
    album: release.title,
    recordingMbid: track.mbid,
    format,
    bitRate,
    lengthSeconds: track.lengthMs / 1000,
  }));
}

/** A lossless album: auto-download picks it and imports it. */
export const semaphoreGardensTracks = tracks(semaphoreGardens, 1_903_017_310, 'flac', null);

/** An MP3 320 EP. */
export const morseBloomTracks = tracks(morseBloom, 1_903_015_120, 'mp3', 320);

export const netEaseTracks: NetEaseTrackFixture[] = [
  ...semaphoreGardensTracks,
  ...morseBloomTracks,
];

/** The name a NetEase file lands under in its batch folder, and the end of its row's item. */
export function landingName(track: NetEaseTrackFixture, position: number): string {
  return `${String(position).padStart(2, '0')} - ${track.name} [${track.id}].${track.format}`;
}

/** The generated encodes NetEase's files are, as the fixture generator takes them. */
export function gdstudioAudioFiles(): Pick<PeerShare, 'recordingMbid' | 'format' | 'bitRate'>[] {
  return netEaseTracks.map(({ recordingMbid, format, bitRate }) => ({
    recordingMbid,
    format,
    bitRate,
  }));
}
