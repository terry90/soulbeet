// What the AirDC++ double holds between requests: searches, open file lists and queued file
// bundles, each bundle written as `<target>.dctmp` in the shared downloads folder and renamed
// to its target once sent, as AirDC++ 2.14.0 does.

import { createHash } from 'node:crypto';
import { copyFileSync, existsSync, mkdirSync, readFileSync, renameSync, rmSync, statSync } from 'node:fs';
import { dirname, join } from 'node:path';
import {
  dcHubName,
  dcHubUrl,
  dcPeers,
  type DcFile,
  type DcPeer,
} from '../../fixtures/airdcpp-dataset.js';
import { audioFileName } from '../../fixtures/dataset.js';

/** At most this many results per peer per hub, as NMDC hubs send. */
const RESULTS_PER_PEER = 5;
/** After this long a queued file starts, and after this long again it is sent. */
const START_MS = 400;
const SEND_MS = 1200;

const BASE32 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567';

/** A Tiger-hash-shaped name for the bytes: 39 base32 characters of their SHA-256. */
function fakeTth(bytes: Buffer): string {
  const digest = createHash('sha256').update(bytes).digest();
  let bits = 0;
  let value = 0;
  let out = '';
  for (const byte of digest.subarray(0, 25)) {
    value = (value << 8) | byte;
    bits += 8;
    while (bits >= 5) {
      out += BASE32[(value >>> (bits - 5)) & 31];
      bits -= 5;
    }
  }
  return out.slice(0, 39);
}

export interface Shared {
  file: DcFile;
  peer: DcPeer;
  size: number;
  tth: string;
}

interface Bundle {
  id: number;
  target: string;
  shared: Shared;
  queuedAt: number;
  state: 'queued' | 'running' | 'completed';
  bytes: number;
}

export interface Removal {
  id: number;
  body: unknown;
  /** The bundle had finished: its file stays. */
  finished: boolean;
}

export class AirdcppState {
  /** Read from the generated audio on first use, which may come after the double starts. */
  private known: Shared[] | null = null;
  private bundles: Bundle[] = [];
  private nextBundle = 1000;
  /** Open file lists by CID: the folder asked for and how often it was looked at. */
  readonly lists = new Map<string, { directory: string; looks: number }>();
  searches = 0;
  removals: Removal[] = [];
  /** Every file queued since the last reset: its bundle, its peer and its target. */
  queued: { id: number; nick: string; target: string }[] = [];

  constructor(
    private readonly audioDir: string,
    readonly downloadsDir: string,
  ) {}

  private get shared(): Shared[] {
    this.known ??= dcPeers.flatMap((peer) =>
      peer.files.map((file) => {
        const bytes = readFileSync(join(this.audioDir, audioFileName(file)));
        return { file, peer, size: bytes.length, tth: fakeTth(bytes) };
      }),
    );
    return this.known;
  }

  /** The user as AirDC++ names one in its answers. */
  static user(peer: DcPeer): Record<string, unknown> {
    const flags = peer.behavior === 'passive' ? ['passive', 'nmdc'] : ['nmdc'];
    return {
      cid: peer.cid,
      nicks: peer.nick,
      hub_url: dcHubUrl,
      hub_urls: [dcHubUrl],
      hub_names: dcHubName,
      flags,
    };
  }

  /** The grouped results of a search for `pattern`: every word in the path, as AirDC++ matches. */
  search(pattern: string): Record<string, unknown>[] {
    this.searches += 1;
    const words = pattern.toLowerCase().split(/\s+/).filter(Boolean);
    const matches = (path: string) => words.every((w) => path.toLowerCase().includes(w));
    const results: Record<string, unknown>[] = [];
    for (const peer of dcPeers) {
      const mine = this.shared.filter((s) => s.peer === peer);
      const folders = [...new Set(mine.map((s) => `${dirname(s.file.path)}/`))].filter(matches);
      const found = [
        ...folders.map((folder) => ({ folder, shared: undefined as Shared | undefined })),
        ...mine.filter((s) => matches(s.file.path)).map((s) => ({ folder: undefined, shared: s })),
      ].slice(0, RESULTS_PER_PEER);
      for (const { folder, shared } of found) {
        const slots = {
          free: peer.freeSlots,
          total: peer.totalSlots,
          str: `${peer.freeSlots}/${peer.totalSlots}`,
        };
        const path = folder ?? shared!.file.path;
        results.push({
          id: shared ? shared.tth : `DIR${createHash('sha256').update(peer.cid + path).digest('hex').slice(0, 36).toUpperCase()}`,
          name: path.replace(/\/$/, '').split('/').pop(),
          path,
          size: shared ? shared.size : 0,
          tth: shared ? shared.tth : '',
          type: shared
            ? { id: 'file', str: shared.file.format, content_type: 'audio' }
            : { id: 'directory', str: '' },
          slots,
          users: { count: 1, user: AirdcppState.user(peer) },
          hits: 1,
          dupe: null,
          time: 0,
          relevance: 0.5,
          connection: 0,
        });
      }
    }
    return results;
  }

  peer(cid: string): DcPeer | undefined {
    return dcPeers.find((p) => p.cid === cid);
  }

