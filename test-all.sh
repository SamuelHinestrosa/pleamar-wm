#!/bin/sh
# Every check of pleamar-wm's own session, one after another, by itself.
# From a TTY of its own (Ctrl+Alt+F3, log in there):
#
#   ~/Proyectos/pleamar-wm/test-all.sh
#
# and wait about two minutes. Three rounds:
#   1. the measurement: a terminal redrawing by itself, 40 s;
#   2. the whole desktop on both monitors —wallpaper, Marea, the window
#      manager—, 45 s; play with it if you like: the buttons above,
#      Super+T, Super+Shift+arrows, dragging a title bar, Super+Space, Marea;
#   3. the same desktop by itself: 35 s, Marea's card opens alone.
# The Marea running on Hyprland is stopped for rounds 2 and 3 (they start
# their own) and started again on Hyprland at the end. Everything is written to
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

# Rounds 2 and 3 start their own Marea inside the session (autostart): the
# one running on Hyprland is stopped first and started again at the end.
was_running=no
if wants 2 || wants 3; then
    if "$pleamar" --say marea "get open" > /dev/null 2>&1; then
        was_running=yes
        "$pleamar" --say marea quit > /dev/null 2>&1
        sleep 1.5
    fi
fi
desktop_report() {
    grep -E "session · (monitor|monitors|the surface)|windows · (starting|'.*' on)" "$state/session.log"
    echo "frames lost to a busy monitor: $(grep -c 'resource busy' "$state/session.log")"
    echo "frames on the card that could not be read: $(grep -c 'could not be read' "$state/session.log")"
    grep -iE "panicked|error|could not" "$state/session.log" | grep -viE "adwaita|libenchant|glfw|gdk" | head -8
    marea_log="${XDG_STATE_HOME:-$HOME/.local/state}/marea-plm/marea.log"
    if ! grep -q "windows · starting: .*marea" "$state/session.log"; then
        echo "Marea was NOT started inside the session"
    elif [ -f "$marea_log" ] && [ "$marea_log" -nt "$state/session.log.1" ]; then
        echo "Marea, inside:"
        grep -E "^render · surface|panicked|error" "$marea_log" | head -12
    fi
}

# ── 2 · the desktop ─────────────────────────────────────────────
if wants 2; then
say "2/3 · the whole desktop (45 s): wallpaper, Marea and the window manager. Try Super+T, dragging a title bar, Super+Shift+arrows, Super+Space, Marea's card"
countdown 5
"$here/session.sh" --seconds 45 > /dev/null 2>&1
{
    echo
    echo "── 2 · desktop"
    desktop_report
} >> "$report"
cp -f "$state/session.log" "$state/test-all-desktop.log"
fi

# ── 3 · the desktop by itself ───────────────────────────────────
if wants 3; then
say "3/3 · the desktop by itself (35 s): Marea's card and settings open alone"
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
"$here/session.sh" --seconds 35 > /dev/null 2>&1
wait "$opener" 2> /dev/null
{
    echo
    echo "── 3 · Marea inside"
    desktop_report
} >> "$report"
cp -f "$state/session.log" "$state/test-all-marea.log"
[ -f "${XDG_STATE_HOME:-$HOME/.local/state}/marea-plm/marea.log" ] && cp -f "${XDG_STATE_HOME:-$HOME/.local/state}/marea-plm/marea.log" "$state/test-all-marea-inside.log"
fi

# Marea back on Hyprland, as she was. First the one that ran inside the
# session has to be gone: while she lives she holds the notifications, and the
# new one would find them taken and show her made-up examples instead.
if [ "$was_running" = yes ]; then
    n=0
    while pgrep -f "pleamar --scene marea.plm" > /dev/null && [ "$n" -lt 50 ]; do sleep 0.1; n=$((n + 1)); done
    pkill -f "pleamar --scene marea.plm" 2> /dev/null && sleep 1
    runtime="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
    display=$(ls "$runtime" 2> /dev/null | grep -E '^wayland-[0-9]+$' | head -1)
    signature=$(ls -t "$runtime/hypr" 2> /dev/null | head -1)
    if [ -n "$display" ]; then
        (cd "$marea" && WAYLAND_DISPLAY="$display" HYPRLAND_INSTANCE_SIGNATURE="$signature" setsid nohup ./marea start > /dev/null 2>&1 &)
        echo "Marea started again on Hyprland ($display)" >> "$report"
    fi
fi

say "done: back to Hyprland with Ctrl+Alt+F1"
cat "$report"
