// In-memory slskd double: searches, transfers and their lifecycle. The wire
// shapes mirror slskd's API v0 (System.Text.Json camelCase, TransferStates
// flag strings) as implemented in the slskd C# source.

import { randomUUID } from 'node:crypto';
import { copyFileSync, mkdirSync, statSync } from 'node:fs';
import { join } from 'node:path';
import {
  audioFileName,
  peers,
  type Peer,
  type PeerBehavior,
  type PeerShare,
} from '../../fixtures/dataset.js';

export interface SearchFile {
  filename: string;
  size: number;
  extension: string;
  code: number;
  bitRate: number | null;
  sampleRate: number | null;
  bitDepth: number | null;
  length: number;
  isVariableBitRate: boolean;
  isLocked: boolean;
}

export interface SearchResponseJson {
  username: string;
  token: number;
  uploadSpeed: number;
  queueLength: number;
  fileCount: number;
  lockedFileCount: number;
  hasFreeUploadSlot: boolean;
  files: SearchFile[];
  lockedFiles: SearchFile[];
}

interface SearchRecord {
  id: string;
  searchText: string;
  token: number;
  startedAt: Date;
  /** epoch ms after which the search reports complete */
  completeAtMs: number;
  responses: SearchResponseJson[];
}

interface TransferStep {
  afterMs: number;
  state: string;
  percent: number;
  exception?: string;
  deliverFile?: boolean;
}

interface TransferRecord {
  id: string;
  username: string;
  filename: string;
  size: number;
  requestedAtMs: number;
  enqueuedAtMs: number | null;
  startedAtMs: number | null;
  endedAtMs: number | null;
  state: string;
  percentComplete: number;
  exception: string | null;
  /** ghost transfers are accepted but never show up in listings */
  hidden: boolean;
  removed: boolean;
  plan: TransferStep[];
  /** number of plan steps already applied */
  applied: number;
  delivered: boolean;
}

// Transfers hold "Requested" and "Initializing" for longer than the app's 2s
// monitor poll interval so every download is guaranteed to be observed in
// those states: treating them as terminal made the monitor cancel live
// transfers (issue #71).
const BEHAVIOR_PLANS: Record<Exclude<PeerBehavior, 'offline'>, TransferStep[]> = {
  happy: [
    { afterMs: 0, state: 'Queued, Locally', percent: 0 },
    { afterMs: 100, state: 'Requested', percent: 0 },
    { afterMs: 2600, state: 'Queued, Remotely', percent: 0 },
    { afterMs: 3400, state: 'Initializing', percent: 0 },
    { afterMs: 5900, state: 'InProgress', percent: 24 },
    { afterMs: 6600, state: 'InProgress', percent: 71 },
    { afterMs: 7300, state: 'Completed, Succeeded', percent: 100, deliverFile: true },
  ],
  flaky: [
    { afterMs: 0, state: 'Queued, Locally', percent: 0 },
    { afterMs: 100, state: 'Requested', percent: 0 },
    { afterMs: 2600, state: 'Queued, Remotely', percent: 0 },
    { afterMs: 3100, state: 'InProgress', percent: 31 },
    {
      afterMs: 4000,
      state: 'Completed, Errored',
      percent: 31,
      exception: 'Connection reset by peer',
    },
  ],
  stall: [
    { afterMs: 0, state: 'Queued, Locally', percent: 0 },
    { afterMs: 400, state: 'Queued, Remotely', percent: 0 },
  ],
  ghost: [{ afterMs: 0, state: 'Queued, Locally', percent: 0 }],
};

/**
 * slskd maps a remote path to {downloads}/{last directory}/{filename}
 * (Extensions.ToLocalRelativeFilename in the slskd source). The stub delivers
 * completed files to the same place so the app's path resolution sees exactly
 * what a real slskd would produce.
 */
export function toLocalRelativePath(remotePath: string): { directory: string; file: string } {
  const parts = remotePath.split('\\').filter((p) => p.length > 0);
  const file = parts[parts.length - 1] ?? remotePath;
  const directory = parts.length >= 2 ? (parts[parts.length - 2] as string) : '';
  return { directory, file };
}

export class SlskdState {
  private searches = new Map<string, SearchRecord>();
  private transfers: TransferRecord[] = [];
  private behaviors = new Map<string, PeerBehavior>();
  private fileSizes = new Map<string, number>();
  private nextToken = 1000;
  outage = false;

  constructor(
    private readonly audioDir: string,
    private readonly downloadsDir: string,
    private readonly searchCompleteAfterMs = 1500,
  ) {
    this.resetBehaviors();
    for (const peer of peers) {
      for (const share of peer.shares) {
        const path = join(audioDir, audioFileName(share));
        try {
          this.fileSizes.set(share.remotePath, statSync(path).size);
        } catch {
          throw new Error(
            `audio fixture missing for ${share.remotePath} (${path}); run "npm run fixtures" first`,
          );
        }
      }
    }
  }

  reset(): void {
    this.searches.clear();
    this.transfers = [];
    this.resetBehaviors();
    this.outage = false;
  }

