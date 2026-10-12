#!/bin/sh
# Install TurboRM from a release archive for the current user. No root needed.
#
#   ./install.sh               install into ~/.local (or $PREFIX)
#   ./install.sh --uninstall   remove what this script installed
#
# Installs turborm and turborm-cli into $PREFIX/bin, an application-menu
# entry and the icons. The menu entry names the binary by its absolute path,
# because GNOME Shell's PATH usually lacks ~/.local/bin.
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
prefix=${PREFIX:-$HOME/.local}
bin=$prefix/bin
apps=$prefix/share/applications
icons=$prefix/share/icons/hicolor

refresh() {
    command -v update-desktop-database >/dev/null 2>&1 &&
        update-desktop-database -q "$apps" 2>/dev/null || true
    command -v gtk-update-icon-cache >/dev/null 2>&1 &&
        gtk-update-icon-cache -q -t "$icons" 2>/dev/null || true
}

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$bin/turborm" "$bin/turborm-cli" "$apps/turborm.desktop" \
        "$icons/scalable/apps/turborm.svg" "$icons/256x256/apps/turborm.png" \
        "$icons/24x24/apps/turborm.png" "$icons/22x22/apps/turborm.png" \
        "$icons/16x16/apps/turborm.png"
    refresh
    echo "TurboRM removed. Your pairing stays in the keyring and the cache in ~/.cache/remarkable-send."
    exit 0
fi
[ $# -eq 0 ] || { echo "Usage: $0 [--uninstall]" >&2; exit 2; }

install -Dm755 "$here/turborm" "$bin/turborm"
install -Dm755 "$here/turborm-cli" "$bin/turborm-cli"
install -Dm644 "$here/icons/turborm.svg" "$icons/scalable/apps/turborm.svg"
install -Dm644 "$here/icons/turborm.png" "$icons/256x256/apps/turborm.png"
for size in 16 22 24; do
    install -Dm644 "$here/icons/turborm-$size.png" "$icons/${size}x$size/apps/turborm.png"
done
mkdir -p "$apps"
sed "s|^Exec=.*|Exec=\"$bin/turborm\"|" "$here/turborm.desktop" >"$apps/turborm.desktop"
chmod 644 "$apps/turborm.desktop"
refresh

echo "Installed TurboRM to $bin/turborm. It is in your application menu as TurboRM."
missing=
command -v pdftoppm >/dev/null 2>&1 || missing="$missing poppler-utils"
[ -x /usr/bin/prlimit ] || missing="$missing util-linux"
if [ -n "$missing" ]; then
    echo "PDF pages need:$missing. On Debian/Ubuntu: sudo apt install$missing"
fi
