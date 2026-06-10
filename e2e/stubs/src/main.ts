// Entry point: boots the slskd double and the MusicBrainz double.

import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createMusicBrainzServer } from './mb-server.js';
import { createSlskdServer } from './slskd-server.js';
import { SlskdState } from './slskd-state.js';

const e2eRoot = dirname(dirname(dirname(fileURLToPath(import.meta.url))));

const slskdPort = Number(process.env.SLSKD_PORT ?? 5030);
const mbPort = Number(process.env.MB_PORT ?? 5050);
const apiKey = process.env.SLSKD_API_KEY ?? 'e2e-slskd-api-key';
const audioDir = resolve(process.env.AUDIO_DIR ?? join(e2eRoot, '.fixtures', 'audio'));
const downloadsDir = resolve(process.env.DOWNLOADS_DIR ?? join(e2eRoot, '.runtime', 'downloads'));

const state = new SlskdState(audioDir, downloadsDir);
setInterval(() => state.tick(), 200).unref();

const slskd = createSlskdServer(state, apiKey);
slskd.listen(slskdPort, '0.0.0.0', () => {
  console.log(`[slskd] listening on :${slskdPort} (downloads -> ${downloadsDir})`);
});

const mb = createMusicBrainzServer();
mb.listen(mbPort, '0.0.0.0', () => {
  console.log(`[mb] listening on :${mbPort}`);
});

// Keep the tick interval from holding the process open on shutdown but make
// sure the servers do hold it open.
process.on('SIGTERM', () => {
  slskd.close();
  mb.close();
  process.exit(0);
});
