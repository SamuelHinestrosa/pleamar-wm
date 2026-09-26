#!/bin/sh
# pleamar-wm as a session of its own. From a TTY of its own (Ctrl+Alt+F3, log
# in there), not from inside another desktop:
#
#   ./session.sh                        the demo window manager
#   ./session.sh --seconds 45           ...and it leaves by itself after 45 s
#   ./session.sh other.plm [options]    another scene
#
# Ctrl+Alt+Backspace leaves; Ctrl+Alt+F1…F12 go to another TTY. What it does
# goes to its log, which is shown when it ends.
here=$(dirname "$(readlink -f "$0")")
dir_state="${XDG_STATE_HOME:-$HOME/.local/state}/pleamar-wm"
log="$dir_state/session.log"
mkdir -p "$(dirname "$log")"
scene="$here/examples/session.plm"
case "$1" in
    *.plm) scene="$1"; shift ;;
esac
[ -f "$log" ] && mv -f "$log" "$log.1"
# The monitors left to right, as Hyprland had them the last time it said so
# (or as PLEAMAR_MONITORS already says).
order="$dir_state/monitors"
if [ -z "$PLEAMAR_MONITORS" ] && [ -f "$order" ]; then
    PLEAMAR_MONITORS=$(cat "$order")
    export PLEAMAR_MONITORS
fi
echo "pleamar-wm · its log: $log"
"$here/target/release/pleamar-wm" session "$scene" "$@" > "$log" 2>&1
status=$?
echo "pleamar-wm · left (exit $status). The end of its log:"
tail -n 20 "$log"
