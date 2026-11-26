# Clients

Soulbeet ships three client surfaces backed by your Soulbeet server and Navidrome library: the Web Studio Player, the native Desktop app (Linux and Windows), and the Android + Android Auto app.

The Desktop and Android apps both embed `lib/client-core`, an offline-first Rust engine that mirrors your library into local SQLite (WAL mode with FTS5 full-text search), manages an LRU audio and artwork cache (`LosslessFlac`, `CellularOpus160`, or custom Navidrome transcode profiles), queues offline ratings and scrobbles in a mutation outbox, and connects to Soulbeet Connect.

## Web Studio Player

The built-in web UI streams audio and cover art directly from your Navidrome server using scoped OpenSubsonic credentials (salt + MD5 token, never your plaintext password) returned by `/api/client/bootstrap`.

### Navidrome URL and CORS

When Soulbeet and Navidrome run on the same Docker network, `NAVIDROME_URL` is usually a container-internal address such as `http://navidrome:4533`. Browsers and external native clients cannot resolve that hostname.

- Set `NAVIDROME_PUBLIC_URL` to the address your browser and devices can reach (a LAN IP such as `http://192.168.1.50:4533`, or an HTTPS reverse-proxy URL such as `https://navidrome.example.com`). When set, `/api/client/bootstrap` advertises `NAVIDROME_PUBLIC_URL` instead of `NAVIDROME_URL`.
- **CORS note**: If Soulbeet and Navidrome are served on different origins (different hostnames or ports), ensure Navidrome or your reverse proxy allows cross-origin requests from your Soulbeet origin so browser audio streaming and artwork fetches are not blocked by CORS. Serving both behind the same reverse proxy avoids mixed-content issues (HTTPS Soulbeet loading HTTP Navidrome audio).

### Keyboard shortcuts

Global shortcuts are active whenever focus is outside text inputs, editable elements, and open modals:

| Shortcut | Action |
| --- | --- |
| `Ctrl+K` / `Cmd+K` | Open or close the Command Palette |
| `Space` | Play / Pause |
| `ArrowLeft` / `ArrowRight` | Seek backward / forward 5 seconds |
| `Shift+ArrowLeft` / `Shift+ArrowRight` | Previous / Next track |
| `M` | Mute / Unmute |
| `S` | Star / Unstar current track |
| `1` to `5` | Rate current track (1 to 5 stars) |
| `P` | Keep (promote) active Discovery track |
| `X` | Drop current track (5-second undo for pending Discovery tracks, deletion guard preview for library tracks) |
| `R` | Start Seed Radio from current track |
| `L` | Toggle Lyrics drawer (synced karaoke lyrics) |
| `Q` | Toggle Queue drawer |
| `I` | Toggle Inspector drawer |
| `Escape` | Close Command Palette or Downloads drawer |

### Soulbeet Connect

Soulbeet Connect provides live multi-device presence and playback handoff over WebSocket (`WS /api/ws/connect?device_id=<id>&name=<name>&platform=<platform>`). Each signed-in Web, Desktop, or Android client announces its device and publishes `PlaybackSnapshot` updates so you can transfer the active track, queue, and playback position between devices from the Connect picker in the player bar.

### 1-Tap Acquire & Play and On-Keep Archival Upgrade

From Studio Search, clicking play on a track you do not yet own runs the 1-tap Acquire & Play pipeline:

1. Soulbeet checks your Navidrome library first. If the track is already owned, playback starts immediately with zero downloads.
2. Otherwise Soulbeet searches enabled backends, prioritizing fast `Delivery::LossyOnly` sources when enabled and falling back to archival backends (such as Soulseek or Internet Archive).
3. Live stage events (`Acquiring -> Importing -> Indexing -> Ready`) stream over the user WebSocket, and playback starts automatically once Navidrome indexes the file.
4. Lossy fast acquisitions are recorded in `fast_acquisitions` without automatically re-downloading FLAC. If you later **Keep**, **Star**, or **Promote** the track, Soulbeet queues a low-priority background archival FLAC upgrade on an archival backend and replaces the file cleanly while preserving Navidrome identity, ratings, and playlists.

### Multi-Listener Deletion Guard

