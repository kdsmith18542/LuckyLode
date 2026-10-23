#!/usr/bin/env bash
# Install the Lucky Lode Wallet as a normal desktop app (click-to-run).
#
# Takes the AppImage produced by `cargo tauri build`, unpacks it into
# ~/Applications/LuckyLodeWallet (no FUSE needed), and registers a
# .desktop entry so it launches from the app menu / panel like a normal
# installed app.
set -euo pipefail

BIN_DIR="$(cd "$(dirname "$0")" && pwd)"
INSTALL_DIR="$HOME/Applications/LuckyLodeWallet"

# Newest AppImage under either layout (repo root or src-tauri as CWD).
APPIMAGE=""
for f in "$BIN_DIR"/target/release/bundle/appimage/*.AppImage \
         "$BIN_DIR"/src-tauri/target/release/bundle/appimage/*.AppImage; do
    [ -f "$f" ] || continue
    if [ -z "$APPIMAGE" ] || [ "$f" -nt "$APPIMAGE" ]; then
        APPIMAGE="$f"
    fi
done

if [ -z "$APPIMAGE" ]; then
    echo "no AppImage found — run: cd $BIN_DIR/src-tauri && cargo tauri build" >&2
    exit 1
fi

echo "using $APPIMAGE"
mkdir -p "$INSTALL_DIR"
# Extract (AppImages are self-mounting squashfs; --appimage-extract avoids
# FUSE, which some distros don't ship by default).
rm -rf /tmp/lw-extract && mkdir -p /tmp/lw-extract
(cd /tmp/lw-extract && "$APPIMAGE" --appimage-extract >/dev/null 2>&1 || true)

if [ -x /tmp/lw-extract/squashfs-root/usr/bin/lucky-lode-wallet ]; then
    cp /tmp/lw-extract/squashfs-root/usr/bin/lucky-lode-wallet "$INSTALL_DIR/lucky-lode-wallet"
    cp /tmp/lw-extract/squashfs-root/usr/share/icons/hicolor/256x256/apps/lucky-lode-wallet.png \
        "$INSTALL_DIR/256x256.png" 2>/dev/null || true
    cp /tmp/lw-extract/squashfs-root/*.png "$INSTALL_DIR/" 2>/dev/null || true
elif [ -x /tmp/lw-extract/squashfs-root/lucky-lode-wallet ]; then
    cp /tmp/lw-extract/squashfs-root/lucky-lode-wallet "$INSTALL_DIR/lucky-lode-wallet"
    cp /tmp/lw-extract/squashfs-root/*.png "$INSTALL_DIR/" 2>/dev/null || true
else
    # Fallback: launcher that runs the AppImage directly.
    cat > "$INSTALL_DIR/lucky-lode-wallet" <<EOF
#!/usr/bin/env bash
exec "$APPIMAGE" "\$@"
EOF
fi
chmod +x "$INSTALL_DIR/lucky-lode-wallet"

ICON="$INSTALL_DIR/256x256.png"
[ -f "$ICON" ] || ICON="$INSTALL_DIR/128x128.png"
[ -f "$ICON" ] || ICON="utilities-terminal"

mkdir -p "$HOME/.local/share/applications"
cat > "$HOME/.local/share/applications/luckylode-wallet.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Lucky Lode Wallet
Comment=Lucky Lode wallet: self-custody keys, mining, luckynodes and rewards
Exec=$INSTALL_DIR/lucky-lode-wallet
Icon=$ICON
Terminal=false
Categories=Finance;Network;
StartupWMClass=lucky-lode-wallet
EOF
update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true

echo
echo "installed:"
echo "  app:        $INSTALL_DIR/lucky-lode-wallet   (double-clickable)"
echo "  menu entry: ~/.local/share/applications/luckylode-wallet.desktop"
echo
echo "launch it from the app menu (search 'Lucky Lode') or run:"
echo "  $INSTALL_DIR/lucky-lode-wallet"
