//! A computer-use agent's hands: `cua-inject v1`, the line protocol Cua
//! Driver speaks to a compositor of its own (its nested `cua-compositor`),
//! here spoken by the session's. Cua Driver uses it whenever
//! `CUA_INJECT_SOCKET` is set, so the programs this session starts find it.
//!
//! What stock Wayland forbids a program —typing into a window without the
//! keyboard, clicking one that is under another— the compositor can do: the
//! agent has a seat of its own (`cua-agent`, one per cursor), so its pointer
//! and its keyboard are not yours, and your mouse and your focus stay where
//! they are while it works. A program that only listens to one seat
//! (Chromium, GLFW's) gets the agent's events on the usual one instead,
//! straight to its own objects, without moving anybody's focus.
//!
//! Where each cursor is goes to the scene (`agent.$k.x`…), which draws it:
//! a second cursor, moving on its own.
//!
//! The protocol: the client sends `cua-inject v1`, which is echoed back; then
//! one command a line, each answered by `ok` or `err <reason>` once it has
//! been delivered:
//!
//! | command | |
//! | --- | --- |
//! | `q PID` | `state FOCUSED_PID foreground|background_visible|background_occluded|not_found` |
//! | `g PID` | `geometry AX AY SX SY`: where the window (AX, AY) and its root surface (SX, SY) are on the desktop |
//! | `r PID` | `rect X Y W H VISIBLE`: the root surface's box on the desktop, as the scene shows it, and whether it is seen (1) — what a screenshot of the window is cut from, and what `m`'s coordinates count in |
//! | `f PID` | the keyboard to that process' only window (and its workspace shown) |
//! | `m TARGET IDX X Y` | cursor IDX to X, Y of the window (its root surface's coordinates) |
//! | `b TARGET IDX BUTTON PRESSED` | an evdev button (272 left), 1 down, 0 up |
//! | `a TARGET IDX AXIS VALUE` | the wheel: axis 0 vertical, 1 horizontal |
//! | `t TARGET HEX` | ASCII text, hex encoded, typed into the window |
//! | `k TARGET KEY` | a named key: enter, tab, escape, backspace, space, arrows, f1–f12 |
//! | `h TARGET MODS KEY` | a chord: `ctrl,shift` and a key |
//! | `d X Y COUNT BUTTON` | a click at a point of the desktop |
//!
//! A TARGET is `pid:N` (that process' only window), `root:N` (the only window
//! of that process or a child of it: a browser's), or an app_id.

use super::{State, Toplevel};
use crate::layers;
use pleamar::scene::{NestEvent, ToRender};
use smithay::backend::input::{Axis, AxisSource, ButtonState, KeyState};
use smithay::input::keyboard::{xkb, FilterResult, KeyboardHandle, XkbConfig};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent, PointerHandle};
use smithay::input::Seat;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, Mode, PostAction};
use smithay::reexports::wayland_server::protocol::{wl_keyboard, wl_pointer, wl_surface::WlSurface};
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::{UnixListener, UnixStream};

const HELLO: &str = "cua-inject v1";
/// Cursors the agent may drive at once.
const CURSORS: usize = 2;

/// The agent's seats and what each cursor last did.
pub struct Agent {
    seats: Vec<(Seat<State>, PointerHandle<State>, KeyboardHandle<State>)>,
    /// Per cursor, on the usual seat (for the programs that only hear that
    /// one): the surface it entered, straight on its objects.
    raw_entered: [Option<WlSurface>; CURSORS],
    /// Per cursor: how many things it has done, for the scene to know it moved.
    seen: [u32; CURSORS],
    /// And all of them, keys too: the scene knows the agent is at work.
    busy: u32,
    /// And where it last was, in its window's root surface.
    at: [Point<f64, Logical>; CURSORS],
    pub path: String,
}

/// Where the socket goes: beside the session's other ones.
pub fn socket_path(display: &str) -> String {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    format!("{dir}/pleamar-{display}/cua-inject.sock")
}

