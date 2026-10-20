// Host-side filesystem assertions on the bind-mounted music and downloads
// directories: the final word on whether the pipeline really worked.

import { existsSync, readdirSync } from 'node:fs';
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

/** Every regular file under `dir`, as absolute paths; empty when `dir` is missing. */
export function listFilesRecursive(dir: string): string[] {
  if (!existsSync(dir)) {
    return [];
  }
  return readdirSync(dir, { recursive: true, withFileTypes: true })
    .filter((entry) => entry.isFile())
    .map((entry) => join(entry.parentPath, entry.name));
}
