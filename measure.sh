#!/bin/sh
# What pleamar-wm costs as a session of its own. From a TTY of its own
# (Ctrl+Alt+F3, logged in there): ./measure.sh — and wait ~40 s. A terminal
# opens by itself and redraws 50 times a second; for 15 s the CPU of
# pleamar-wm and of the terminal is measured; then it leaves by itself. The
# result goes to ~/.local/state/pleamar-wm/measure.txt.
here=$(dirname "$(readlink -f "$0")")
dir="${XDG_STATE_HOME:-$HOME/.local/state}/pleamar-wm"
mkdir -p "$dir"
out="$dir/measure.txt"
log="$dir/measure.log"
echo "pleamar-wm · measuring for ~40 s; it leaves by itself"
PLEAMAR_TIMING=1 "$here/target/release/pleamar-wm" session "$here/examples/measure.plm" --seconds 38 > "$log" 2>&1 &
sleep 14
ticks=$(getconf CLK_TCK)
wm=$(pgrep -x pleamar-wm | head -1)
kitty=$(pgrep -f "class pleamar-measure" | head -1)
cpu() { [ -n "$1" ] && [ -r "/proc/$1/stat" ] && awk '{print $14 + $15}' "/proc/$1/stat" || echo 0; }
a1=$(cpu "$wm"); b1=$(cpu "$kitty")
threads() { for t in /proc/"$wm"/task/*; do echo "$(basename "$t") $(tr ' ' _ < "$t/comm") $(awk '{print $14 + $15}' "$t/stat")"; done | sort; }
threads > "$dir/threads.0"
sleep 15
threads > "$dir/threads.1"
a2=$(cpu "$wm"); b2=$(cpu "$kitty")
{
    echo "measured $(date '+%F %T'), 15 s, % of one core"
    echo "pleamar-wm: $(awk -v a="$a1" -v b="$a2" -v t="$ticks" 'BEGIN { printf "%.1f", (b - a) * 100 / t / 15 }') %"
    echo "terminal:   $(awk -v a="$b1" -v b="$b2" -v t="$ticks" 'BEGIN { printf "%.1f", (b - a) * 100 / t / 15 }') %"
    echo "by thread:"
    join "$dir/threads.0" "$dir/threads.1" | awk -v t="$ticks" '{ d = $5 - $3; if (d > 0) printf "  %-22s %5.1f %%\n", $2, d * 100 / t / 15 }' | sort -k2 -rn
    echo "where each round's time goes:"
    grep "timing ·" "$log" | tail -4
} > "$out"
wait
cat "$out"