/// The agent's seats and its socket, if session.conf says `agent on`.
pub fn start(state: &mut State) {
    if !crate::config::get().agent {
        return;
    }
    let path = socket_path(&state.socket);
    if let Some(dir) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("agent · cannot listen at {path}: {e}");
            return;
        }
    };
    // Yours only.
    let _ = std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600));
    let _ = listener.set_nonblocking(true);
    let mut seats = Vec::new();
    for k in 0..CURSORS {
        let name = if k == 0 { "cua-agent".to_owned() } else { format!("cua-agent-{}", k + 1) };
        let mut seat = state.seats.new_wl_seat(&state.dh, name);
        // A plain US keyboard: what it types is what was asked, whatever yours is.
        let us = XkbConfig { layout: "us", ..Default::default() };
        let Ok(keyboard) = seat.add_keyboard(us, 600, 25) else { continue };
        let pointer = seat.add_pointer();
        seats.push((seat, pointer, keyboard));
    }
    let source = Generic::new(listener, Interest::READ, Mode::Level);
    let inserted = state.handle.insert_source(source, |_, listener, state: &mut State| {
        while let Ok((stream, _)) = listener.accept() {
            // Only the same user (the socket is 0600; this says it again).
            if !same_user(&stream) {
                continue;
            }
            let _ = stream.set_nonblocking(true);
            state.agent_connection(stream);
        }
        Ok(PostAction::Continue)
    });
    if let Err(e) = inserted {
        eprintln!("agent · {e}");
        return;
    }
    println!("agent · computer use: cua-inject v1 at {path}");
    state.agent = Some(Agent { seats, raw_entered: [None, None], seen: [0; CURSORS], busy: 0, at: [(0.0, 0.0).into(); CURSORS], path });
}