On shared Navidrome servers, deleting or dropping a library track runs the Multi-Listener Deletion Guard before touching disk. Any track starred, rated above 1 star, or kept in a playlist by another household listener is protected. When a track is protected or cannot be verified safely, the UI opens a guard preview modal explaining which listeners or checks blocked deletion.

## Native Desktop App (Linux & Windows)

The desktop client (`desktop/`) is a native offline-first studio player built with `dioxus-desktop` and the shared UI components.

### Architecture

- **UI shell**: `dioxus-desktop` window (`1360x860` default, `960x640` minimum) sharing the Refined Midnight Studio components and keyboard shortcuts with the web client.
- **In-process `client-core`**: local SQLite WAL library mirror, FTS5 instant search, LRU offline audio/cover-art cache, offline curation/scrobble outbox, and Soulbeet Connect WebSocket.
- **Audio engine (`libmpv`)**: runs `libmpv2` on a dedicated worker thread with gapless next-track preloading (`gapless-audio=yes`, `prefetch-playlist=yes`), ReplayGain loudness normalization (`Track`, `Album`, or `Off`), and live decoder telemetry (codec, sample rate, format, bitrate).
- **OS media controls (`souvlaki`)**: integrates with Linux MPRIS2 over D-Bus and Windows SMTC for hardware media keys, lock screen controls, and desktop notifications.
- **Floating Mini-Player**: frameless, always-on-top `360x110` secondary window with drag handle, artwork, mono codec badge, transport controls, star toggle, context-aware Discovery Keep / Drop buttons, and a seekable progress rail.

### Sign-in and offline cache

The Setup view accepts either:

- **Username and password** against your Soulbeet server URL (exchanging credentials for an `auth_token` session cookie), or
- **Token sign-in** using a pasted Bearer token or `auth_token` session cookie.

Credentials are saved in the desktop app config directory with `0600` permissions on Unix. Tracks can be pinned explicitly for offline listening; unpinned cached tracks are evicted LRU when the configured cache budget is exceeded. Toggling Offline Only mode restricts playback and browsing to locally cached tracks.

### System dependencies (Linux)

Building and running the desktop app on Linux requires development headers for `libmpv`, WebKitGTK 4.1, GTK 3, and D-Bus (`pkg-config` names: `mpv`, `webkit2gtk-4.1`, `gtk+-3.0`, `dbus-1`):

- **Debian / Ubuntu**: `libmpv-dev libwebkit2gtk-4.1-dev libgtk-3-dev libdbus-1-dev`
- **Fedora**: `mpv-libs-devel webkit2gtk4.1-devel gtk3-devel dbus-devel`
- **Arch Linux**: `mpv webkit2gtk-4.1 gtk3 dbus`

### Running and packaging

Run from the repository root:

```bash
cargo run -p desktop
```

Run desktop unit tests:

```bash
cargo test -p desktop
```

Package a Linux AppImage (builds with `dx bundle --package desktop --platform desktop --release` if `dx` is installed, or falls back to `cargo build --release --locked -p desktop`, assembles `target/AppDir`, and invokes `appimagetool` if available to produce `target/Soulbeet-x86_64.AppImage`):

```bash
./desktop/packaging/build-linux-appimage.sh
```

Prebuilt Linux (`soulbeet-<tag>-linux-x86_64.AppImage`) and Windows (`soulbeet-<tag>-windows-x86_64.zip`, bundled with `libmpv-2.dll`) binaries are attached to every GitHub Release alongside `SHA256SUMS.txt`. The Linux AppImage expects host `libmpv` (`libmpv.so.2`), `webkit2gtk-4.1`, and `glibc >= 2.39` at runtime.

## Android & Android Auto

The Android client (`android/`, `minSdk = 26`, `targetSdk = 36`) is a multi-module Gradle project wrapping the same Rust `client-core` engine.

### Architecture

