#!/bin/bash
# Run inside the session (its autostart): opens and closes windows, round
# after round. KINDS: any of kitty, wl (GTK on Wayland), x11 (GTK on X11).
here=$(dirname "$(readlink -f "$0")")
sleep "${WAIT:-20}"
for i in $(seq 1 "${ROUNDS:-40}"); do
  case " ${KINDS:-kitty wl x11} " in *" kitty "*) command -v kitty >/dev/null && kitty --class soak -e sh -c "echo round $i; sleep 2" & ;; esac
  case " ${KINDS:-kitty wl x11} " in *" wl "*) GDK_BACKEND=wayland python3 "$here/win.py" "w$i" 2 >/dev/null 2>&1 & ;; esac
  case " ${KINDS:-kitty wl x11} " in *" x11 "*) GDK_BACKEND=x11 python3 "$here/win.py" "x$i" 2 >/dev/null 2>&1 & ;; esac
  sleep 3
done