impl State {
    fn agent_connection(&mut self, stream: UnixStream) {
        let mut buffer = Vec::<u8>::new();
        let mut hello = false;
        let source = Generic::new(stream, Interest::READ, Mode::Level);
        let _ = self.handle.insert_source(source, move |_, stream, state: &mut State| {
            let stream: &mut UnixStream = unsafe { stream.get_mut() };
            let mut chunk = [0u8; 4096];
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) => return Ok(PostAction::Remove),
                    Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => return Ok(PostAction::Remove),
                }
                // A line longer than any command is not one.
                if buffer.len() > 64 * 1024 {
                    return Ok(PostAction::Remove);
                }
            }
            while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buffer.drain(..=end).collect();
                let line = String::from_utf8_lossy(&line).trim_end_matches(['\n', '\r']).to_owned();
                let reply = if !hello {
                    if line == HELLO {
                        hello = true;
                        HELLO.to_owned()
                    } else {
                        let _ = stream.write_all(b"err unsupported-version\n");
                        return Ok(PostAction::Remove);
                    }
                } else {
                    state.agent_command(&line)
                };
                // Blocking for the reply: it is one short line.
                let _ = stream.set_nonblocking(false);
                let ok = stream.write_all(format!("{reply}\n").as_bytes()).is_ok();
                let _ = stream.set_nonblocking(true);
                if !ok {
                    return Ok(PostAction::Remove);
                }
            }
            Ok(PostAction::Continue)
        });
    }

    /// One command, answered once it has been delivered.
    fn agent_command(&mut self, line: &str) -> String {
        let w: Vec<&str> = line.split_whitespace().collect();
        let result = match w.as_slice() {
            ["q", pid] => return self.agent_query(pid.parse().unwrap_or(0)),
            ["g", pid] => self.agent_geometry(pid.parse().unwrap_or(0)),
            ["r", pid] => self.agent_rect(pid.parse().unwrap_or(0)),
            ["f", pid] => self.agent_activate(pid.parse().unwrap_or(0)).map(|_| "ok".to_owned()),
            ["m", target, idx, x, y] => self.agent_target(target).and_then(|s| self.agent_motion(s, idx.parse().unwrap_or(99), num(x)?, num(y)?)).map(|_| "ok".into()),
            ["b", target, idx, button, pressed] => self.agent_target(target).and_then(|s| self.agent_button(s, idx.parse().unwrap_or(99), button.parse().map_err(|_| "bad-args")?, *pressed != "0")).map(|_| "ok".into()),
            ["a", target, idx, axis, value] => self.agent_target(target).and_then(|s| self.agent_axis(s, idx.parse().unwrap_or(99), *axis == "1", num(value)?)).map(|_| "ok".into()),
            ["t", target, hex] => self.agent_target(target).and_then(|s| self.agent_type(s, &unhex(hex)?)).map(|_| "ok".into()),
            ["k", target, key] => self.agent_target(target).and_then(|s| {
                let code = named_key(key).ok_or("unknown-key")?;
                self.agent_keys(s, &[], code)
            }).map(|_| "ok".into()),
            ["h", target, mods, key] => self.agent_target(target).and_then(|s| {
                let mut held = Vec::new();
                for m in mods.split(',') {
                    held.push(match m.to_ascii_lowercase().as_str() {
                        "ctrl" | "control" => 29,
                        "shift" => 42,
                        "alt" | "option" => 56,
                        "meta" | "super" | "win" | "cmd" => 125,
                        _ => return Err("unknown-hotkey"),
                    });
                }
                let code = named_key(key).or_else(|| (key.len() == 1).then(|| us_key(key.as_bytes()[0]).map(|(c, _)| c)).flatten()).ok_or("unknown-hotkey")?;
                self.agent_keys(s, &held, code)
            }).map(|_| "ok".into()),
            ["d", x, y, count, button] => num(x).and_then(|x| Ok((x, num(y)?))).and_then(|(x, y)| self.agent_desktop_click(x, y, count.parse().unwrap_or(1), button.parse().unwrap_or(272))).map(|_| "ok".into()),
            [] => Err("empty"),
            _ => Err("unknown-command"),
        };
        match result {
            Ok(reply) => reply,
            Err(e) => format!("err {e}"),
        }
    }

    fn pid_of(&self, slot: usize) -> u32 {
        let Some(Some(w)) = self.slots.get(slot) else { return 0 };
        if let Toplevel::X11(x) = &w.toplevel {
            if let Some(pid) = x.pid() {
                return pid;
            }
        }
        w.surface.client().and_then(|c| c.get_credentials(&self.dh).ok()).map_or(0, |c| c.pid as u32)
    }

    fn slots_open(&self) -> impl Iterator<Item = usize> + '_ {
        self.slots.iter().enumerate().filter(|(_, w)| w.is_some()).map(|(k, _)| k)
    }

    /// The window a target names: refused if it names none, or more than one.
    fn agent_target(&self, target: &str) -> Result<usize, &'static str> {
        let found: Vec<usize> = if let Some(pid) = target.strip_prefix("root:") {
            let pid: u32 = pid.parse().map_err(|_| "bad-root-pid")?;
            self.slots_open().filter(|s| in_family(self.pid_of(*s), pid)).collect()
        } else if let Some(pid) = target.strip_prefix("pid:") {
            let pid: u32 = pid.parse().map_err(|_| "bad-pid")?;
            self.slots_open().filter(|s| self.pid_of(*s) == pid).collect()
        } else {
            self.slots_open().filter(|s| self.slots[*s].as_ref().is_some_and(|w| w.app == target)).collect()
        };
        match found.as_slice() {
            [one] => Ok(*one),
            [] => Err(if target.starts_with("root:") { "unknown-root-pid" } else if target.starts_with("pid:") { "unknown-pid" } else { "unknown-app-id" }),
            _ => Err(if target.starts_with("root:") { "ambiguous-root-pid" } else if target.starts_with("pid:") { "ambiguous-pid" } else { "ambiguous-app-id" }),
        }
    }

    /// Where a window is on the desktop, in units: the window itself, and its
    /// root surface (a program's own shadow included), which is what the
    /// agent's coordinates count from. None if the scene does not show it.
    fn agent_place(&self, slot: usize) -> Option<([f64; 2], [f64; 2], f64, usize)> {
        let w = self.slots.get(slot)?.as_ref()?;
        let (name, r) = w.shown.clone()?;
        let monitors = layers::monitors();
        let k = monitors.iter().position(|m| m.name == name)?;
        let m = &monitors[k];
        // How much the scene scales it (1 when laid out at its own size).
        let g = w.geometry;
        let zoom = if g[2] > 0 { r[2] as f64 / m.scale / g[2] as f64 } else { 1.0 };
        let ax = m.x as f64 + r[0] as f64 / m.scale;
        let ay = m.y as f64 + r[1] as f64 / m.scale;
        Some(([ax, ay], [ax - g[0] as f64 * zoom, ay - g[1] as f64 * zoom], zoom, k))
    }

    fn agent_query(&self, pid: u32) -> String {
        let focused = self.focus.map_or(0, |s| self.pid_of(s));
        let target = self.slots_open().find(|s| self.pid_of(*s) == pid);
        let state = match target {
            None => "not_found",
            Some(s) if Some(s) == self.focus => "foreground",
            Some(s) => {
                if self.agent_place(s).is_some() {
                    "background_visible"
                } else {
                    "background_occluded"
                }
            }
        };
        format!("state {focused} {state}")
    }

    fn agent_geometry(&self, pid: u32) -> Result<String, &'static str> {
        let found: Vec<usize> = self.slots_open().filter(|s| in_family(self.pid_of(*s), pid)).collect();
        let slot = match found.as_slice() {
            [one] => *one,
            [] => return Err("target-not-found"),
            _ => return Err("ambiguous-pid"),
        };
        let (window, root, _, _) = self.agent_place(slot).ok_or("unmapped-target")?;
        Ok(format!("geometry {} {} {} {}", window[0].round(), window[1].round(), root[0].round(), root[1].round()))
    }

    /// The box a picture of the window is cut from: its root surface (a
    /// program's own shadow included) where the scene shows it, so that a
    /// pixel of that picture is a point `m` understands. Said by the
    /// compositor, which is what draws it: an agent can trust it.
    fn agent_rect(&self, pid: u32) -> Result<String, &'static str> {
        let found: Vec<usize> = self.slots_open().filter(|s| in_family(self.pid_of(*s), pid)).collect();
        let slot = match found.as_slice() {
            [one] => *one,
            [] => return Err("target-not-found"),
            _ => return Err("ambiguous-pid"),
        };
        let w = self.slots[slot].as_ref().ok_or("gone")?;
        let Some((_, root, zoom, _)) = self.agent_place(slot) else {
            return Ok("rect 0 0 0 0 0".to_owned());
        };
        // From the root surface's corner to the window's far edges: the scene
        // draws the window only (not a program's own shadow), and starting at
        // the root's corner keeps a pixel of the picture a point `m` takes.
        let g = w.geometry;
        let (width, height) = (((g[0] + g[2]) as f64 * zoom).round(), ((g[1] + g[3]) as f64 * zoom).round());
        Ok(format!("rect {} {} {} {} 1", root[0].round(), root[1].round(), width, height))
    }

    fn agent_activate(&mut self, pid: u32) -> Result<(), &'static str> {
        let found: Vec<usize> = self.slots_open().filter(|s| self.pid_of(*s) == pid).collect();
        let slot = match found.as_slice() {
            [one] => *one,
            [] => return Err("unknown-pid"),
            _ => return Err("ambiguous-pid"),
        };
        self.set_focus(Some(slot));
        // On another workspace, the scene goes there.
        self.tell(NestEvent::Reveal(slot));
        Ok(())
    }

    /// The scene is told the agent is at work, on which window and monitor:
    /// it lights that monitor's edges and that window's outline while it is.
    fn agent_busy(&mut self, slot: usize) {
        let Some(agent) = self.agent.as_mut() else { return };
        agent.busy = agent.busy.wrapping_add(1);
        let busy = agent.busy;
        let monitor = self.agent_place(slot).map_or(-1.0, |p| p.3 as f64);
        for (name, v) in [("agent.win", slot as f64), ("agent.screen", monitor), ("agent.seen", busy as f64)] {
            let _ = self.to_render.send(ToRender::Fact(pleamar::scene::intern(name), v as f32));
        }
    }

    /// The scene is told where cursor `idx` is, on which monitor, and that
    /// it did something (it draws it while it does).
    fn agent_show(&mut self, slot: usize, idx: usize, at: Point<f64, Logical>, down: Option<bool>) {
        self.agent_busy(slot);
        let Some(agent) = self.agent.as_mut() else { return };
        agent.seen[idx] = agent.seen[idx].wrapping_add(1);
        let seen = agent.seen[idx];
        let Some((_, root, zoom, monitor)) = self.agent_place(slot) else { return };
        let m = layers::monitors().get(monitor).map(|m| (m.x as f64, m.y as f64)).unwrap_or_default();
        let fact = |name: String, v: f64| {
            let _ = self.to_render.send(ToRender::Fact(pleamar::scene::intern(&name), v as f32));
        };
        fact(format!("agent.{idx}.x"), root[0] + at.x * zoom - m.0);
        fact(format!("agent.{idx}.y"), root[1] + at.y * zoom - m.1);
        fact(format!("agent.{idx}.screen"), monitor as f64);
        if let Some(down) = down {
            fact(format!("agent.{idx}.down"), if down { 1.0 } else { 0.0 });
        }
        fact(format!("agent.{idx}.seen"), seen as f64);
    }

    fn agent_motion(&mut self, slot: usize, idx: usize, x: f64, y: f64) -> Result<(), &'static str> {
        if idx >= CURSORS {
            return Err("bad-cursor");
        }
        let w = self.slots[slot].as_ref().ok_or("gone")?;
        let (root, g) = (w.surface.clone(), w.geometry);
        let at: Point<f64, Logical> = (x, y).into();
        let (surface, origin) = self.surface_under(&root, g, at).map(|(s, o)| (s, o.to_f64())).ok_or("no-surface-at-point")?;
        let client = surface.client().ok_or("gone")?;
        let (serial, time) = (SERIAL_COUNTER.next_serial(), self.time());
        let agent = self.agent.as_ref().ok_or("off")?;
        let own = agent.seats.get(idx).map(|s| s.1.clone()).filter(|p| p.client_pointers(&client).next().is_some());
        if let Some(pointer) = own {
            if let Some(a) = self.agent.as_mut() {
                a.raw_entered[idx] = None;
            }
            pointer.motion(self, Some((surface, origin)), &MotionEvent { location: at, serial, time });
            pointer.frame(self);
        } else {
            let local = at - origin;
            let pointers: Vec<wl_pointer::WlPointer> = self.pointer.client_pointers(&client).collect();
            if pointers.is_empty() {
                return Err("no-pointer-resource");
            }
            let entered = self.agent.as_ref().and_then(|a| a.raw_entered[idx].clone());
            if entered.as_ref() != Some(&surface) {
                if let Some(old) = entered.filter(|s| s.is_alive()) {
                    if let Some(c) = old.client() {
                        for p in self.pointer.client_pointers(&c) {
                            p.leave(SERIAL_COUNTER.next_serial().into(), &old);
                            frame(&p);
                        }
                    }
                }
                for p in &pointers {
                    p.enter(SERIAL_COUNTER.next_serial().into(), &surface, local.x, local.y);
                    frame(p);
                }
                if let Some(a) = self.agent.as_mut() {
                    a.raw_entered[idx] = Some(surface.clone());
                }
            }
            for p in &pointers {
                p.motion(time, local.x, local.y);
                frame(p);
            }
        }
        if let Some(a) = self.agent.as_mut() {
            a.at[idx] = at;
        }
        self.agent_show(slot, idx, at, None);
        Ok(())
    }

    fn agent_button(&mut self, slot: usize, idx: usize, button: u32, pressed: bool) -> Result<(), &'static str> {
        if idx >= CURSORS || !(272..=279).contains(&button) {
            return Err("bad-args");
        }
        let (serial, time) = (SERIAL_COUNTER.next_serial(), self.time());
        let agent = self.agent.as_ref().ok_or("off")?;
        let (_, pointer, _) = agent.seats.get(idx).cloned().ok_or("bad-cursor")?;
        let raw = agent.raw_entered[idx].clone();
        if let Some(surface) = raw {
            let client = surface.client().ok_or("gone")?;
            let state = if pressed { wl_pointer::ButtonState::Pressed } else { wl_pointer::ButtonState::Released };
            let mut any = false;
            for p in self.pointer.client_pointers(&client) {
                p.button(serial.into(), time, button, state);
                frame(&p);
                any = true;
            }
            if !any {
                return Err("no-pointer-resource");
            }
        } else if pointer.current_focus().is_some() {
            let state = if pressed { ButtonState::Pressed } else { ButtonState::Released };
            pointer.button(self, &ButtonEvent { serial, time, button, state });
            pointer.frame(self);
        } else {
            return Err("no-pointer-resource");
        }
        // A press of the agent's does not change whose the keyboard is: it stays yours.
        let at = self.agent.as_ref().map_or((0.0, 0.0).into(), |a| a.at[idx]);
        self.agent_show(slot, idx, at, Some(pressed));
        Ok(())
    }

    fn agent_axis(&mut self, slot: usize, idx: usize, horizontal: bool, value: f64) -> Result<(), &'static str> {
        if idx >= CURSORS || !value.is_finite() {
            return Err("bad-args");
        }
        self.agent_busy(slot);
        let time = self.time();
        let agent = self.agent.as_ref().ok_or("off")?;
        let (_, pointer, _) = agent.seats.get(idx).cloned().ok_or("bad-cursor")?;
        let raw = agent.raw_entered[idx].clone();
        let step = if value < 0.0 { -1 } else { 1 };
        if raw.is_none() && pointer.current_focus().is_some() {
            let axis = if horizontal { Axis::Horizontal } else { Axis::Vertical };
            let frame = AxisFrame::new(time).source(AxisSource::Wheel).value(axis, value).v120(axis, step * 120);
            pointer.axis(self, frame);
            pointer.frame(self);
        } else {
            let surface = raw.ok_or("no-pointer-resource")?;
            let client = surface.client().ok_or("gone")?;
            let axis = if horizontal { wl_pointer::Axis::HorizontalScroll } else { wl_pointer::Axis::VerticalScroll };
            let mut any = false;
            for p in self.pointer.client_pointers(&client) {
                if p.version() >= 5 {
                    p.axis_source(wl_pointer::AxisSource::Wheel);
                }
                if p.version() >= 8 {
                    p.axis_value120(axis, step * 120);
                } else if p.version() >= 5 {
                    p.axis_discrete(axis, step);
                }
                p.axis(time, axis, value);
                frame(&p);
                any = true;
            }
            if !any {
                return Err("no-pointer-resource");
            }
        }
        Ok(())
    }

    /// Keys into a window: held modifiers, then the key, all let go.
    fn agent_keys(&mut self, slot: usize, held: &[u32], code: u32) -> Result<(), &'static str> {
        self.agent_press(slot, &[(held.to_vec(), code)])
    }

    /// ASCII text, each character as the keys that make it: on a US
    /// keyboard, or on yours if it goes through yours.
    fn agent_type(&mut self, slot: usize, text: &[u8]) -> Result<(), &'static str> {
        let yours = self.agent_through_yours(slot);
        let table = if yours { Some(self.your_keymap(|k| char_table(k))) } else { None };
        let mut presses = Vec::with_capacity(text.len());
        for &c in text {
            let (code, shift) = match &table {
                Some(t) => t.get(c as usize).copied().flatten().ok_or("unknown-char")?,
                None => us_key(c).ok_or("unknown-char")?,
            };
            presses.push((if shift { vec![42] } else { vec![] }, code));
        }
        self.agent_press(slot, &presses)
    }

    /// Something read from your keyboard's layout.
    fn your_keymap<T>(&mut self, f: impl Fn(&xkb::Keymap) -> T) -> T {
        let keyboard = self.keyboard.clone();
        keyboard.with_xkb_state(self, |ctx| {
            let xkb = ctx.xkb().lock().unwrap();
            f(unsafe { xkb.keymap() })
        })
    }

    /// Whether keys for that window go through your keyboard: the program
    /// does not hear the agent's, and the window is the one you are typing in.
    fn agent_through_yours(&self, slot: usize) -> bool {
        let Some(Some(w)) = self.slots.get(slot) else { return false };
        let Some(client) = w.surface.client() else { return false };
        let own = self.agent.as_ref().and_then(|a| a.seats.first()).is_some_and(|s| s.2.client_keyboards(&client).next().is_some());
        !own && self.focus == Some(slot) && self.host_focus
    }

    fn agent_press(&mut self, slot: usize, presses: &[(Vec<u32>, u32)]) -> Result<(), &'static str> {
        self.agent_busy(slot);
        let w = self.slots[slot].as_ref().ok_or("gone")?;
        let root = w.surface.clone();
        let client = root.client().ok_or("gone")?;
        let agent = self.agent.as_ref().ok_or("off")?;
        let own = agent.seats.first().map(|s| s.2.clone()).filter(|k| k.client_keyboards(&client).next().is_some());
        let mine = self.agent_through_yours(slot);
        // The agent's own keyboard (US), which the program hears apart from yours.
        if let Some(keyboard) = own {
            if keyboard.current_focus().as_ref() != Some(&root) {
                keyboard.set_focus(self, Some(root.clone()), SERIAL_COUNTER.next_serial());
            }
            for (held, code) in presses {
                for (code, down) in held.iter().map(|c| (*c, true)).chain(std::iter::once((*code, true))).chain(std::iter::once((*code, false))).chain(held.iter().rev().map(|c| (*c, false))) {
                    let state = if down { KeyState::Pressed } else { KeyState::Released };
                    let time = self.time();
                    keyboard.input::<(), _>(self, (code + 8).into(), state, SERIAL_COUNTER.next_serial(), time, |_, _, _| FilterResult::Forward);
                }
            }
            return Ok(());
        }
        // If it is the window you are typing in, as if you typed it (the
        // characters were already looked up in your layout).
        if mine {
            let keyboard = self.keyboard.clone();
            for (held, code) in presses {
                for (code, down) in held.iter().map(|c| (*c, true)).chain(std::iter::once((*code, true))).chain(std::iter::once((*code, false))).chain(held.iter().rev().map(|c| (*c, false))) {
                    let state = if down { KeyState::Pressed } else { KeyState::Released };
                    let time = self.time();
                    keyboard.input::<(), _>(self, (code + 8).into(), state, SERIAL_COUNTER.next_serial(), time, |_, _, _| FilterResult::Forward);
                }
            }
            return Ok(());
        }
        // Not yours: straight to its keyboard objects, entered and left around
        // the keys, with the US keymap for that while and yours back after.
        let keyboards: Vec<wl_keyboard::WlKeyboard> = self.keyboard.client_keyboards(&client).collect();
        if keyboards.is_empty() {
            return Err("no-keyboard-resource");
        }
        let keymap = self.your_keymap(|k| k.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1));
        let us_keymap = us_keymap_string();
        let us = keymap_fd(&us_keymap).ok_or("no-keymap")?;
        let yours = keymap_fd(&keymap);
        let time = self.time();
        for k in &keyboards {
            k.keymap(wl_keyboard::KeymapFormat::XkbV1, us.0.as_fd(), us.1);
            k.enter(SERIAL_COUNTER.next_serial().into(), &root, Vec::new());
            k.modifiers(SERIAL_COUNTER.next_serial().into(), 0, 0, 0, 0);
        }
        for (held, code) in presses {
            let mask = held.iter().fold(0u32, |m, c| m | modifier_mask(*c));
            for k in &keyboards {
                if mask != 0 {
                    k.modifiers(SERIAL_COUNTER.next_serial().into(), mask, 0, 0, 0);
                }
                k.key(SERIAL_COUNTER.next_serial().into(), time, *code, wl_keyboard::KeyState::Pressed);
                k.key(SERIAL_COUNTER.next_serial().into(), time, *code, wl_keyboard::KeyState::Released);
                if mask != 0 {
                    k.modifiers(SERIAL_COUNTER.next_serial().into(), 0, 0, 0, 0);
                }
            }
        }
        for k in &keyboards {
            k.leave(SERIAL_COUNTER.next_serial().into(), &root);
            if let Some(y) = &yours {
                k.keymap(wl_keyboard::KeymapFormat::XkbV1, y.0.as_fd(), y.1);
            }
        }
        Ok(())
    }

    /// A click at a point of the desktop: on the window the scene shows there.
    fn agent_desktop_click(&mut self, x: f64, y: f64, count: u32, button: u32) -> Result<(), &'static str> {
        let mut hit = None;
        for slot in self.slots_open().collect::<Vec<_>>() {
            let Some((window, root, zoom, _)) = self.agent_place(slot) else { continue };
            let g = self.slots[slot].as_ref().map_or([0; 4], |w| w.geometry);
            let inside = x >= window[0] && y >= window[1] && x < window[0] + g[2] as f64 * zoom && y < window[1] + g[3] as f64 * zoom;
            // The one with the keyboard first, if several are there.
            if inside && (hit.is_none() || self.focus == Some(slot)) {
                hit = Some((slot, (x - root[0]) / zoom, (y - root[1]) / zoom));
            }
        }
        let (slot, lx, ly) = hit.ok_or("no-surface-at-point")?;
        self.agent_motion(slot, 0, lx, ly)?;
        for _ in 0..count.clamp(1, 3) {
            self.agent_button(slot, 0, button, true)?;
            self.agent_button(slot, 0, button, false)?;
        }
        Ok(())
    }
}

