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

The language side —`windows`, `window`, `launch`, `focus`, `close`,
`promote`— is pleamar's and is documented in its reference (§10.3). This repo
is the compositor that fills it: the protocol side of
[Smithay](https://github.com/Smithay/smithay), with pleamar doing the painting.

## Where it is

It runs nested, as a window of your current compositor. It takes windows in
shared memory (programs that draw with the GPU are started with Mesa's
software GL), flattens subsurfaces and menus into one image per window, keeps
menus inside their window, draws the frames itself (server-side decorations),
passes on cursor shapes and shares a clipboard between its windows.

Not yet: dmabuf, damage-only uploads, XWayland, more than one scale, a session
of its own on DRM/libinput, layer-shell, screencopy and the portals. Measured
today, a terminal redrawing 50 times a second costs about 2 cores inside it
against 0.2 on Hyprland, almost all of it the software GL: dmabuf is next.
