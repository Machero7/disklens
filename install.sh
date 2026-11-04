#!/usr/bin/env bash
# Build DiskLens (Rust) and install it for the current user (no sudo needed).
#
#   ./install.sh              build, install, and retire old DiskLens launchers
#   ./install.sh --uninstall  remove it again and put the old launchers back
#
# Installs:  ~/.local/bin/disklens
#            ~/.local/share/applications/com.familjenhero.DiskLens.desktop  (the menu entry)
#            ~/.local/share/icons/hicolor/scalable/apps/com.familjenhero.DiskLens.svg
# Old launchers (the Python "DiskLens" and "DiskLens Preview") are moved, not deleted, to
#            ~/.local/share/disklens-old-launchers/
set -euo pipefail
cd "$(dirname "$0")"

APP_ID=com.familjenhero.DiskLens
BIN_DIR="$HOME/.local/bin"
APPS="$HOME/.local/share/applications"
ICONS="$HOME/.local/share/icons/hicolor"
BACKUP="$HOME/.local/share/disklens-old-launchers"

refresh_menus() {
    update-desktop-database "$APPS" 2>/dev/null || true
    gtk-update-icon-cache -q -t "$ICONS" 2>/dev/null || true
}

if [ "${1:-}" = "--uninstall" ]; then
    rm -fv "$BIN_DIR/disklens" "$APPS/$APP_ID.desktop" "$ICONS/scalable/apps/$APP_ID.svg"
    if [ -d "$BACKUP" ]; then
        for f in "$BACKUP"/*.desktop; do
            [ -e "$f" ] || continue
            mv -v "$f" "$APPS/"
        done
        rmdir "$BACKUP" 2>/dev/null || true
    fi
    refresh_menus
    echo "DiskLens (Rust) removed; old launchers restored."
    exit 0
fi

echo "1/4  Building the release version (a few minutes the first time)…"
cargo build --release -p disklens

echo "2/4  Installing the program to $BIN_DIR/disklens"
install -Dm755 target/release/disklens "$BIN_DIR/disklens"

echo "3/4  Installing the icon and the menu launcher"
install -Dm644 "data/$APP_ID.svg" "$ICONS/scalable/apps/$APP_ID.svg"
mkdir -p "$APPS"
sed "s|@BIN@|$BIN_DIR/disklens|" "data/$APP_ID.desktop.in" > "$APPS/$APP_ID.desktop"
chmod 644 "$APPS/$APP_ID.desktop"

echo "4/4  Retiring old DiskLens launchers"
moved=0
for f in "$APPS"/*.desktop; do
    [ -e "$f" ] || continue
    [ "$(basename "$f")" = "$APP_ID.desktop" ] && continue
    if grep -qiE '^(Name|Exec)=.*disklens' "$f"; then
        mkdir -p "$BACKUP"
        mv "$f" "$BACKUP/"
        echo "     moved $(basename "$f") to $BACKUP"
        moved=1
    fi
done
[ "$moved" = 1 ] || echo "     none found"
refresh_menus

echo
echo "Done. Open DiskLens from the app grid, or run: disklens [folder]"
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "Note: $BIN_DIR isn't on your PATH, so 'disklens' in a terminal needs the full path." ;;
esac
