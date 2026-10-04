#!/usr/bin/env python3
"""The agent's cursors, seen moving: a computer-use agent's hands, driven by hand.

Run from a terminal of a pleamar-wm session with `agent on` in session.conf.
It speaks `cua-inject v1` to the session's socket (what Cua Driver speaks)
and, on the window of the program you name (default: the newest kitty),
draws two loops with the agent's two cursors and types a line into it —
without taking your mouse or your keyboard.

    tools/agent/demo.py [PROGRAM] [--type "text"]
"""
import math, os, socket, subprocess, sys, time

path = os.environ.get("CUA_INJECT_SOCKET")
if not path:
    sys.exit("no CUA_INJECT_SOCKET: is this a pleamar-wm session with `agent on` in session.conf?")
args = sys.argv[1:]
text = "hello from the agent's keyboard"
if "--type" in args:
    k = args.index("--type")
    text = args[k + 1]
    del args[k:k + 2]
program = args[0] if args else "kitty"
pids = subprocess.run(["pgrep", "-n", program], capture_output=True, text=True).stdout.split()
if not pids:
    sys.exit(f"no '{program}' running")
pid = pids[0]

s = socket.socket(socket.AF_UNIX)
s.connect(path)
f = s.makefile("rw")


def say(line):
    f.write(line + "\n")
    f.flush()
    reply = f.readline().strip()
    if reply != "ok" and not reply.startswith(("state", "geometry", "cua-inject")):
        print(line, "->", reply)
    return reply


say("cua-inject v1")
target = f"root:{pid}"
print(say(f"q {pid}"))
print(say(f"g {pid}"))
# Two cursors, two loops: a circle and a figure of eight.
for i in range(240):
    a = i / 120 * math.pi
    say(f"m {target} 0 {320 + 160 * math.cos(a):.1f} {260 + 120 * math.sin(a):.1f}")
    say(f"m {target} 1 {520 + 150 * math.sin(a):.1f} {300 + 70 * math.sin(2 * a):.1f}")
    time.sleep(0.016)
say(f"b {target} 0 272 1")
say(f"b {target} 0 272 0")
say(f"t {target} {text.encode('ascii', 'replace').hex()}")