  private resetBehaviors(): void {
    this.behaviors.clear();
    for (const peer of peers) {
      this.behaviors.set(peer.username, peer.defaultBehavior);
    }
  }

  setBehavior(username: string, behavior: PeerBehavior): boolean {
    if (!this.behaviors.has(username)) {
      return false;
    }
    this.behaviors.set(username, behavior);
    return true;
  }

  behaviorOf(username: string): PeerBehavior {
    return this.behaviors.get(username) ?? 'happy';
  }

  // ---- searches ----------------------------------------------------------

  createSearch(searchText: string, id?: string): SearchRecord {
    const record: SearchRecord = {
      id: id ?? randomUUID(),
      searchText,
      token: this.nextToken++,
      startedAt: new Date(),
      completeAtMs: Date.now() + this.searchCompleteAfterMs,
      responses: this.buildResponses(searchText),
    };
    this.searches.set(record.id, record);
    console.log(
      `[slskd] search "${searchText}" -> ${record.responses.length} responses (${record.id})`,
    );
    return record;
  }

  getSearch(id: string): SearchRecord | undefined {
    return this.searches.get(id);
  }

  deleteSearch(id: string): boolean {
    return this.searches.delete(id);
  }

  searchJson(record: SearchRecord): Record<string, unknown> {
    const isComplete = Date.now() >= record.completeAtMs;
    return {
      id: record.id,
      searchText: record.searchText,
      token: record.token,
      state: isComplete ? 'Completed, Succeeded' : 'InProgress',
      isComplete,
      startedAt: record.startedAt.toISOString(),
      endedAt: isComplete ? new Date(record.completeAtMs).toISOString() : null,
      responseCount: record.responses.length,
      fileCount: record.responses.reduce((n, r) => n + r.files.length, 0),
      lockedFileCount: 0,
    };
  }

  /**
   * Soulseek-style matching: every search token must appear somewhere in the
   * shared file's full remote path.
   */
  private buildResponses(searchText: string): SearchResponseJson[] {
    const tokens = searchText
      .toLowerCase()
      .split(/\s+/)
      .filter((t) => t.length > 0);

    const responses: SearchResponseJson[] = [];
    for (const peer of peers) {
      if (this.behaviorOf(peer.username) === 'offline') {
        continue;
      }
      const files = peer.shares
        .filter((share) => {
          const haystack = share.remotePath.toLowerCase();
          return tokens.every((t) => haystack.includes(t));
        })
        .map((share) => this.searchFileJson(share));
      if (files.length > 0) {
        responses.push({
          username: peer.username,
          token: this.nextToken++,
          uploadSpeed: peer.uploadSpeed,
          queueLength: peer.queueLength,
          fileCount: files.length,
          lockedFileCount: 0,
          hasFreeUploadSlot: peer.hasFreeUploadSlot,
          files,
          lockedFiles: [],
        });
      }
    }
    return responses;
  }

  private searchFileJson(share: PeerShare): SearchFile {
    return {
      filename: share.remotePath,
      size: this.fileSizes.get(share.remotePath) ?? 0,
      extension: share.format,
      code: 1,
      bitRate: share.bitRate,
      sampleRate: share.sampleRate,
      bitDepth: share.bitDepth,
      length: 30,
      isVariableBitRate: false,
      isLocked: false,
    };
  }

  // ---- transfers ---------------------------------------------------------

  enqueue(
    username: string,
    files: Array<{ filename: string; size: number }>,
  ): { status: number; body: unknown } {
    const peer = peers.find((p) => p.username === username);
    if (!peer) {
      return { status: 404, body: `User ${username} not found` };
    }
    const behavior = this.behaviorOf(username);
    if (behavior === 'offline') {
      return { status: 404, body: `User ${username} appears to be offline` };
    }

    const plan = BEHAVIOR_PLANS[behavior];
    const now = Date.now();
    const enqueued: Array<Record<string, unknown>> = [];
    const failed: string[] = [];

    for (const file of files) {
      const share = peer.shares.find((s) => s.remotePath === file.filename);
      if (!share) {
        failed.push(file.filename);
        continue;
      }
      // Real slskd creates the record in "Queued, Locally"
      // (DownloadService.EnqueueAsync), then the download task overwrites the
      // state with Soulseek.NET's own progression starting at "Requested".
      const transfer: TransferRecord = {
        id: randomUUID(),
        username,
        filename: file.filename,
        size: this.fileSizes.get(file.filename) ?? file.size,
        requestedAtMs: now,
        enqueuedAtMs: null,
        startedAtMs: null,
        endedAtMs: null,
        state: 'Queued, Locally',
        percentComplete: 0,
        exception: null,
        hidden: behavior === 'ghost',
        removed: false,
        plan: plan.map((step) => ({ ...step })),
        applied: 1,
        delivered: false,
      };
      this.transfers.push(transfer);
      enqueued.push(this.transferJson(transfer));
    }

    console.log(
      `[slskd] enqueue ${files.length} file(s) from ${username} (${behavior}): ${enqueued.length} ok, ${failed.length} failed`,
    );
    return { status: 201, body: { enqueued, failed } };
  }

