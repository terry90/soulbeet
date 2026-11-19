// The downloads panel shows what the browser hears on its updates socket. For
// its picture the socket is answered here with an album part way through, so
// the instance downloads nothing and the picture does not depend on what the
// account happens to have in its panel that day.

import type { Page, WebSocketRoute } from '@playwright/test';

/** Each step a file goes through, in the order the panel lists an album that is half done. */
const STEPS = [
  { state: 'Imported', percent: 100, index: 'listed' },
  { state: 'Imported', percent: 100, index: 'listed' },
  { state: 'Importing', percent: 100 },
  { state: 'InProgress', percent: 62 },
  { state: 'Queued', percent: 0 },
] as const;

/**
 * Answers the page's updates socket instead of the instance. Call before the
 * page loads; the returned function then shows `titles`, the first tracks of
 * `album`, as files on their way into the library.
 */
export async function holdTransfers(page: Page): Promise<(artist: string, album: string, titles: string[]) => void> {
  const sockets: WebSocketRoute[] = [];
  await page.routeWebSocket(/\/api\/downloads\/updates/, (socket) => {
    sockets.push(socket);
  });
  return (artist, album, titles) => {
    const rows = STEPS.slice(0, titles.length).map((step, i) => {
      const size = (24 + ((i * 7) % 15)) * 1_000_000;
      return {
        id: `shot-${i}`,
        source: 'peer',
        item: `${artist}\\${album}\\${String(i + 1).padStart(2, '0')} ${titles[i].replace(/[\\/]/g, ' ')}.flac`,
        size,
        transferred: Math.round((size * step.percent) / 100),
        state: step.state,
        percent: step.percent,
        speed: step.state === 'InProgress' ? 1_400_000 : 0,
        error: null,
        backend: 'slskd',
        batch_id: 'shot',
        batch_label: album,
        index: 'index' in step ? step.index : null,
      };
    });
    // The app sends its events as JSON in binary frames.
    const frame = Buffer.from(JSON.stringify({ Progress: rows }));
    for (const socket of sockets) {
      socket.send(frame);
    }
  };
}
