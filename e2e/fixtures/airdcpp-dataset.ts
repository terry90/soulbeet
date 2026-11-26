// The AirDC++ double's peers: who shares which releases of the shared discography on its one
// Direct Connect hub, and how each behaves. Nicks, CIDs and the hub are invented. No Soulseek
// peer and no archive item has these releases, so auto-download can only find them here.

import {
  atticSignals,
  collectedDispatches,
  liveAtTheExchange,
  semaphoreGardens,
  type AudioFormat,
  type FixtureRelease,
  type PeerShare,
} from './dataset.js';

/** The web user Soulbeet signs in to the double as. */
export const airdcppUser = 'soulbeet';
export const airdcppPassword = 'e2e-airdcpp-password';

export const dcHubUrl = 'nmdcs://hub-one.example:411';
export const dcHubName = 'Hub One';

/**
 * How a peer answers: `happy` sends each file once queued, `passive` takes no incoming
 * connection (as the double itself), `offline` has left the hub by the time a file is queued.
 */
export type DcBehavior = 'happy' | 'passive' | 'offline';

export interface DcFile {
  /** The ADC path the peer shares it under. */
  path: string;
  /** Recording this file is an encode of (audio and tags come from it). */
  recordingMbid: string;
  format: AudioFormat;
  /** For the generated encode; null for lossless. */
  bitRate: number | null;
  /** The transfer starts and never finishes. */
  stalls?: boolean;
}

export interface DcPeer {
  nick: string;
  /** 39 base32 characters, as AirDC++ writes a CID. */
  cid: string;
  freeSlots: number;
  totalSlots: number;
  behavior: DcBehavior;
  files: DcFile[];
}

function files(
  release: FixtureRelease,
  format: AudioFormat,
  bitRate: number | null,
  stalls: number[] = [],
): DcFile[] {
  return release.tracks.map((track) => ({
    path: `/Music/${release.artist}/${release.title}/${String(track.position).padStart(2, '0')} - ${track.title}.${format}`,
    recordingMbid: track.mbid,
    format,
    bitRate,
    stalls: stalls.includes(track.position),
  }));
}

/** The complete FLAC album auto-download picks and imports. */
export const lighthousePeer: DcPeer = {
  nick: 'lighthouse_k',
  cid: 'KQ7HXE3MJPLT2WCRVBNU5DGYF4OAZSI6Q3WEHMA',
  freeSlots: 4,
  totalSlots: 6,
  behavior: 'happy',
  files: files(semaphoreGardens, 'flac', null),
};

/** The only peer with Live at the Exchange: passive, so a passive double never offers it. */
export const harborPeer: DcPeer = {
  nick: 'harbor_ghost',
  cid: 'M2RTYUIOPLKJHGFDSAZXCVBNMQWERTY234567AB',
  freeSlots: 2,
  totalSlots: 2,
  behavior: 'passive',
  files: files(liveAtTheExchange, 'flac', null),
};

/** The best source of Collected Dispatches, gone from the hub when its file is queued. */
export const lampPeer: DcPeer = {
  nick: 'lamp_post',
  cid: 'ZXCVBNMASDFGHJKLQWERTYUIOP234567ZXCVBNM',
  freeSlots: 3,
  totalSlots: 3,
  behavior: 'offline',
  files: files(collectedDispatches, 'flac', null),
};

/** The next source of Collected Dispatches, which sends it. */
export const relayPeer: DcPeer = {
  nick: 'relay_tower',
  cid: 'QWERTYUIOPASDFGHJKLZXCVBNM234567QWERTYU',
  freeSlots: 1,
  totalSlots: 2,
  behavior: 'happy',
  files: files(collectedDispatches, 'mp3', 320),
};

/** Attic Signals, whose second track starts and never finishes. */
export const atticPeer: DcPeer = {
  nick: 'attic_radio',
  cid: 'ASDFGHJKLZXCVBNMQWERTYUIOP234567ASDFGHJ',
  freeSlots: 2,
  totalSlots: 4,
  behavior: 'happy',
  files: files(atticSignals, 'flac', null, [2]),
};

export const dcPeers: DcPeer[] = [lighthousePeer, harborPeer, lampPeer, relayPeer, atticPeer];

/** The generated encodes the peers' files are, as the fixture generator takes them. */
export function airdcppAudioFiles(): Pick<PeerShare, 'recordingMbid' | 'format' | 'bitRate'>[] {
  return dcPeers.flatMap((peer) =>
    peer.files.map(({ recordingMbid, format, bitRate }) => ({ recordingMbid, format, bitRate })),
  );
}
