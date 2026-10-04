#!/bin/bash
# Does pleamar-wm keep anything it should not? A headless session opens and
# closes windows for a while (churn.sh) and this samples, every 2 s, its
# memory, threads, open files and children; then says whether any of them
# kept growing. Run it before and after a change.
#
#   tools/soak/soak.sh [OUT]       ROUNDS=40 KINDS="kitty wl x11" WM=… SCENE=…
#
# OUT gets samples.txt (one line every 2 s) and wm.log (with PLEAMAR_TIMING's
# «kept:» lines: what each part holds, which should not only grow).
here=$(dirname "$(readlink -f "$0")")
repo=$(readlink -f "$here/../..")
out=$(readlink -f "${1:-$repo/target/soak}")
mkdir -p "$out"
wm=${WM:-$repo/target/release/pleamar-wm}
scene=${SCENE:-$repo/examples/session.plm}
rounds=${ROUNDS:-40}
wait=${WAIT:-20}
total=$((wait + rounds * 3 + 40))
[ -x "$wm" ] || { echo "no $wm: cargo build --release first, or WM=…"; exit 1; }
echo "$here/churn.sh" > "$out/autostart"
run() {
  unset PLEAMAR_SOCKETS
  export WAIT=$wait ROUNDS=$rounds KINDS=${KINDS:-kitty wl x11}
  PLEAMAR_TIMING=1 PLEAMAR_HEADLESS_AT=999999 PLEAMAR_HEADLESS_PNG="$out/shot.png" PLEAMAR_WM_AUTOSTART="$out/autostart" PLEAMAR_HEADLESS_SCREENS=1 \
    "$wm" headless "$scene" --seconds "$total" > "$out/wm.log" 2>&1 &
  p=$!
  echo "t rss_kb anon_kb threads fds children zombies" > "$out/samples.txt"
  t=0
  while kill -0 $p 2>/dev/null; do
    rss=$(awk '/VmRSS/{print $2}' /proc/$p/status 2>/dev/null)
    anon=$(awk '/^Anonymous:/{print $2}' /proc/$p/smaps_rollup 2>/dev/null)
    th=$(awk '/Threads/{print $2}' /proc/$p/status 2>/dev/null)
    fds=$(ls /proc/$p/fd 2>/dev/null | wc -l)
    ch=$(ps --ppid $p -o pid= | wc -l)
    z=$(ps --ppid $p -o stat= | grep -c Z)
    [ -n "$rss" ] && echo "$t $rss $anon $th $fds $ch $z" >> "$out/samples.txt"
    sleep 2; t=$((t + 2))
    [ $t -gt $((total + 30)) ] && kill $p
  done
}
echo "soak · $rounds rounds of ${KINDS:-kitty wl x11}, about $total s · $out"
# A bus of its own: the session tells dbus where it is, and that is not this desktop's.
export -f run; export out wm scene total wait rounds
dbus-run-session -- bash -c run
# Before the churn (once started) against after it (once it is quiet).
awk -v w="$wait" -v e="$((wait + rounds * 3 + 10))" '
  NR == 1 { next }
  $1 >= w - 4 && $1 <= w && !b { b = 1; r0 = $2; a0 = $3; t0 = $4; f0 = $5 }
  $1 >= e { r1 = $2; a1 = $3; t1 = $4; f1 = $5; z = $7 }
  END {
    if (!b || !r1) { print "soak · not enough samples: see samples.txt"; exit 1 }
    printf "soak · memory %d → %d MB (anonymous %d → %d) · threads %d → %d · open files %d → %d · zombies %d\n", r0/1024, r1/1024, a0/1024, a1/1024, t0, t1, f0, f1, z
    grow = (a1 - a0) / 1024
    if (grow > 64 || t1 > t0 || f1 > f0 + 4 || z > 0) print "soak · something stayed: compare the «kept:» lines in wm.log"
    else print "soak · nothing kept growing"
  }' "$out/samples.txt"
