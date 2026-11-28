#!/usr/bin/env node
// Seeds an isolated Soulbeet dev SQLite database with realistic Studio data:
// folders, encrypted Navidrome credentials, slskd/backend config, user settings,
// culture profile (Taste DNA), discovery triage tracks, and contextual mixes.
//
// Pass `--all-mixes` to force every built-in seasonal & moment mix into an active
// window so all 34 mix sleeves can be inspected live in the UI.

import { createCipheriv, createHash, randomBytes } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { mkdirSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = dirname(dirname(__dirname));

const DB_PATH = process.env.DEV_DB_PATH;
const SECRET_KEY = process.env.SECRET_KEY;
const BASE_URL = process.env.DEV_BASE_URL ?? 'http://127.0.0.1:9766';
const MUSIC_ROOT = process.env.NAVIDROME_MUSIC_PATH ?? '/tmp/soulbeet-dev-music';
const SLSKD_URL = (process.env.DEV_SLSKD_URL ?? '').trim();
const SLSKD_API_KEY = process.env.SLSKD_API_KEY ?? 'e2e-slskd-api-key';
const ALL_MIXES = process.argv.includes('--all-mixes') || process.env.DEV_ALL_MIXES === '1';

if (!DB_PATH || !SECRET_KEY) {
  console.error('DEV_DB_PATH and SECRET_KEY env vars are required');
  process.exit(1);
}
if (Buffer.byteLength(SECRET_KEY, 'utf8') < 32) {
  console.error('SECRET_KEY must be at least 32 bytes to match Soulbeet encryption requirements');
  process.exit(1);
}

const ADMIN_ID = '00000000-0000-0000-0000-000000000000';
const FOLDER_LIB_ID = 'dev-folder-library';
const FOLDER_SHARED_ID = 'dev-folder-shared';

function readCrateVersion() {
  const toml = readFileSync(join(REPO_ROOT, 'api', 'Cargo.toml'), 'utf8');
  const m = toml.match(/^version\s*=\s*"([^"]+)"/m);
  return m ? m[1] : '1.0.0-alpha-2';
}

// Mirrors api/src/crypto.rs: key = SHA256(SECRET_KEY), out = base64(nonce[12] || ct || tag[16])
function encryptNavidromePassword(plaintext, secretKey) {
  const key = createHash('sha256').update(Buffer.from(secretKey, 'utf8')).digest();
  const nonce = randomBytes(12);
  const cipher = createCipheriv('aes-256-gcm', key, nonce);
  const ct = Buffer.concat([cipher.update(plaintext, 'utf8'), cipher.final()]);
  const tag = cipher.getAuthTag();
  return Buffer.concat([nonce, ct, tag]).toString('base64');
}

function sqlStr(val) {
  if (val === null || val === undefined) return 'NULL';
  return `'${String(val).replace(/'/g, "''")}'`;
}

function runSql(sql) {
  execFileSync('sqlite3', [DB_PATH], {
    input: `PRAGMA foreign_keys = ON;\nBEGIN TRANSACTION;\n${sql}\nCOMMIT;\n`,
    stdio: ['pipe', 'inherit', 'inherit'],
  });
}

async function clearDefaultAdminPassword() {
  // Login as default admin/admin and switch password to devadmin123 via the real
  // Soulbeet API so Argon2id hashing and in-memory `SEEDED_ADMIN_HAS_DEFAULT_PASSWORD`
  // are both updated cleanly.
  const loginRes = await fetch(`${BASE_URL}/api/auth/login`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ username: 'admin', password: 'admin' }),
  });
  if (!loginRes.ok) {
    // Already changed on an earlier run; verify devadmin123 works or rely on autologin.
    return;
  }
  const setCookie = loginRes.headers.get('set-cookie') ?? '';
  const cookiePair = setCookie.split(';')[0];
  if (!cookiePair) return;

  const pwRes = await fetch(`${BASE_URL}/api/users/password`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', Cookie: cookiePair },
    body: JSON.stringify({ current_password: 'admin', password: 'devadmin123' }),
  });
  if (!pwRes.ok) {
    throw new Error(`Failed to clear default admin password: ${pwRes.status} ${await pwRes.text()}`);
  }
}

