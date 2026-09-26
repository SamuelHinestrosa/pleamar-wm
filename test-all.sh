#!/bin/sh
# Every check of pleamar-wm's own session, one after another, by itself.
# From a TTY of its own (Ctrl+Alt+F3, log in there):
#
#   ~/Proyectos/pleamar-wm/test-all.sh
#
# and wait about two minutes. Three rounds:
#   1. the measurement: a terminal redrawing by itself, 40 s;
#   2. the window manager on both monitors, 45 s — play with it if you like:
#      the buttons above, Alt+Return, Alt+s (to the other monitor), Alt+o;
#   3. Marea on her own, with no Hyprland: 35 s, her card opens by itself.
# The Marea running on Hyprland is stopped for round 3 and started again on
# Hyprland at the end. Everything is written to
# ~/.local/state/pleamar-wm/test-all.txt, which is shown when it ends.
# Ctrl+Alt+Backspace ends a round early. `test-all.sh 3` runs only round 3;
# `test-all.sh 1 3`, rounds 1 and 3.

here=$(dirname "$(readlink -f "$0")")
state="${XDG_STATE_HOME:-$HOME/.local/state}/pleamar-wm"
mkdir -p "$state"
report="$state/test-all.txt"
pleamar="$HOME/Proyectos/pleamar/target/release/pleamar"
marea="$HOME/Proyectos/marea-plm"
[ -z "$PLEAMAR_MONITORS" ] && [ -f "$state/monitors" ] && PLEAMAR_MONITORS=$(cat "$state/monitors") && export PLEAMAR_MONITORS

rounds="${*:-1 2 3}"
wants() { case " $rounds " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
say() { printf '\n\033[1;36m== %s\033[0m\n' "$*"; }
countdown() {
    n=$1
    while [ "$n" -gt 0 ]; do printf '\r   starts in %s… ' "$n"; sleep 1; n=$((n - 1)); done
    printf '\r                    \r'
}

{
    echo "pleamar-wm · test-all · $(date '+%F %T')"
    echo "monitors left to right: ${PLEAMAR_MONITORS:-as the card lists them}"
} > "$report"

# ── 1 · the measurement ─────────────────────────────────────────
if wants 1; then
say "1/3 · measuring: a terminal opens by itself; do not touch anything (40 s)"
countdown 3
"$here/measure.sh" > /dev/null 2>&1
{
    echo
    echo "── 1 · measurement"
    cat "$state/measure.txt"
    echo "frames lost to a busy monitor: $(grep -c 'resource busy' "$state/measure.log")"
} >> "$report"
fi

# ── 2 · the window manager ──────────────────────────────────────
if wants 2; then
say "2/3 · the window manager on both monitors (45 s): try the buttons above, Alt+Return, Alt+s, Alt+o"
countdown 5
"$here/session.sh" --seconds 45 > /dev/null 2>&1
{
    echo
    echo "── 2 · window manager"
    grep -E "session · (monitor|monitors|the surface)" "$state/session.log"
    echo "frames lost to a busy monitor: $(grep -c 'resource busy' "$state/session.log")"
    echo "frames on the card that could not be read: $(grep -c 'could not be read' "$state/session.log")"
    grep -iE "panicked|error" "$state/session.log" | grep -viE "adwaita|libenchant|glfw|gdk" | head -5
} >> "$report"
cp -f "$state/session.log" "$state/test-all-windows.log"
fi

# ── 3 · Marea on her own ────────────────────────────────────────
if wants 3; then
say "3/3 · Marea with no Hyprland (35 s): her card opens by itself"
was_running=no
if "$pleamar" --say marea "get open" > /dev/null 2>&1; then
    was_running=yes
    "$pleamar" --say marea quit > /dev/null 2>&1
    sleep 1.5
fi
countdown 3
(
    sleep 6
    "$pleamar" --say marea "fact open true" > /dev/null 2>&1
    sleep 8
    "$pleamar" --say marea "fact page settings" > /dev/null 2>&1
    sleep 6
    "$pleamar" --say marea "fact page none" > /dev/null 2>&1
    "$pleamar" --say marea "fact open false" > /dev/null 2>&1
) &
opener=$!
"$here/session.sh" "$marea/marea.plm" --seconds 35 > /dev/null 2>&1
wait "$opener" 2> /dev/null
{
    echo
    echo "── 3 · Marea"
    grep -E "session · (monitor|monitors|the surface)" "$state/session.log"
    echo "frames lost to a busy monitor: $(grep -c 'resource busy' "$state/session.log")"
    grep -iE "panicked|error|could not" "$state/session.log" | grep -viE "adwaita|libenchant|glfw|gdk" | head -8
} >> "$report"
cp -f "$state/session.log" "$state/test-all-marea.log"

# Marea back on Hyprland, as she was.
if [ "$was_running" = yes ]; then
    runtime="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
    display=$(ls "$runtime" 2> /dev/null | grep -E '^wayland-[0-9]+$' | head -1)
    signature=$(ls -t "$runtime/hypr" 2> /dev/null | head -1)
    if [ -n "$display" ]; then
        (cd "$marea" && WAYLAND_DISPLAY="$display" HYPRLAND_INSTANCE_SIGNATURE="$signature" setsid nohup ./marea start > /dev/null 2>&1 &)
        echo "Marea started again on Hyprland ($display)" >> "$report"
    fi
fi
fi

say "done: back to Hyprland with Ctrl+Alt+F1"
cat "$report"
