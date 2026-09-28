#!/bin/sh
# Puts pleamar-wm in the login screen's list of sessions (SDDM, GDM…):
#
#   sudo ./install.sh            installs
#   sudo ./install.sh remove     takes it out again
#
# The session runs this repo's build (target/release/pleamar-wm) through
# session.sh, as from a TTY: its log is ~/.local/state/pleamar-wm/session.log.
set -e
here=$(dirname "$(readlink -f "$0")")
[ "$(id -u)" = 0 ] || { echo "run it with sudo: sudo $0 $*"; exit 1; }
if [ "$1" = remove ]; then
    rm -f /usr/local/bin/pleamar-wm-session /usr/share/wayland-sessions/pleamar-wm.desktop /usr/share/xdg-desktop-portal/pleamar-portals.conf /usr/share/xdg-desktop-portal/portals/pleamar.portal
    echo "pleamar-wm · out of the login screen"
    exit 0
fi
[ -x "$here/target/release/pleamar-wm" ] || { echo "build it first: cargo build --release"; exit 1; }
cat > /usr/local/bin/pleamar-wm-session <<SCRIPT
#!/bin/sh
# pleamar-wm's session, as the login screen starts it (see $here/install.sh).
export XDG_CURRENT_DESKTOP=pleamar XDG_SESSION_DESKTOP=pleamar XDG_SESSION_TYPE=wayland
# The only desktop running: the programs started from dbus or systemd (the
# portals, notifications) are told where it is.
export PLEAMAR_WM_EXPORT=1
exec "$here/session.sh" "\$@"
SCRIPT
chmod 755 /usr/local/bin/pleamar-wm-session
install -Dm644 "$here/pleamar-wm.desktop" /usr/share/wayland-sessions/pleamar-wm.desktop
install -Dm644 "$here/pleamar-portals.conf" /usr/share/xdg-desktop-portal/pleamar-portals.conf
# Its own portal (sharing the screen), answered by pleamar-wm itself.
install -Dm644 "$here/pleamar.portal" /usr/share/xdg-desktop-portal/portals/pleamar.portal
echo "pleamar-wm · in the login screen's list: log out and choose «pleamar-wm»"