/// Whether the one at the other end of the socket is this user.
fn same_user(stream: &UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred { pid: 0, uid: u32::MAX, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let got = unsafe { libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, &mut cred as *mut _ as *mut libc::c_void, &mut len) };
    got == 0 && cred.uid == unsafe { libc::getuid() }
}

fn frame(p: &wl_pointer::WlPointer) {
    if p.version() >= 5 {
        p.frame();
    }
}

fn num(s: &str) -> Result<f64, &'static str> {
    s.parse::<f64>().ok().filter(|v| v.is_finite()).ok_or("bad-args")
}

fn unhex(hex: &str) -> Result<Vec<u8>, &'static str> {
    if hex.len() % 2 != 0 || hex.len() > 8192 {
        return Err("bad-args");
    }
    (0..hex.len()).step_by(2).map(|k| u8::from_str_radix(&hex[k..k + 2], 16).map_err(|_| "bad-args")).collect()
}

/// Whether `pid` is `root` or one of its children: a browser's windows may
/// belong to a process it started.
fn in_family(mut pid: u32, root: u32) -> bool {
    if pid == 0 || root == 0 {
        return false;
    }
    for _ in 0..64 {
        if pid == root {
            return true;
        }
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { return false };
        // After the name, which is in parentheses and may hold anything.
        let Some(after) = stat.rfind(')').map(|k| &stat[k + 2..]) else { return false };
        let Some(parent) = after.split_whitespace().nth(1).and_then(|p| p.parse::<u32>().ok()) else { return false };
        if parent <= 1 || parent == pid {
            return false;
        }
        pid = parent;
    }
    false
}

