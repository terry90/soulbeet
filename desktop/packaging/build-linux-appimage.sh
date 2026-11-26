#!/usr/bin/env bash
set -euo pipefail

# Builds a release Linux bundle of Soulbeet Desktop and packages it for AppImage / desktop installation.
# Prerequisites:
#   - Rust toolchain + optional `dx` CLI (`cargo install dioxus-cli`)
#   - Build libraries: libmpv (2.x), webkit2gtk-4.1, gtk3, dbus-1
#   - Optional: `appimagetool` on PATH to produce an .AppImage
# Runtime note:
#   The produced AppImage packages the Soulbeet binary and desktop metadata, and expects
#   host-provided libmpv (libmpv.so.2), webkit2gtk-4.1, and glibc >= 2.39 at runtime.

CALLER_PWD="$PWD"
if [[ -n "${OUTPUT_PATH:-}" && "${OUTPUT_PATH}" != /* ]]; then
    OUTPUT_PATH="${CALLER_PWD}/${OUTPUT_PATH}"
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"

export ARCH="${ARCH:-x86_64}"
export APPIMAGE_EXTRACT_AND_RUN="${APPIMAGE_EXTRACT_AND_RUN:-1}"

if [[ ! -s "$REPO_ROOT/web/assets/tailwind.css" ]]; then
    echo "ERROR: missing compiled stylesheet at web/assets/tailwind.css (embedded by desktop/src/assets.rs)." >&2
    exit 1
fi

echo "==> Checking required system libraries via pkg-config..."
for pkg in mpv webkit2gtk-4.1 gtk+-3.0 dbus-1; do
    if ! pkg-config --exists "$pkg"; then
        echo "ERROR: missing development package '$pkg' (install via dnf/apt/pacman)." >&2
        exit 1
    fi
    echo "    $pkg $(pkg-config --modversion "$pkg")"
done

echo "==> Building Soulbeet Desktop release bundle..."
if command -v dx >/dev/null 2>&1; then
    dx bundle --package desktop --platform desktop --release
    BIN_SRC="$(find "$TARGET_DIR/dx/desktop/release/linux/app" -maxdepth 2 -type f -executable | head -n 1 || true)"
else
    echo "NOTE: 'dx' CLI not found; falling back to cargo build --release --locked -p desktop"
    cargo build --release --locked -p desktop
    BIN_SRC="$TARGET_DIR/release/desktop"
fi

if [[ -z "${BIN_SRC:-}" || ! -f "$BIN_SRC" ]]; then
    echo "ERROR: could not locate compiled desktop binary." >&2
    exit 1
fi

APPDIR="$TARGET_DIR/AppDir"
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/icons/hicolor/scalable/apps"

install -m 0755 "$BIN_SRC" "$APPDIR/usr/bin/soulbeet-desktop"
install -m 0644 "$REPO_ROOT/desktop/packaging/org.soulbeet.Desktop.desktop" \
    "$APPDIR/usr/share/applications/org.soulbeet.Desktop.desktop"
cp "$REPO_ROOT/desktop/packaging/org.soulbeet.Desktop.desktop" "$APPDIR/org.soulbeet.Desktop.desktop"
install -m 0644 "$REPO_ROOT/desktop/packaging/org.soulbeet.Desktop.svg" \
    "$APPDIR/usr/share/icons/hicolor/scalable/apps/org.soulbeet.Desktop.svg"
cp "$REPO_ROOT/desktop/packaging/org.soulbeet.Desktop.svg" "$APPDIR/org.soulbeet.Desktop.svg"
cp "$REPO_ROOT/desktop/packaging/org.soulbeet.Desktop.svg" "$APPDIR/.DirIcon"

cat > "$APPDIR/AppRun" <<'EOF'
#!/usr/bin/env bash
HERE="$(dirname "$(readlink -f "${0}")")"
exec "$HERE/usr/bin/soulbeet-desktop" "$@"
EOF
chmod +x "$APPDIR/AppRun"

if command -v appimagetool >/dev/null 2>&1; then
    OUT="${OUTPUT_PATH:-$TARGET_DIR/Soulbeet-${ARCH}.AppImage}"
    mkdir -p "$(dirname "$OUT")"
    ARCH="$ARCH" APPIMAGE_EXTRACT_AND_RUN="$APPIMAGE_EXTRACT_AND_RUN" appimagetool "$APPDIR" "$OUT"
    echo "==> Created AppImage: $OUT"
else
    echo "==> Prepared AppDir at: $APPDIR (install appimagetool to pack into .AppImage)"
fi
