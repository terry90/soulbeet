// Host-side filesystem assertions on the bind-mounted music and downloads
// directories: the final word on whether the pipeline really worked.

import { existsSync, readdirSync, type Dirent } from 'node:fs';
import { join } from 'node:path';
import { expect } from '@playwright/test';
import { librariesDirOnHost, musicDirForApp, musicDirOnHost } from './env.js';
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

/**
 * Every track in the folder's beets library, which the app keeps under
 * `beets-libraries` at the folder's path as the app sees it.
 */
export function libraryItems(folderName: string): LibraryItem[] {
  const path = join(librariesDirOnHost, musicDirForApp, folderName, '.beets_library.db');
  const db = new DatabaseSync(path, { readOnly: true });
  try {
    // beets may be writing its library: wait for its lock instead of failing.
    db.exec('PRAGMA busy_timeout = 5000');
    const rows = db.prepare('SELECT title, mb_trackid FROM items').all() as {
      title: string;
      mb_trackid: string;
    }[];
    return rows.map((row) => ({ title: row.title, mbTrackId: row.mb_trackid }));
  } finally {
    db.close();
  }
}
