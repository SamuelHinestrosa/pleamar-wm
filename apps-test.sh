#!/bin/sh
# Real programs in pleamar-wm, one at a time, with no screen (headless): does
# each one get a window, of what size, and does it draw something in it?
#
#   ./apps-test.sh              every program on the list that is installed
#   ./apps-test.sh kitty vkcube only those
#
# Each one runs alone for a few seconds in the session's own scene. What it
# showed is compared with the desktop with nothing open: a window that stays
# black or empty is caught. Report and pictures in ~/.local/state/pleamar-wm/apps/.
here=$(dirname "$(readlink -f "$0")")
out="${XDG_STATE_HOME:-$HOME/.local/state}/pleamar-wm/apps"
mkdir -p "$out"
wm="$here/target/release/pleamar-wm"
say="$HOME/Proyectos/pleamar/target/release/pleamar"
profile=$(mktemp -d)
trap 'rm -rf "$profile"' EXIT

# name | seconds to wait | command
list="kitty|8|kitty
alacritty|8|alacritty
gnome-calculator|8|gnome-calculator
gnome-text-editor|10|gnome-text-editor
pavucontrol|8|pavucontrol
zenity|8|zenity --info --text=pleamar-wm
dolphin|12|dolphin
firefox|18|firefox --new-instance --profile $profile about:blank
vkcube|8|vkcube
glxgears (X11)|8|glxgears
calculator on X11|8|env GDK_BACKEND=x11 gnome-calculator"

# Runs the session with that autostart, and after `wait` seconds asks the
# scene about its first window and takes the picture.
run() {
    name=$1; wait=$2; cmd=$3; png=$4
    printf '%s\n' "$cmd" > "$out/autostart"
    PLEAMAR_WM_AUTOSTART="$out/autostart" PLEAMAR_HEADLESS_SCREENS=1 PLEAMAR_HEADLESS_AT=$((wait - 1)) PLEAMAR_HEADLESS_PNG="$png" \
        timeout $((wait + 6)) "$wm" headless "$here/examples/session.plm" --seconds $((wait + 3)) > "$out/log.txt" 2>&1 &
    pid=$!
    sleep "$wait"
    open=$("$say" --say session "get win.0.open" 2> /dev/null)
    dialog=$("$say" --say session "get win.0.dialog" 2> /dev/null)
    title=$("$say" --say session "get win.0.title" 2> /dev/null)
    app=$("$say" --say session "get win.0.app" 2> /dev/null)
    w=$("$say" --say session "get win.0.width" 2> /dev/null)
    h=$("$say" --say session "get win.0.height" 2> /dev/null)
    # (The program leaves on its own when the compositor goes: never killed
    # by name, which would also take the ones open on the real desktop.)
    wait "$pid" 2> /dev/null
    sleep 1
    panics=$(grep -c "panicked" "$out/log.txt")
}

# How much of the picture differs from the empty desktop, in %.
differs() {
    python3 - "$1" "$2" << 'EOF'
import sys
from PIL import Image, ImageChops
a, b = Image.open(sys.argv[1]).convert("RGB"), Image.open(sys.argv[2]).convert("RGB")
d = ImageChops.difference(a, b).convert("L").point(lambda v: 255 if v > 24 else 0)
print(round(100 * sum(d.histogram()[255:]) / (a.width * a.height), 1))
EOF
}

echo "apps · the empty desktop, to compare with"
run "nothing" 8 "true" "$out/nothing.png"
report="$out/report.txt"
{
    echo "pleamar-wm · real programs · $(date '+%F %T')"
    printf '%-20s %-7s %-10s %-26s %-18s %s\n' "program" "window" "size" "title" "app" "drew"
} > "$report"
echo "$list" | while IFS='|' read -r name wait cmd; do
    [ $# -gt 0 ] && ! echo " $* " | grep -q " ${name%% *} " && continue
    bin=$(echo "$cmd" | awk '{ print ($1 == "env") ? $3 : $1 }')
    if ! command -v "$bin" > /dev/null; then
        printf '%-20s not installed\n' "$name" >> "$report"
        continue
    fi
    echo "apps · $name"
    png="$out/$(echo "$name" | tr ' ()' '___').png"
    run "$name" "$wait" "$cmd" "$png"
    drew="-"
    [ -f "$png" ] && drew="$(differs "$out/nothing.png" "$png") %"
    ok=$([ "$open" = true ] && echo yes || echo NO)
    [ "$ok" = yes ] && [ "$dialog" = true ] && ok="dialog"
    [ "$panics" -gt 0 ] && ok="PANIC"
    printf '%-20s %-7s %-10s %-26.26s %-18.18s %s\n' "$name" "$ok" "${w}×${h}" "$title" "$app" "$drew" >> "$report"
done
cat "$report"
echo "(pictures in $out)"
