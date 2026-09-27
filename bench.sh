#!/bin/sh
# The same load on Hyprland and on pleamar-wm's own session, measured the same
# way, to compare them fairly: the whole desktop (wallpaper and Marea) and a
# terminal redrawing 50 times a second, for 20 s after 10 s to settle.
#
#   bench.sh hyprland    from a terminal inside Hyprland (a terminal opens on
#                        the monitor with the focus, and closes by itself)
#   bench.sh session     from a TTY of its own (Ctrl+Alt+F3): pleamar-wm's
#                        session with its autostart (wallpaper and Marea); the
#                        Marea running on Hyprland is stopped meanwhile and
#                        started again at the end
#   bench.sh compare     both results side by side
#
# What is measured: the CPU of the compositor (and each of its threads), of
# the terminal (and its threads), of Marea and of the wallpaper, as % of one
# core; and, if there is nvidia-smi, how busy the card is and what it draws.
# The card is shared with whatever else is open (a browser counts in both).
# Results in ~/.local/state/pleamar-wm/bench-hyprland.txt and bench-session.txt.

here=$(dirname "$(readlink -f "$0")")
state="${XDG_STATE_HOME:-$HOME/.local/state}/pleamar-wm"
mkdir -p "$state"
pleamar="$HOME/Proyectos/pleamar/target/release/pleamar"
marea="$HOME/Proyectos/marea-plm"
ticks=$(getconf CLK_TCK)
settle=10
span=20
load="kitty --class pleamar-bench sh -c 'while :; do date +%T.%N; sleep 0.02; done'"

