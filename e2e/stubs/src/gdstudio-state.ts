// What the GD Studio double does, as the specs set it through /_control: an outage, a track
// whose URL names another MD5 than its file's, and the API calls it answered, for the budget
// checks.

/** How the double answers for one track. */
export type TrackBehavior =
  /** Its URL and file as they are. */
  | 'normal'
  /** A URL naming another MD5 than its file's. */
  | 'wrongmd5';

/** How the API answers every call. */
export type ApiMode = 'up' | 'outage';

const WINDOW_MS = 5 * 60 * 1000;

export class GdStudioState {
  mode: ApiMode = 'up';
  private behaviors = new Map<string, TrackBehavior>();
  /** When each API call came, oldest first. */
  private calls: number[] = [];

  behaviorOf(trackId: string): TrackBehavior {
    return this.behaviors.get(trackId) ?? 'normal';
  }

  setBehavior(trackId: string, behavior: TrackBehavior): void {
    if (behavior === 'normal') {
      this.behaviors.delete(trackId);
    } else {
      this.behaviors.set(trackId, behavior);
    }
  }

  /** Counts one API call, whatever it is answered. */
  called(at = Date.now()): void {
    this.calls.push(at);
  }

  /** The calls so far, and the most that came in any 5 minutes. */
  callStats(): { total: number; maxInWindow: number } {
    let maxInWindow = 0;
    let start = 0;
    for (let end = 0; end < this.calls.length; end++) {
      while ((this.calls[end] ?? 0) - (this.calls[start] ?? 0) >= WINDOW_MS) {
        start++;
      }
      maxInWindow = Math.max(maxInWindow, end - start + 1);
    }
    return { total: this.calls.length, maxInWindow };
  }

  reset(): void {
    this.mode = 'up';
    this.behaviors.clear();
    this.calls = [];
  }
}
