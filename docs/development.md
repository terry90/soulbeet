# Development

This page covers running Soulbeet (server/web, desktop app, and Android app) from source, running the tests, and refreshing the screenshots.

## Running the server and web UI from source

1. Install Rust and `dioxus_cli`.
2. Run the tailwind watcher:
   ```bash
   ./css.sh
   ```
3. Run the app:
   ```bash
   dx serve --platform web
   ```
4. Run the server, core, and Python tests (the Python tests need Python 3.11+ with beets and pytest installed):
   ```bash
   cargo test --workspace --features server,dioxus/server --lib
   cargo test -p soulbeet --lib
   cargo test -p client-core
   python -m pytest -q lib/soulbeet/tests/probe lib/soulbeet/tests/import
   ```

## Seeded local dev build

To review UI and UX changes interactively in desktop and mobile browsers without touching real data or external services, run the one-command seeded dev stack (debug binary on port `9766` with passwordless `admin` auto-login, mock Navidrome with SVG covers and playable demo audio, local service stubs, and an isolated SQLite DB under `.runtime-dev/`):

```bash
./scripts/dev-seeded.sh up                 # Start stack (builds debug web bundle if needed)
./scripts/dev-seeded.sh up --build         # Rebuild debug web bundle and restart
./scripts/dev-seeded.sh reset --all-mixes  # Reset DB and surface every built-in mix sleeve
./scripts/dev-seeded.sh up --local-only    # Bind 127.0.0.1 instead of 0.0.0.0 (LAN)
./scripts/dev-seeded.sh down               # Stop all dev stack daemons
```

By default it binds `0.0.0.0:9766` (trusted LAN only) so you can open `http://127.0.0.1:9766` on your workstation or `http://<workstation-ip>:9766` on your phone. Auto-login (`SOULBEET_DEV_AUTOLOGIN=1`) is compiled only in debug builds and omitted from release binaries.

## Native desktop app (`desktop/`)

On Linux, install the development packages checked by `pkg-config` (`mpv`, `webkit2gtk-4.1`, `gtk+-3.0`, `dbus-1`):

- **Debian / Ubuntu**: `libmpv-dev libwebkit2gtk-4.1-dev libgtk-3-dev libdbus-1-dev`
- **Fedora**: `mpv-libs-devel webkit2gtk4.1-devel gtk3-devel dbus-devel`
- **Arch Linux**: `mpv webkit2gtk-4.1 gtk3 dbus`

Run and test from the repository root:

```bash
cargo test -p desktop
cargo run -p desktop
```

Build a release Linux bundle and AppDir (and `.AppImage` when `appimagetool` is on `PATH`):

```bash
./desktop/packaging/build-linux-appimage.sh
```

## Android and Android Auto (`android/`)

Prerequisites: `cargo-ndk` (`cargo install cargo-ndk`), Rust Android targets (`aarch64-linux-android`, `armv7-linux-androideabi`, `x86_64-linux-android`), Android NDK (auto-discovered from `ANDROID_NDK_HOME` or `~/Android/Sdk/ndk/`), and Android SDK (API 35).

1. Cross-compile `lib/client-core` into JNI `.so` libraries and generate UniFFI Kotlin bindings (`android/core-bindings/`):
   ```bash
   ./scripts/build-android-core.sh
   ```
   Pass `--profile debug --abis "arm64-v8a"` for faster local builds.
2. Run unit tests and assemble the debug APK:
   ```bash
   cd android
   ./gradlew testDebugUnitTest :app:assembleDebug
   ```

## Refreshing the screenshots

The pictures in [screenshots](../screenshots) (`home.png`, `discover.png`, `search.png`, `library.png`, `dashboard.png`, `downloads.png`, `settings.png`, `mobile.png`) are taken on the e2e stub stack, so they need no running Soulbeet, no account, and no real external service. The specs register their own account, search the stub MusicBrainz for "Night Garden" (fixture albums kept for the pictures), and stage the rest in the browser: drawn covers, the downloads panel part way through an album, a Navidrome library for the dashboard and its health, two folders, and the version check. The downloader and beets report their real health.

1. Build the app (the stack runs the release binary) from the repository root:
   ```bash
   npm ci && npx @tailwindcss/cli -i ./web/assets/input.css -o ./web/assets/tailwind.css
   dx bundle --package web --release
   ```
2. Start the stub stack and take the pictures:
   ```bash
   cd e2e && npm ci && npx playwright install chromium
   scripts/dev-stack.sh up
   npm run screenshots
   scripts/dev-stack.sh down
   ```
   The stack needs a Python 3.11 interpreter, as in the image (`PYTHON=/path/to/python3.11` if it is not `python3.11` on the PATH).