  /** Advance every transfer along its behavior plan; called on a short timer. */
  tick(): void {
    const now = Date.now();
    for (const transfer of this.transfers) {
      const elapsed = now - transfer.requestedAtMs;
      while (transfer.applied < transfer.plan.length) {
        const step = transfer.plan[transfer.applied] as TransferStep;
        if (elapsed < step.afterMs) {
          break;
        }
        transfer.applied++;
        this.applyStep(transfer, step);
      }
    }
  }

  private applyStep(transfer: TransferRecord, step: TransferStep): void {
    // Cancellation is terminal; the rest of the plan no longer applies.
    if (transfer.state.startsWith('Completed')) {
      return;
    }

    transfer.state = step.state;
    transfer.percentComplete = step.percent;
    if (step.state === 'Queued, Remotely' && transfer.enqueuedAtMs === null) {
      transfer.enqueuedAtMs = Date.now();
    }
    if (step.state === 'InProgress' && transfer.startedAtMs === null) {
      transfer.startedAtMs = Date.now();
    }
    if (step.state.startsWith('Completed')) {
      transfer.endedAtMs = Date.now();
      transfer.exception = step.exception ?? null;
    }
    if (step.deliverFile && !transfer.delivered) {
      transfer.delivered = true;
      this.deliver(transfer);
    }
  }

  private deliver(transfer: TransferRecord): void {
    const peer = peers.find((p) => p.username === transfer.username) as Peer;
    const share = peer.shares.find((s) => s.remotePath === transfer.filename);
    if (!share) {
      return;
    }
    const { directory, file } = toLocalRelativePath(transfer.filename);
    const targetDir = directory ? join(this.downloadsDir, directory) : this.downloadsDir;
    mkdirSync(targetDir, { recursive: true });
    const target = join(targetDir, file);
    copyFileSync(join(this.audioDir, audioFileName(share)), target);
    console.log(`[slskd] delivered ${target}`);
  }

  cancel(username: string, id: string, remove: boolean): boolean {
    const transfer = this.transfers.find((t) => t.username === username && t.id === id);
    if (!transfer) {
      return false;
    }
    if (!transfer.state.startsWith('Completed')) {
      transfer.state = 'Completed, Cancelled';
      transfer.endedAtMs = Date.now();
    }
    if (remove) {
      transfer.removed = true;
    }
    return true;
  }

  transferJson(transfer: TransferRecord): Record<string, unknown> {
    const bytesTransferred = Math.round((transfer.size * transfer.percentComplete) / 100);
    return {
      id: transfer.id,
      username: transfer.username,
      direction: 'Download',
      filename: transfer.filename,
      size: transfer.size,
      startOffset: 0,
      state: transfer.state,
      stateDescription: transfer.state,
      requestedAt: new Date(transfer.requestedAtMs).toISOString(),
      enqueuedAt: transfer.enqueuedAtMs ? new Date(transfer.enqueuedAtMs).toISOString() : null,
      startedAt: transfer.startedAtMs ? new Date(transfer.startedAtMs).toISOString() : null,
      endedAt: transfer.endedAtMs ? new Date(transfer.endedAtMs).toISOString() : null,
      bytesTransferred,
      bytesRemaining: transfer.size - bytesTransferred,
      averageSpeed: transfer.state === 'InProgress' ? 250000 : 0,
      percentComplete: transfer.percentComplete,
      elapsedTime: null,
      remainingTime: null,
      placeInQueue: null,
      exception: transfer.exception,
    };
  }

  /** GET /api/v0/transfers/downloads shape: user -> directories -> files. */
  downloadsJson(includeRemoved: boolean): Array<Record<string, unknown>> {
    const byUser = new Map<string, TransferRecord[]>();
    for (const transfer of this.transfers) {
      if (transfer.hidden || (transfer.removed && !includeRemoved)) {
        continue;
      }
      const list = byUser.get(transfer.username) ?? [];
      list.push(transfer);
      byUser.set(transfer.username, list);
    }

    const result: Array<Record<string, unknown>> = [];
    for (const [username, list] of byUser) {
      const byDirectory = new Map<string, TransferRecord[]>();
      for (const transfer of list) {
        const dir = transfer.filename.split('\\').slice(0, -1).join('\\');
        const dirList = byDirectory.get(dir) ?? [];
        dirList.push(transfer);
        byDirectory.set(dir, dirList);
      }
      result.push({
        username,
        directories: Array.from(byDirectory.entries()).map(([directory, files]) => ({
          directory,
          fileCount: files.length,
          files: files.map((t) => this.transferJson(t)),
        })),
      });
    }
    return result;
  }

  /** Debug/control snapshot for specs. */
  snapshot(): Record<string, unknown> {
    return {
      outage: this.outage,
      behaviors: Object.fromEntries(this.behaviors),
      searches: Array.from(this.searches.values()).map((s) => this.searchJson(s)),
      transfers: this.transfers.map((t) => this.transferJson(t)),
    };
  }
}