async function triggerBuiltinMixSeeding() {
  // Hitting contextual-mixes once ensures Soulbeet's own seed_builtins_in() populates
  // every built-in row in `contextual_mix_definitions` with authoritative defaults.
  const res = await fetch(`${BASE_URL}/api/discovery/contextual-mixes`);
  if (!res.ok) {
    console.warn(`[seed] GET /api/discovery/contextual-mixes returned ${res.status}`);
  }
}

function isoDateOffset(days) {
  const d = new Date(Date.now() + days * 86400_000);
  return d.toISOString().slice(0, 10);
}

function isoTimestampOffset(days) {
  return new Date(Date.now() + days * 86400_000).toISOString();
}

async function main() {
  const libDir = join(MUSIC_ROOT, 'library');
  const sharedDir = join(MUSIC_ROOT, 'shared');
  mkdirSync(libDir, { recursive: true });
  mkdirSync(sharedDir, { recursive: true });

  await clearDefaultAdminPassword();
  await triggerBuiltinMixSeeding();

  const encryptedNavPw = encryptNavidromePassword('devadmin123', SECRET_KEY);
  const crateVersion = readCrateVersion();

  const today = new Date();
  const startDay = new Date(today.getTime() - 10 * 86400_000);
  const endDay = new Date(today.getTime() + 25 * 86400_000);
  const activeCalendarWindow = JSON.stringify({
    start_month: startDay.getUTCMonth() + 1,
    start_day: startDay.getUTCDate(),
    end_month: endDay.getUTCMonth() + 1,
    end_day: endDay.getUTCDate(),
    cleanup_grace_days: 9,
  });
  const activeHourWindow = JSON.stringify({ start_hour: 0, end_hour: 23 });

  const cultureWeights = JSON.stringify([
    ['US', 0.52],
    ['GB', 0.24],
    ['FR', 0.14],
    ['JP', 0.1],
  ]);
  const cultureTags = JSON.stringify(['indie rock', 'dream pop', 'french electro', 'shoegaze']);
  const preferredLangs = JSON.stringify(['en', 'fr']);

  const triageRows = [
    {
      id: 'dev-disc-1',
      songId: 'tri-1',
      title: 'Winter Nocturne',
      artist: 'Luma Drift',
      album: 'Solstice Echoes',
      path: join(libDir, 'Discovery/Balanced/Winter Nocturne.flac'),
      profile: 'Balanced',
      daysAgo: -2,
      exclude: 0,
      reason: { kind: 'taste_wildcard', mix_slug: 'winter_holiday', genre: 'Dream Pop' },
      bucket: 'taste_wildcard',
      packSlug: 'winter_holiday',
    },
    {
      id: 'dev-disc-2',
      songId: 'tri-2',
      title: 'Snowline Telemetry',
      artist: 'Cinder Vale',
      album: 'Northbound Ephemera',
      path: join(libDir, 'Discovery/Balanced/Snowline Telemetry.flac'),
      profile: 'Balanced',
      daysAgo: -3,
      exclude: 0,
      reason: { kind: 'similar_artist', seed_artist: 'Hollow Pines', artist: 'Cinder Vale' },
      bucket: 'local',
      packSlug: null,
    },
    {
      id: 'dev-disc-3',
      songId: 'tri-3',
      title: 'Rue des Étoiles',
      artist: 'Clair Obscur',
      album: 'Minuit Analogique',
      path: join(libDir, 'Discovery/Adventurous/Rue des Etoiles.flac'),
      profile: 'Adventurous',
      daysAgo: -4,
      exclude: 1,
      reason: { kind: 'regional_contextual', mix_slug: 'winter_holiday', country_code: 'FR' },
      bucket: 'international',
      packSlug: 'winter_holiday',
    },
    {
      id: 'dev-disc-4',
      songId: 'tri-4',
      title: 'Phosphor Drift',
      artist: 'Static Harbor',
      album: 'Glass Atlas',
      path: join(libDir, 'Discovery/Balanced/Phosphor Drift.flac'),
      profile: 'Balanced',
      daysAgo: -5,
      exclude: 0,
      reason: { kind: 'moment_match', mix_slug: 'late_night', mood_tag: 'dream pop', genre: 'Shoegaze' },
      bucket: 'local',
      packSlug: 'late_night',
    },
    {
      id: 'dev-disc-5',
      songId: 'tri-5',
      title: 'Cedar & Frost',
      artist: 'Luma Drift',
      album: 'Solstice Echoes',
      path: join(libDir, 'Discovery/Conservative/Cedar and Frost.flac'),
      profile: 'Conservative',
      daysAgo: -6,
      exclude: 1,
      reason: { kind: 'international_contextual', mix_slug: 'winter_holiday' },
      bucket: 'international',
      packSlug: 'winter_holiday',
    },
  ];

  // Contextual mix states to surface cleanly on Home & Discover
  const mixStates = [
    { slug: 'winter_holiday', steering: 'balanced', localRatio: 0.45, playlistId: 'pl-winter', expiresAt: isoDateOffset(34) },
    { slug: 'drive', steering: 'balanced', localRatio: null, playlistId: 'pl-drive-mix', expiresAt: null },
    { slug: 'late_night', steering: 'balanced', localRatio: null, playlistId: 'pl-late-night', expiresAt: null },
  ];

  const sqlParts = [
    // 1. Update admin user: linked Navidrome + current changelog_seen
    `UPDATE users SET
      navidrome_token = ${sqlStr(encryptedNavPw)},
      navidrome_status = 'connected',
      changelog_seen = ${sqlStr(crateVersion)}
    WHERE id = ${sqlStr(ADMIN_ID)};`,

    // 2. Folder roots + folders (UPSERT avoids ON DELETE CASCADE wiping discovery_tracks on re-seed)
    `INSERT OR IGNORE INTO folder_roots (path) VALUES (${sqlStr(MUSIC_ROOT)});`,
    `INSERT INTO folders (id, user_id, name, path, shared) VALUES
      (${sqlStr(FOLDER_LIB_ID)}, ${sqlStr(ADMIN_ID)}, 'Library', ${sqlStr(libDir)}, 0),
      (${sqlStr(FOLDER_SHARED_ID)}, ${sqlStr(ADMIN_ID)}, 'Shared', ${sqlStr(sharedDir)}, 1)
     ON CONFLICT(id) DO UPDATE SET
      name = excluded.name,
      path = excluded.path,
      shared = excluded.shared;`,

    // 3. App config: enable Archive & YouTube Music toggles, and only wire slskd when a stub URL is provided
    `INSERT OR REPLACE INTO app_config (key, value) VALUES
      ('archive_enabled', 'true'),
      ('ytdlp_enabled', 'true');`,
    ...(SLSKD_URL
      ? [
          `INSERT OR REPLACE INTO app_config (key, value) VALUES
            ('slskd_url', ${sqlStr(SLSKD_URL)}),
            ('slskd_api_key', ${sqlStr(SLSKD_API_KEY)});`,
        ]
      : []),

    // 4. User settings (discovery_track_count, discovery_lifetime_days, discovery_playlist_name are JSON TEXT)
    `INSERT OR REPLACE INTO user_settings (
      user_id, last_search_type, auto_delete_enabled, discovery_promote_threshold,
      navidrome_banner_dismissed, listenbrainz_username, discovery_enabled,
      discovery_folder_id, discovery_track_count, discovery_lifetime_days,
      discovery_profiles, discovery_playlist_name, discovery_navidrome_playlist_id,
      discovery_last_generated_at, default_download_folder_id,
      deezer_recommendations, lossy_fallback, external_rating_sync_enabled
    ) VALUES (
      ${sqlStr(ADMIN_ID)}, 'album', 1, 4,
      1, 'studio_listener', 1,
      ${sqlStr(FOLDER_LIB_ID)},
      ${sqlStr(JSON.stringify({ Conservative: 10, Balanced: 15, Adventurous: 10 }))},
      ${sqlStr(JSON.stringify({ Conservative: 14, Balanced: 14, Adventurous: 14 }))},
      'Conservative,Balanced,Adventurous',
      ${sqlStr(JSON.stringify({ Conservative: 'Comfort Zone', Balanced: 'Fresh Picks', Adventurous: 'Deep Cuts' }))},
      ${sqlStr(JSON.stringify({ Balanced: 'pl-fresh' }))},
      ${sqlStr(isoTimestampOffset(-1))}, ${sqlStr(FOLDER_LIB_ID)},
      1, 'ask', 1
    );`,

    // 5. Culture profile (Your Taste DNA)
    `INSERT OR REPLACE INTO user_culture_profiles (
      user_id, home_country, preferred_languages, local_culture_ratio,
      inferred_area_weights, inferred_local_tags, updated_at
    ) VALUES (
      ${sqlStr(ADMIN_ID)}, 'US', ${sqlStr(preferredLangs)}, 0.45,
      ${sqlStr(cultureWeights)}, ${sqlStr(cultureTags)}, ${sqlStr(isoTimestampOffset(0))}
    );`,

    // 6. Discovery triage tracks
    `DELETE FROM discovery_tracks WHERE id LIKE 'dev-disc-%';`,
    ...triageRows.map(
      (t) => `INSERT INTO discovery_tracks (
        id, song_id, title, artist, album, path, folder_id,
        rating, status, created_at, profile, exclude_from_core_taste,
        reason_json, culture_bucket, pack_slug
      ) VALUES (
        ${sqlStr(t.id)}, ${sqlStr(t.songId)}, ${sqlStr(t.title)}, ${sqlStr(t.artist)},
        ${sqlStr(t.album)}, ${sqlStr(t.path)}, ${sqlStr(FOLDER_LIB_ID)},
        NULL, 'Pending', ${sqlStr(isoTimestampOffset(t.daysAgo))}, ${sqlStr(t.profile)}, ${t.exclude},
        ${sqlStr(JSON.stringify(t.reason))}, ${sqlStr(t.bucket)}, ${sqlStr(t.packSlug)}
      );`,
    ),

    // 7. Contextual mix windows: always surface winter_holiday hero + curated Right Now + Collection mixes
    `UPDATE contextual_mix_definitions
      SET calendar_window = ${sqlStr(activeCalendarWindow)}, enabled = 1
      WHERE slug = 'winter_holiday';`,
    `UPDATE contextual_mix_definitions
      SET always_surfaced = 1, hour_window = NULL, days_of_week = '[]', weather_conditions = '[]', enabled = 1
      WHERE slug IN ('drive', 'late_night', 'focus', 'forgotten_gems', 'fresh_arrivals', 'decade_80s', 'decade_90s', 'decade_2000s');`,

    ...mixStates.map(
      (m) => `INSERT OR REPLACE INTO contextual_mix_states (
        id, user_id, slug, steering, local_ratio, enabled_override,
        navidrome_playlist_id, expires_at, last_generated_at
      ) VALUES (
        ${sqlStr(`dev-mix-${m.slug}`)}, ${sqlStr(ADMIN_ID)}, ${sqlStr(m.slug)},
        ${sqlStr(m.steering)}, ${m.localRatio === null ? 'NULL' : m.localRatio}, 1,
        ${sqlStr(m.playlistId)}, ${sqlStr(m.expiresAt)}, ${sqlStr(isoTimestampOffset(-1))}
      );`,
    ),
  ];

  if (ALL_MIXES) {
    // Surface every single built-in seasonal & moment mix at once for full visual QA
    sqlParts.push(
      `UPDATE contextual_mix_definitions
        SET calendar_window = ${sqlStr(activeCalendarWindow)},
            hour_window = ${sqlStr(activeHourWindow)},
            always_surfaced = 1,
            countries = '[]',
            days_of_week = '[]',
            weather_conditions = '[]',
            enabled = 1;`,
    );
  }

  runSql(sqlParts.join('\n'));
  console.log(
    `[seed] Seeded ${DB_PATH} (${triageRows.length} triage tracks, folders, Navidrome, Taste DNA, contextual mixes${ALL_MIXES ? ' [ALL MIXES]' : ''}).`,
  );
}

main().catch((err) => {
  console.error('[seed] failed:', err);
  process.exit(1);
});
