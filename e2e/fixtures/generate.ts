// Generates everything the stack needs that does not belong in git:
// tagged audio files for every peer share (ffmpeg sine tones), the beets
// config for the e2e stack (the shipped beets_config.yaml with MusicBrainz
// pointed at the stub), the broken config the user90 instance starts with
// and the runtime directories the services share. Run it before the stack
// starts, so the container runtime does not create those directories as root.

import { execFileSync } from 'node:child_process';
import { copyFileSync, mkdirSync, existsSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parse, stringify } from 'yaml';
import { allAudioFiles, audioFileName, findRecording } from './dataset.js';
import { user90BrokenConfig } from '../helpers/env.js';

const e2eRoot = dirname(dirname(fileURLToPath(import.meta.url)));
const audioDir = join(e2eRoot, '.fixtures', 'audio');
const runtimeDir = join(e2eRoot, '.runtime');

// Where beets reaches the MusicBrainz stub from the app's point of view:
// "stubs:5050" inside compose, "127.0.0.1:5050" for a native stack.
const mbHost = process.env.E2E_MB_HOST ?? '127.0.0.1:5050';

function generateAudio(): void {
  mkdirSync(audioDir, { recursive: true });

  for (const share of allAudioFiles()) {
    const target = join(audioDir, audioFileName(share));
    if (existsSync(target)) {
      continue;
    }

    const hit = findRecording(share.recordingMbid);
    if (!hit) {
      throw new Error(`share references unknown recording ${share.recordingMbid}`);
    }
    const { release, track } = hit;

    const frequency = 330 + track.position * 110;
    const seconds = track.lengthMs / 1000;

    const codecArgs =
      share.format === 'flac'
        ? ['-c:a', 'flac']
        : share.format === 'mp3'
          ? ['-c:a', 'libmp3lame', '-b:a', `${share.bitRate ?? 320}k`]
          : ['-c:a', 'wmav2', '-b:a', `${share.bitRate ?? 128}k`];

    execFileSync(
      'ffmpeg',
      [
        '-loglevel',
        'error',
        '-f',
        'lavfi',
        '-i',
        `sine=frequency=${frequency}:duration=${seconds}`,
        '-metadata',
        `title=${track.title}`,
        '-metadata',
        `artist=${release.artist}`,
        '-metadata',
        `album_artist=${release.artist}`,
        '-metadata',
        `album=${release.title}`,
        '-metadata',
        `track=${track.position}`,
        '-metadata',
        `date=${release.date.slice(0, 4)}`,
        ...codecArgs,
        '-y',
        target,
      ],
      { stdio: 'inherit' },
    );
    console.log(`generated ${target}`);
  }
}

function generateBeetsConfig(): void {
  const shipped = readFileSync(join(dirname(e2eRoot), 'beets_config.yaml'), 'utf8');
  const config = parse(shipped) as Record<string, unknown>;

  const musicbrainz = (config.musicbrainz ?? {}) as Record<string, unknown>;
  musicbrainz.host = mbHost;
  musicbrainz.https = false;
  config.musicbrainz = musicbrainz;

  const target = join(e2eRoot, '.fixtures', 'beets-e2e.yaml');
  mkdirSync(dirname(target), { recursive: true });
  writeFileSync(target, stringify(config));
  console.log(`generated ${target} (musicbrainz host ${mbHost})`);
}

function createRuntimeDirs(): void {
  for (const dir of ['downloads', 'music', 'data', 'navidrome', 'user90-config', 'music-user90']) {
    mkdirSync(join(runtimeDir, dir), { recursive: true });
  }
}

// The issue #90 config: no musicbrainz in plugins. Copied on every run, since user90.spec.ts replaces it with a
// working one.
function generateUser90Config(): void {
  const target = join(runtimeDir, 'user90-config', 'config.yaml');
  copyFileSync(user90BrokenConfig, target);
  console.log(`generated ${target}`);
}

generateAudio();
generateBeetsConfig();
createRuntimeDirs();
generateUser90Config();
