// Entry point: boots the slskd, MusicBrainz, ListenBrainz, Labs, Deezer, archive.org, GD Studio
// and OpenID Connect doubles.

import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createArchiveServer } from './archive-server.js';
import { createDeezerServer } from './deezer-server.js';
import { createGdStudioServer } from './gdstudio-server.js';
import { GdStudioState } from './gdstudio-state.js';
import { createLabsServer, createListenBrainzServer } from './listenbrainz-server.js';
import { MbControl, createMusicBrainzServer } from './mb-server.js';
import { createOidcServer } from './oidc-server.js';
import { createSlskdServer } from './slskd-server.js';
import { SlskdState } from './slskd-state.js';
import { proxyFromEnv } from './tcp-proxy.js';

const e2eRoot = dirname(dirname(dirname(fileURLToPath(import.meta.url))));

const slskdPort = Number(process.env.SLSKD_PORT ?? 5030);
const mbPort = Number(process.env.MB_PORT ?? 5050);
const lbPort = Number(process.env.LB_PORT ?? 5060);
const labsPort = Number(process.env.LABS_PORT ?? 5061);
const deezerPort = Number(process.env.DEEZER_PORT ?? 5062);
const archivePort = Number(process.env.ARCHIVE_PORT ?? 5064);
const gdstudioPort = Number(process.env.GDSTUDIO_PORT ?? 5063);
const apiKey = process.env.SLSKD_API_KEY ?? 'e2e-slskd-api-key';
const audioDir = resolve(process.env.AUDIO_DIR ?? join(e2eRoot, '.fixtures', 'audio'));
const downloadsDir = resolve(process.env.DOWNLOADS_DIR ?? join(e2eRoot, '.runtime', 'downloads'));
const enqueueLatencyMs = Number(process.env.ENQUEUE_LATENCY_MS ?? 150);
// Not 5060: browsers refuse that port (SIP).
const oidcPort = Number(process.env.OIDC_PORT ?? 5070);
// The app and the browser reach the stub on different hosts in compose.
const oidcIssuer = process.env.OIDC_ISSUER ?? `http://127.0.0.1:${oidcPort}`;
const oidcBrowserUrl = process.env.OIDC_BROWSER_URL ?? oidcIssuer;
// Where the provider may send the browser back to: the callback at each
// address the suite reaches the app at. Set OIDC_REDIRECT_URIS (comma
// separated) when E2E_BASE_URL names another one.
const oidcRedirectUris = (
  process.env.OIDC_REDIRECT_URIS ?? 'http://127.0.0.1:9765/api/auth/oidc/callback'
).split(',');

const state = new SlskdState(audioDir, downloadsDir, undefined, enqueueLatencyMs);
setInterval(() => state.tick(), 200).unref();

const navidromeProxy = proxyFromEnv(process.env);
if (navidromeProxy) {
  await navidromeProxy.start();
  console.log(`[navidrome-proxy] listening on :${process.env.NAVIDROME_PROXY_PORT}`);
}

const slskd = createSlskdServer(state, apiKey, navidromeProxy);
slskd.listen(slskdPort, '0.0.0.0', () => {
  console.log(`[slskd] listening on :${slskdPort} (downloads -> ${downloadsDir})`);
});

const mb = createMusicBrainzServer(new MbControl());
mb.listen(mbPort, '0.0.0.0', () => {
  console.log(`[mb] listening on :${mbPort}`);
});

const lb = createListenBrainzServer();
lb.listen(lbPort, '0.0.0.0', () => {
  console.log(`[listenbrainz] listening on :${lbPort}`);
});

const labs = createLabsServer();
labs.listen(labsPort, '0.0.0.0', () => {
  console.log(`[labs] listening on :${labsPort}`);
});

const deezer = createDeezerServer();
deezer.listen(deezerPort, '0.0.0.0', () => {
  console.log(`[deezer] listening on :${deezerPort}`);
});

const archive = createArchiveServer(audioDir);
archive.listen(archivePort, '0.0.0.0', () => {
  console.log(`[archive] listening on :${archivePort}`);
});

const gdstudio = createGdStudioServer(audioDir, new GdStudioState());
gdstudio.listen(gdstudioPort, '0.0.0.0', () => {
  console.log(`[gdstudio] listening on :${gdstudioPort}`);
});

const oidc = createOidcServer({
  issuer: oidcIssuer,
  browserUrl: oidcBrowserUrl,
  clientId: 'soulbeet-e2e',
  clientSecret: 'e2e-oidc-secret',
  redirectUris: oidcRedirectUris,
});
oidc.listen(oidcPort, '0.0.0.0', () => {
  console.log(`[oidc] listening on :${oidcPort} (issuer ${oidcIssuer})`);
});

// Keep the tick interval from holding the process open on shutdown but make
// sure the servers do hold it open.
process.on('SIGTERM', () => {
  slskd.close();
  mb.close();
  lb.close();
  labs.close();
  deezer.close();
  archive.close();
  gdstudio.close();
  oidc.close();
  process.exit(0);
});
