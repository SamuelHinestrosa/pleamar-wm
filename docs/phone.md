# Your session, in your pocket

`pleamar-wm remote` shows a monitor in a browser elsewhere. From a phone that
is a 1920×1080 desktop squeezed into a hand, with fingers playing mouse. This
note is about the other thing: **taking the session with you**. The same
windows, with whatever was half done in them, become a phone's apps while you
are away; back at the desk, everything is where you left it, plus what you did
from the phone.

Nobody does this. Remote desktops show the computer as it is (GNOME's and
Sunshine's virtual monitors match the client's size, but the desktop stays a
desktop). Phosh and Plasma Mobile are phone shells, not a way into your desk.
DeX turns a phone into a desktop, the other way round. Apple's Continuity
hands over one document at a time. Here the session is one, and it has two
shapes: changing shape is changing scene, which is only possible because the
window manager and the shell are scenes.

## What happens

1. **You open the page on the phone.** It says what it is: its size in pixels,
   its scale, that it is touch. The server asks the session for a **phone
   monitor** of that size (`P W H SCALE` on the agent socket), and the session
   puts one up: a monitor with no screen, painted like the headless ones,
   placed far to the right of the real ones so that no mouse wanders into it.
   The page streams that monitor, not a real one.
2. **The windows come to the phone.** The scene is told which monitor is the
   phone (`phone`), notes where every window was —monitor, pool—, and sends
   them there. On the phone each one is an app: all of the screen under a
   status line, one at a time. A swipe up from the bottom shows the open ones
   as cards; a tap brings one, a flick up closes it, a swipe along the bottom
   edge goes to the next. The window with the keyboard comes first.
3. **The desk is covered.** While the session is away, the real monitors show
   a curtain: whoever walks past your desk sees that it is in use from the
   phone, not what you are doing.
4. **Back at the desk**, any key or movement of the mouse there takes the
   session back: the curtain asks you to unlock (Marea's lock: the session
   was out of your hands), the windows go back to their monitors and pools,
   and the phone says the session went back to the desk. Touching the phone
   again takes it away again.
5. **The phone leaves** (the page closed, the network gone for long enough):
   the same as coming back, without the lock — nobody left the desk exposed.

## Pieces

| Piece | Where | |
| --- | --- | --- |
| A monitor with no screen, put up and taken down while running | `session.rs`, `headless.rs`, `phone.rs` | the same road as a monitor plugged in: the scene's copies are given again |
| `P W H SCALE` · `P off` | `agent.rs` → `layers` → the session's loop | asked by the remote server |
| `phone` · `phone_cards`, `phone_next`, `phone_prev` | a fact and events of the scene | which monitor is the phone (-1: none); the gestures from its bottom edge (`E` on the agent socket) |
| `U HEX` | `agent.rs` | the phone's keyboard: any text into the window with the keyboard, as the person's own typing |
| Apps, cards, status line, gestures | `session.plm` | the phone's copy of the scene |
| The curtain | `session.plm` | a surface over the real monitors, left out of captures |
| Touch | `remote.html` | taps, holds, scrolls with momentum, swipes from the edges; the phone's keyboard types |

## Marea, and other panels

Marea follows you onto the phone (`screens: each max 3`): she lives at the
top of it, in the middle of the status line, like an island, and her card
opens there. A panel wider than the phone —her surface is 820 points, for
her card and its shadow— is shown smaller on the phone's monitor, as if it
were 580 points across (`phone_zoom`): the compositor scales where its
pieces go and where the pointer touches them, and the program draws as
ever. Programs that run through XWayland are drawn at scale 1 and enlarged
on the phone: softer than the rest (Discord and the browsers are native).

## Locked at the desk

The lock screen is only on the real monitors, so a locked session does not
go to the phone: the phone shows the desk as it is —its lock screen— with a
note on top, the phone's keyboard types the password there (each character
as the key of the desk's own layout), and the session comes the moment it
is unlocked.

## Settings

`session.conf`: `phone lock COMMAND` is what locks the session when it comes
back to the desk (by default `marea lock`; `phone lock none`, nothing). The
page tells it is on a phone by itself; `?phone=1` or `?phone=0` says so.

## Trying it without a phone

`PLEAMAR_HEADLESS_PHONE="1080x2400@2.5 3 20"` puts the phone's monitor up on
a headless desktop by itself (3 s in, down at 20 s); `grim -o PHONE-1`
takes its picture. With `PLEAMAR_HEADLESS_INPUT_FIFO=path` on the headless
desktop and `PLEAMAR_REMOTE_HANDS_TO=path` on `pleamar-wm remote`, the
page's taps go down that pipe instead of to real devices: an emulated phone
(Chrome's device mode) can drive the whole thing without touching the real
session's mouse.

## Measured

Headless, four windows, three runs (2026-10-07):

| | |
| --- | --- |
| Asked for → every window on the phone | 188–197 ms |
| Given back → every window in its monitor and pool | 58–264 ms |
| The arrival, as it is seen | the deck at once; the app in front opens out of it 1.3 s later |

The phone's monitor is painted at the phone's own pixels (1080 × 2344 at
scale 2.45 on a 393-point-wide phone: about 440 points across), so text is
as sharp as the phone's own. Still to measure on a real phone: from a tap
to the picture changing.
