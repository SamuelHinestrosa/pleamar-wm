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
| `phone`, `phone.away` | facts of the scene | which monitor is the phone (-1: none), and whether the session is on it |
| Apps, cards, status line, gestures | `session.plm` | the phone's copy of the scene |
| The curtain | `session.plm` | a surface over the real monitors, left out of captures |
| Touch | `remote.html` | taps, holds, scrolls with momentum, swipes from the edges; the phone's keyboard types |

## Measured

- From a tap on the phone to the picture changing (the page stamps both).
- Sharpness: the phone monitor is painted at the phone's own pixels.
- Back at the desk: from the first key to every window in its place.
