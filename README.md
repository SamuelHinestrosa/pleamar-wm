# pleamar-wm

A window manager written as a [pleamar](https://github.com/k4ditano/pleamar)
scene. pleamar-wm is pleamar with a Wayland compositor inside: a scene that
says `windows win max 6` holds other programs' windows, and where they go, how
they arrive and how they leave is the scene's —springs, rules and zones—. Save
the scene while programs are open in it and the new layout takes them where it
says, without closing anything.

```sh
# next to a clone of pleamar: ../pleamar
cargo build --release
./target/release/pleamar-wm --scene examples/windows.plm
```

| | |
| --- | --- |
| Alt+Return | a terminal |
| Alt+q | close the window with the keyboard |
| Alt+m | make it the leader |
| Alt+j / Alt+k | the keyboard to the next / previous |
| Alt+h / Alt+l | the leader narrower / wider |
| Alt+o | everything at a glance |

## A session of its own

Without a compositor underneath: pleamar-wm takes the monitors and the input
through the seat (libseat, via logind; no root) and paints each monitor
straight into buffers of the card that go to the screen with page flips. From
a TTY of its own —Ctrl+Alt+F3 and log in there, not from inside a desktop—:

```sh
./session.sh --seconds 45   # the first time: it leaves by itself after 45 s
./session.sh                # the demo window manager, until Ctrl+Alt+Backspace
```

Every surface of the scene is shown: its own —one copy per monitor with
`screens: each`— and the named ones, a bar or a corner, each painted in frames
of its own and put together on the monitor by level and anchor, as
layer-shell would; the pointer goes to the highest one with a zone under it.
Marea, whole, runs in it.

Ctrl+Alt+Backspace leaves; Ctrl+Alt+F1…F12 go to another TTY and back. Its
log goes to `~/.local/state/pleamar-wm/session.log`, and the end of it is shown
when it leaves. `pleamar-wm probe` tries what the card needs for it —buffers
for the screen, painted by wgpu and read back— without taking the screen.

The language side —`windows`, `window`, `launch`, `focus`, `close`,
`promote`— is pleamar's and is documented in its reference (§10.3). This repo
is the compositor that fills it: the protocol side of
[Smithay](https://github.com/Smithay/smithay), with pleamar doing the painting.

## Where it is

It runs nested, as a window of your current compositor. Programs hand over
their frames on the card (linux-dmabuf, single plane, ARGB/XRGB with the
card's own modifiers) or in shared memory; each surface —the window, its
subsurfaces, its menus— is a piece of its own that pleamar draws in place. A
frame on the card is only taken once the program has finished drawing it, and
handed back once copied. Menus stay inside their window; the frames are the
scene's (server-side decorations); cursor shapes and a clipboard between its
windows.

Measured, a terminal redrawing 50 times a second:

| | terminal | pleamar(-wm) | Hyprland |
| --- | --- | --- | --- |
| straight on Hyprland | 3.5 % | — | 14.6 % |
| inside, with software GL | 192 % | 15.6 % | 10.9 % |
| inside, frames on the card | 5.4 % | 13.1 % | 16.8 % |
| inside, one round per frame | 5.4 % | 10.9 % | 16.5 % |

In the last row the terminal draws 89 frames a second, and pleamar-wm spends
about 1.2 ms of CPU on each (1.4 ms before): 0.6 of it painting, the rest
composing the scene and copying the frame on the card.

Not yet: painting only what changed, explicit sync, multi-plane buffers, menus
beyond their window, XWayland, more than one scale, a session of its own on
DRM/libinput, layer-shell, screencopy and the portals.