/// The keys the protocol names, as evdev codes.
fn named_key(name: &str) -> Option<u32> {
    let n = name.to_ascii_lowercase();
    Some(match n.as_str() {
        "enter" | "return" => 28,
        "tab" => 15,
        "escape" | "esc" => 1,
        "backspace" => 14,
        "space" => 57,
        "up" => 103,
        "down" => 108,
        "left" => 105,
        "right" => 106,
        "delete" => 111,
        "home" => 102,
        "end" => 107,
        "pageup" => 104,
        "pagedown" => 109,
        _ => {
            let f: u32 = n.strip_prefix('f')?.parse().ok()?;
            match f {
                1..=10 => 58 + f,
                11 => 87,
                12 => 88,
                _ => return None,
            }
        }
    })
}

/// An ASCII character on a US keyboard: its evdev key, and whether with shift.
fn us_key(c: u8) -> Option<(u32, bool)> {
    const LOWER: &[(u8, u32)] = &[
        (b'1', 2), (b'2', 3), (b'3', 4), (b'4', 5), (b'5', 6), (b'6', 7), (b'7', 8), (b'8', 9), (b'9', 10), (b'0', 11),
        (b'-', 12), (b'=', 13), (b'q', 16), (b'w', 17), (b'e', 18), (b'r', 19), (b't', 20), (b'y', 21), (b'u', 22),
        (b'i', 23), (b'o', 24), (b'p', 25), (b'[', 26), (b']', 27), (b'a', 30), (b's', 31), (b'd', 32), (b'f', 33),
        (b'g', 34), (b'h', 35), (b'j', 36), (b'k', 37), (b'l', 38), (b';', 39), (b'\'', 40), (b'`', 41), (b'\\', 43),
        (b'z', 44), (b'x', 45), (b'c', 46), (b'v', 47), (b'b', 48), (b'n', 49), (b'm', 50), (b',', 51), (b'.', 52),
        (b'/', 53), (b' ', 57), (b'\n', 28), (b'\t', 15),
    ];
    const UPPER: &[(u8, u8)] = &[
        (b'!', b'1'), (b'@', b'2'), (b'#', b'3'), (b'$', b'4'), (b'%', b'5'), (b'^', b'6'), (b'&', b'7'), (b'*', b'8'),
        (b'(', b'9'), (b')', b'0'), (b'_', b'-'), (b'+', b'='), (b'{', b'['), (b'}', b']'), (b':', b';'), (b'"', b'\''),
        (b'~', b'`'), (b'|', b'\\'), (b'<', b','), (b'>', b'.'), (b'?', b'/'),
    ];
    if let Some((_, k)) = LOWER.iter().find(|(ch, _)| *ch == c) {
        return Some((*k, false));
    }
    if c.is_ascii_uppercase() {
        return us_key(c.to_ascii_lowercase()).map(|(k, _)| (k, true));
    }
    UPPER.iter().find(|(ch, _)| *ch == c).and_then(|(_, base)| us_key(*base)).map(|(k, _)| (k, true))
}

