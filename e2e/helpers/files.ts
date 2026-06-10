// Host-side filesystem assertions on the bind-mounted music and downloads
// directories: the final word on whether the pipeline really worked.

import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { expect } from '@playwright/test';
import { musicDirOnHost } from './env.js';

/**
 * Path of an imported singleton on the host, following the shipped beets
 * config: {folder}/$albumartist/$album/$title.{ext}
 */
export function importedTrackPath(
  folderName: string,
  artist: string,
  album: string,
  title: string,
  ext: string,
): string {
  return join(musicDirOnHost, folderName, artist, album, `${title}.${ext}`);
}

export async function expectFileAppears(path: string, timeoutMs = 60_000): Promise<void> {
  await expect
    .poll(() => existsSync(path), {
      message: `expected file to appear: ${path}`,
      timeout: timeoutMs,
    })
    .toBe(true);
}

export function fileExists(path: string): boolean {
  return existsSync(path);
}
