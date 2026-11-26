#!/usr/bin/env bash
set -euo pipefail

# Builds a release Linux bundle of Soulbeet Desktop and packages it for AppImage / desktop installation.
# Prerequisites:
#   - Rust toolchain + `dx` CLI (`cargo install dioxus-cli`)
#   - System libraries: libmpv (2.x), webkit2gtk-4.1, gtk3, dbus-1
#   - Optional: `appimagetool` on PATH to produce a standalone .AppImage

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

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
else
    echo "NOTE: 'dx' CLI not found; falling back to cargo build --release -p desktop"
    cargo build --release -p desktop
fi

APPDIR="$REPO_ROOT/target/AppDir"
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications"

BIN_SRC="$REPO_ROOT/target/release/desktop"
if [[ ! -f "$BIN_SRC" ]]; then
    BIN_SRC="$(find "$REPO_ROOT/target/dx/desktop/release/linux/app" -maxdepth 2 -type f -executable | head -n 1 || true)"
fi

if [[ -z "${BIN_SRC:-}" || ! -f "$BIN_SRC" ]]; then
    echo "ERROR: could not locate compiled desktop binary." >&2
    exit 1
fi

install -m 0755 "$BIN_SRC" "$APPDIR/usr/bin/soulbeet-desktop"
install -m 0644 "$REPO_ROOT/desktop/packaging/org.soulbeet.Desktop.desktop" \
    "$APPDIR/usr/share/applications/org.soulbeet.Desktop.desktop"
cp "$REPO_ROOT/desktop/packaging/org.soulbeet.Desktop.desktop" "$APPDIR/org.soulbeet.Desktop.desktop"

cat > "$APPDIR/AppRun" <<'EOF'
#!/usr/bin/env bash
HERE="$(dirname "$(readlink -f "${0}")")"
exec "$HERE/usr/bin/soulbeet-desktop" "$@"
EOF
chmod +x "$APPDIR/AppRun"

if command -v appimagetool >/dev/null 2>&1; then
    OUT="$REPO_ROOT/target/Soulbeet-x86_64.AppImage"
    appimagetool "$APPDIR" "$OUT"
    echo "==> Created AppImage: $OUT"
else
    echo "==> Prepared AppDir at: $APPDIR (install appimagetool to pack into .AppImage)"
fi