cpu() { [ -n "$1" ] && [ -r "/proc/$1/stat" ] && sed 's/^.*) //' "/proc/$1/stat" | awk '{print $12 + $13}' || echo 0; }
threads() { [ -n "$1" ] && for t in /proc/"$1"/task/*; do [ -r "$t/stat" ] && echo "$(basename "$t") $(tr ' ' _ < "$t/comm") $(sed 's/^.*) //' "$t/stat" | awk '{print $12 + $13}')"; done | sort; }
pct() { awk -v a="$1" -v b="$2" -v t="$ticks" -v s="$span" 'BEGIN { printf "%.1f", (b - a) * 100 / t / s }'; }

# Measures the processes named, for `span` seconds; writes the result to $1.
measure() {
    out="$1"; env="$2"; comp="$3"; term="$4"; mar="$5"; wall="$6"
    for p in comp term mar wall; do eval "a_$p=\$(cpu \"\$$p\")"; done
    threads "$comp" > "$state/bench.c0"; threads "$term" > "$state/bench.t0"
    gpu=""
    if command -v nvidia-smi > /dev/null; then
        nvidia-smi --query-gpu=utilization.gpu,power.draw --format=csv,noheader,nounits -lms 500 > "$state/bench.gpu" 2> /dev/null &
        gpu=$!
    fi
    # How fast the processor goes meanwhile: a % of a core at 1.5 GHz is not
    # a % at 4 GHz, and with less going on it slows down (schedutil).
    ( i=0; while [ "$i" -lt $((span * 2)) ]; do awk '/cpu MHz/ { s += $4; n++ } END { if (n) printf "%.0f\n", s / n }' /proc/cpuinfo; sleep 0.5; i=$((i + 1)); done ) > "$state/bench.mhz" &
    mhz=$!
    sleep "$span"
    wait "$mhz" 2> /dev/null
    [ -n "$gpu" ] && kill "$gpu" 2> /dev/null
    for p in comp term mar wall; do eval "b_$p=\$(cpu \"\$$p\")"; done
    threads "$comp" > "$state/bench.c1"; threads "$term" > "$state/bench.t1"
    {
        echo "bench · $env · $(date '+%F %T') · ${span} s · % of one core"
        echo "compositor: $(pct "$a_comp" "$b_comp") %"
        echo "terminal:   $(pct "$a_term" "$b_term") %"
        echo "Marea:      $(pct "$a_mar" "$b_mar") %"
        echo "wallpaper:  $(pct "$a_wall" "$b_wall") %"
        echo "sum:        $(awk -v c="$(pct "$a_comp" "$b_comp")" -v t="$(pct "$a_term" "$b_term")" -v m="$(pct "$a_mar" "$b_mar")" -v w="$(pct "$a_wall" "$b_wall")" 'BEGIN { printf "%.1f", c + t + m + w }') %"
        if [ -s "$state/bench.gpu" ]; then
            awk -F', *' '{ u += $1; w += $2; n++ } END { if (n) printf "card:       %.0f %% busy, %.1f W (average of %d samples)\n", u / n, w / n, n }' "$state/bench.gpu"
        fi
        [ -s "$state/bench.mhz" ] && awk '{ s += $1; n++ } END { if (n) printf "processor:  %.0f MHz on average\n", s / n }' "$state/bench.mhz"
        echo "compositor by thread:"
        join "$state/bench.c0" "$state/bench.c1" | awk -v t="$ticks" -v s="$span" '{ d = $5 - $3; if (d > 0) printf "  %-24s %5.1f %%\n", $2, d * 100 / t / s }' | sort -k2 -rn | head -12
        echo "terminal by thread:"
        join "$state/bench.t0" "$state/bench.t1" | awk -v t="$ticks" -v s="$span" '{ d = $5 - $3; if (d > 0) printf "  %-24s %5.1f %%\n", $2, d * 100 / t / s }' | sort -k2 -rn | head -6
    } > "$out"
    rm -f "$state/bench.c0" "$state/bench.c1" "$state/bench.t0" "$state/bench.t1" "$state/bench.gpu" "$state/bench.mhz"
}

# The newest process whose command line says that.
newest() { pgrep -n -f "$1"; }
kitty_pid() { pgrep -n -f "^kitty --class pleamar-bench"; }

case "$1" in
hyprland)
    [ -n "$HYPRLAND_INSTANCE_SIGNATURE" ] || { echo "run it from inside Hyprland"; exit 1; }
    # Marea started again, so that she runs the pleamar built last.
    if "$pleamar" --say marea "get open" > /dev/null 2>&1; then
        echo "bench · Marea starts again (the last pleamar built)"
        "$pleamar" --say marea quit > /dev/null 2>&1
        sleep 1.5
        (cd "$marea" && setsid nohup ./marea start > /dev/null 2>&1 &)
        sleep 5
    fi
    echo "bench · Hyprland: a terminal opens now; ~$((settle + span)) s"
    sh -c "$load" > /dev/null 2>&1 &
    sleep "$settle"
    measure "$state/bench-hyprland.txt" "Hyprland" "$(pgrep -x Hyprland | head -1)" "$(kitty_pid)" "$(newest 'pleamar --scene marea.plm')" "$(pgrep -n -x swaybg)"
    kill "$(kitty_pid)" 2> /dev/null
    cat "$state/bench-hyprland.txt"
    ;;
session)
    [ -z "$WAYLAND_DISPLAY" ] || { echo "run it from a TTY of its own, not from inside a desktop"; exit 1; }
    was_running=no
    if "$pleamar" --say marea "get open" > /dev/null 2>&1; then
        was_running=yes
        "$pleamar" --say marea quit > /dev/null 2>&1
        sleep 1.5
    fi
    # Its usual autostart, and the terminal.
    auto="$state/bench-autostart"
    src="$HOME/.config/pleamar-wm/autostart"
    [ -f "$src" ] || src="$here/autostart"
    { cat "$src"; echo "$load"; } > "$auto"
    w=$(grep -o '"wallpaper" *: *"[^"]*"' "$HOME/.local/share/pleamar/marea/settings.json" 2> /dev/null | sed 's/.*: *"\(.*\)"/\1/')
    [ -n "$w" ] && export PLEAMAR_WALLPAPER="$w"
    [ -z "$PLEAMAR_MONITORS" ] && [ -f "$state/monitors" ] && PLEAMAR_MONITORS=$(cat "$state/monitors") && export PLEAMAR_MONITORS
    echo "bench · pleamar-wm's session: ~$((settle + span + 5)) s, it leaves by itself"
    PLEAMAR_WM_AUTOSTART="$auto" PLEAMAR_TIMING=1 "$here/target/release/pleamar-wm" session "$here/examples/session.plm" --seconds $((settle + span + 4)) > "$state/bench-session.log" 2>&1 &
    sleep "$settle"
    measure "$state/bench-session.txt" "pleamar-wm" "$(pgrep -x pleamar-wm | head -1)" "$(kitty_pid)" "$(newest 'pleamar --scene marea.plm')" "$(pgrep -n -x swaybg)"
    {
        echo "the scene's rounds:"
        grep "timing · .*rounds\|CPU per round" "$state/bench-session.log" | tail -2
        echo "the monitors:"
        for m in $(grep -o "screen · [A-Za-z0-9-]*:" "$state/bench-session.log" | sort -u | cut -d' ' -f3); do grep "screen · $m put together" "$state/bench-session.log" | tail -1; done
    } >> "$state/bench-session.txt"
    wait
    # Marea back on Hyprland, once the one inside has gone.
    if [ "$was_running" = yes ]; then
        n=0
        while pgrep -f "pleamar --scene marea.plm" > /dev/null && [ "$n" -lt 50 ]; do sleep 0.1; n=$((n + 1)); done
        pkill -f "pleamar --scene marea.plm" 2> /dev/null && sleep 1
        runtime="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
        display=$(ls "$runtime" 2> /dev/null | grep -E '^wayland-[0-9]+$' | head -1)
        signature=$(ls -t "$runtime/hypr" 2> /dev/null | head -1)
        [ -n "$display" ] && (cd "$marea" && WAYLAND_DISPLAY="$display" HYPRLAND_INSTANCE_SIGNATURE="$signature" setsid nohup ./marea start > /dev/null 2>&1 &)
    fi
    cat "$state/bench-session.txt"
    ;;
headless)
    # The same, with no screen and only the terminal: to check this script.
    echo "$load" > "$state/bench-autostart"
    PLEAMAR_WM_AUTOSTART="$state/bench-autostart" PLEAMAR_HEADLESS_SCREENS=2 PLEAMAR_HEADLESS_AT=999 PLEAMAR_TIMING=1 "$here/target/release/pleamar-wm" headless "$here/examples/session.plm" --seconds $((settle + span + 4)) > "$state/bench-headless.log" 2>&1 &
    sleep "$settle"
    measure "$state/bench-headless.txt" "pleamar-wm headless" "$(pgrep -x pleamar-wm | head -1)" "$(kitty_pid)" "" ""
    wait
    cat "$state/bench-headless.txt"
    ;;
compare)
    for f in hyprland session; do
        echo "────────────────────────────────"
        cat "$state/bench-$f.txt" 2> /dev/null || echo "(no bench-$f.txt yet: bench.sh $f)"
    done
    ;;
*)
    sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'
    ;;
esac