  /** The items of `directory` in the file list of `peer`: its files and its folders. */
  listItems(peer: DcPeer, directory: string): Record<string, unknown>[] {
    const files = this.shared.filter((s) => s.peer === peer && dirname(s.file.path) + '/' === directory);
    return files.map((s, id) => ({
      id,
      name: s.file.path.split('/').pop(),
      path: s.file.path,
      size: s.size,
      tth: s.tth,
      complete: true,
      dupe: null,
      time: 0,
      type: { id: 'file', str: s.file.format, content_type: 'audio' },
    }));
  }

  /** Queues the file of `cid` with `tth` at `target`: the bundle id, or why it was refused. */
  queue(cid: string, tth: string, target: string): { id: number } | { refused: string } {
    const peer = this.peer(cid);
    if (!peer || peer.behavior === 'offline') {
      return { refused: 'User not found' };
    }
    const shared = this.shared.find((s) => s.peer === peer && s.tth === tth);
    if (!shared) {
      return { refused: 'The file was not found in the peer share' };
    }
    if (existsSync(target)) {
      return { refused: 'File exists on the disk already' };
    }
    if (this.bundles.some((b) => b.target === target)) {
      return { refused: 'A file with a different size already exists in the queue' };
    }
    const id = this.nextBundle++;
    this.bundles.push({ id, target, shared, queuedAt: Date.now(), state: 'queued', bytes: 0 });
    this.queued.push({ id, nick: peer.nick, target });
    return { id };
  }

  /** Moves each bundle on: started after a while, sent after a while more, unless it stalls. */
  tick(): void {
    const now = Date.now();
    for (const bundle of this.bundles) {
      const { peer, file, size } = bundle.shared;
      if (peer.behavior !== 'happy' || bundle.state === 'completed') {
        continue;
      }
      const source = join(this.audioDir, audioFileName(file));
      const partial = `${bundle.target}.dctmp`;
      if (bundle.state === 'queued' && now - bundle.queuedAt >= START_MS) {
        mkdirSync(dirname(bundle.target), { recursive: true });
        copyFileSync(source, partial);
        bundle.state = 'running';
        bundle.bytes = Math.floor(size / 3);
      }
      if (bundle.state === 'running' && !file.stalls && now - bundle.queuedAt >= START_MS + SEND_MS) {
        renameSync(partial, bundle.target);
        bundle.state = 'completed';
        bundle.bytes = size;
        console.log(`[airdcpp] delivered ${bundle.target}`);
      }
    }
  }

  /** The bundles as `GET /queue/bundles` lists them. */
  listedBundles(): Record<string, unknown>[] {
    return this.bundles.map((b) => {
      const percent = b.shared.size ? ((b.bytes * 100) / b.shared.size).toFixed(1) : '0.0';
      const status = {
        queued: { id: 'queued', str: `Waiting (${percent}%)` },
        running: { id: 'queued', str: `Running (${percent}%)` },
        completed: { id: 'completed', str: 'Finished' },
      }[b.state];
      return {
        id: b.id,
        name: b.target.split('/').pop(),
        target: b.target,
        size: b.shared.size,
        downloaded_bytes: b.bytes,
        speed: b.state === 'running' ? 65536 : 0,
        status: { ...status, failed: false, downloaded: b.state === 'completed', completed: b.state === 'completed', hook_error: null },
        sources: { online: 1, total: 1, str: '1/1 online' },
        type: { id: 'file', str: b.shared.file.format },
      };
    });
  }

  /** The transfers as `GET /transfers` lists them: running ones, and passive ones connecting. */
  transfers(): Record<string, unknown>[] {
    return this.bundles
      .filter((b) => b.state !== 'completed')
      .map((b, id) => ({
        id,
        download: true,
        target: b.target,
        name: b.target.split('/').pop(),
        size: b.shared.size,
        bytes_transferred: b.bytes,
        status:
          b.state === 'running'
            ? { id: 'running', str: 'Running' }
            : { id: 'waiting', str: 'Connecting...' },
        user: AirdcppState.user(b.shared.peer),
      }));
  }

  /** Removes a bundle: an unfinished one's `.dctmp` goes, a finished file stays. */
  remove(id: number, body: unknown): boolean {
    const bundle = this.bundles.find((b) => b.id === id);
    if (!bundle) {
      return false;
    }
    const finished = bundle.state === 'completed';
    if (!finished) {
      rmSync(`${bundle.target}.dctmp`, { force: true });
    }
    this.bundles = this.bundles.filter((b) => b !== bundle);
    this.removals.push({ id, body, finished });
    return true;
  }

  snapshot(): Record<string, unknown> {
    return {
      root: this.downloadsDir,
      searches: this.searches,
      bundles: this.bundles.map((b) => ({
        id: b.id,
        target: b.target,
        state: b.state,
        nick: b.shared.peer.nick,
        onDisk: statSafe(b.target),
      })),
      removals: this.removals,
      queued: this.queued,
    };
  }

  reset(): void {
    this.bundles = [];
    this.lists.clear();
    this.removals = [];
    this.queued = [];
  }
}

function statSafe(path: string): number | null {
  try {
    return statSync(path).size;
  } catch {
    return null;
  }
}
