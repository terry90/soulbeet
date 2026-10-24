// Host-side filesystem assertions on the bind-mounted music and downloads
// directories: the final word on whether the pipeline really worked.

import { existsSync, readdirSync, type Dirent } from 'node:fs';
import { join } from 'node:path';
import { expect } from '@playwright/test';
import { musicDirOnHost } from './env.js';
import { DatabaseSync } from './sqlite.js';

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

/**
 * Every regular file under `dir`, as absolute paths. The app prunes emptied
 * download folders while a scan runs, so a directory that is gone by the time
 * it is read counts as empty.
 */
export function listFilesRecursive(dir: string): string[] {
  let entries: Dirent[];
  try {
    entries = readdirSync(dir, { withFileTypes: true });
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code === 'ENOENT') {
      return [];
    }
    throw err;
  }
  return entries.flatMap((entry) => {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) {
      return listFilesRecursive(path);
    }
    return entry.isFile() ? [path] : [];
  });
}

/** A track as a folder's beets library records it. */
export interface LibraryItem {
  title: string;
  /** empty when beets did not match it to a recording */
  mbTrackId: string;
}

/** Every track in the folder's beets library (`.beets_library.db` at its root). */
export function libraryItems(folderName: string): LibraryItem[] {
  const db = new DatabaseSync(join(musicDirOnHost, folderName, '.beets_library.db'), {
    readOnly: true,
  });
  try {
    const rows = db.prepare('SELECT title, mb_trackid FROM items').all() as {
      title: string;
      mb_trackid: string;
    }[];
    return rows.map((row) => ({ title: row.title, mbTrackId: row.mb_trackid }));
  } finally {
    db.close();
  }
}