/// The modifier bit a key holds down, in a US keymap (Shift, Control, Mod1, Mod4).
fn modifier_mask(code: u32) -> u32 {
    match code {
        42 => 1,
        29 => 4,
        56 => 8,
        125 => 64,
        _ => 0,
    }
}

/// Each ASCII character in a keymap: its evdev key and whether with shift
/// (the first two levels only: what needs AltGr is left out).
fn char_table(keymap: &xkb::Keymap) -> Vec<Option<(u32, bool)>> {
    let mut table = vec![None; 128];
    for code in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
        for level in 0..2 {
            for sym in keymap.key_get_syms_by_level(code.into(), 0, level) {
                let c = xkb::keysym_to_utf32(*sym);
                if (1..128).contains(&c) && table[c as usize].is_none() && code >= 8 {
                    table[c as usize] = Some((code - 8, level == 1));
                }
            }
        }
    }
    table[b'\n' as usize] = Some((28, false));
    table[b'\t' as usize] = Some((15, false));
    table
}

fn us_keymap_string() -> String {
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    xkb::Keymap::new_from_names(&context, "", "", "us", "", None, xkb::KEYMAP_COMPILE_NO_FLAGS)
        .map(|k| k.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1))
        .unwrap_or_default()
}

/// A keymap in shared memory, as wl_keyboard.keymap hands it over.
fn keymap_fd(text: &str) -> Option<(std::os::fd::OwnedFd, u32)> {
    use std::os::fd::FromRawFd;
    let name = std::ffi::CString::new("cua-keymap").ok()?;
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    let mut file = std::fs::File::from(fd);
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(0);
    file.write_all(&bytes).ok()?;
    Some((file.into(), bytes.len() as u32))
}

/// Taken by the agent's seats out of what the session's seat does.
pub fn is_agent_seat(seat: &Seat<State>) -> bool {
    seat.name().starts_with("cua-agent")
}