- **`:core-bindings`**: cross-compiled `libclient_core.so` JNI libraries (`arm64-v8a`, `armeabi-v7a`, `x86_64`) and generated UniFFI Kotlin bindings (`org.soulbeet.core`).
- **`:android-media`**: Media3 ExoPlayer playback service (`SoulbeetMediaLibraryService`), Android Auto browse tree (`AutoBrowseTree`), custom actions (`AutoCustomActions`), background sync worker (`OfflineSyncWorker`), scrobble tracking (`ScrobbleTracker`), local cover art provider (`CoverArtContentProvider`), and cache-aware streaming (`SoulbeetDataSourceFactory`).
- **`:app`**: Jetpack Compose phone UI (`Home`, `Discover`, `Search`, `Library`, `NowPlayingSheet`, `OnboardingScreen`, `SettingsSheet`) with 5-second undo drop handling and synced lyrics.

### Android Auto

Android Auto exposes a 4-tab media browser (`For You`, `Playlists`, `Library`, `Radio`) backed entirely by local SQLite queries in `client-core` so browsing works with zero network latency and continues working offline:

- **For You**: pending Discovery triage shortcut, active Moment Mixes, and Seasonal Packs.
- **Playlists**: synced Navidrome playlists.
- **Library**: Offline tracks, Recent albums, and A-Z artist buckets (`A-D`, `E-H`, `I-L`, `M-P`, `Q-T`, `U-Z / #`).
- **Radio**: quick-launch Seed Radio stations.

Voice search resolves against the local FTS5 index, and the last playback session is persisted in SQLite for automatic resumption on car ignition.

### Offline sync and credential storage

- **Background sync**: `OfflineSyncWorker` (Android WorkManager) drains the offline scrobble/curation outbox, refreshes the local SQLite mirror from Soulbeet and Navidrome, and runs Wi-Fi auto-cache pinning rules (defaulting to `CellularOpus160` on mobile data and `LosslessFlac` on Wi-Fi, configurable in settings).
- **Encrypted credentials**: session secrets (`auth_token`, `subsonic_token`, `subsonic_salt`) are encrypted with an Android Keystore AES-256-GCM key (`SecretCipher`) before being stored in DataStore Preferences.

### Building the Android app

Prerequisites: Rust Android targets, `cargo-ndk` (`cargo install cargo-ndk`), Android NDK (auto-discovered from `ANDROID_NDK_HOME` or `~/Android/Sdk/ndk/`), and Android SDK (API 35).

1. Cross-compile `client-core` JNI `.so` libraries and generate the UniFFI Kotlin bindings:

```bash
./scripts/build-android-core.sh
```

(Pass `--profile debug` or `--abis "arm64-v8a"` for faster local iteration.)

2. Run Android unit tests and build the debug APK:

```bash
cd android
./gradlew testDebugUnitTest
./gradlew :app:assembleDebug
```

The built APK lands in `android/app/build/outputs/apk/debug/`.

### Prebuilt releases, release signing, and Google Play publishing

Every GitHub Release attaches an installable Android APK alongside `SHA256SUMS.txt`. To publish release-signed APKs (`soulbeet-<tag>-android.apk`) and signed Android App Bundles (`.aab`) instead of debug-signed APKs (`soulbeet-<tag>-android-debug.apk`), configure all four signing secrets in GitHub Actions: `ANDROID_KEYSTORE_BASE64`, `ANDROID_KEYSTORE_PASSWORD`, `ANDROID_KEY_ALIAS`, and `ANDROID_KEY_PASSWORD`. Local Gradle builds continue to work without any signing environment variables.

To enable automated Google Play uploads on every signed release, add the `GOOGLE_PLAY_SERVICE_ACCOUNT_JSON` repository secret containing the JSON key for a Google Play Console API service account with release permissions on `soulbeet.insynia.fr`:

- Prerelease tags (such as `v1.0.0-alpha-1`) automatically upload to the `internal` testing track; stable tags (`v1.0.0`) default to `beta`.
- Manual `workflow_dispatch` runs accept `play_track` (`auto`, `internal`, `alpha`, `beta`, `production`, `skip`) and `play_status` (`completed` or `draft`). While the app is still in Play Console's initial Draft state before first review approval, either dispatch with `play_status=draft` or set repository variable `GOOGLE_PLAY_DEFAULT_STATUS=draft`.
- Note that Google Play requires uploading the very first signed `.aab` manually in Play Console to enroll the app in Play App Signing before API uploads succeed.
