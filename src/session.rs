//! pleamar-wm's own session: no compositor underneath. It is started from a
//! TTY of its own (`pleamar-wm session scene.plm`), takes the monitors and the
//! input through the seat (libseat: logind or seatd, no root), and paints each
//! monitor straight into buffers of the card that go to the screen with a page
//! flip. Programs connect to the compositor inside the scene as before.
//!
//! Ctrl+Alt+Backspace leaves at once; Ctrl+Alt+F1…F12 go to another TTY and
//! back. Everything it does is said on its output: run it with that going to
//! a file, so that if the screen stays black there is something to read.

use pleamar::scene::{Cursor, Mods, Screens, Surface, ToRender};
use pleamar::wgpu;
use crate::layers::{self, MonitorInfo, ToLayers};
use crate::screen::{self, Hit, LayerFrames, LayerWindow, Output, Screen};
use pleamar::{NewSheet, Target, View};
use smithay::backend::drm::DrmDeviceFd;
use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, InputEvent, KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session as _};
use smithay::backend::udev::{all_gpus, primary_gpu};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, Mode as LoopMode, PostAction};
use smithay::reexports::drm::control::{connector, crtc, framebuffer, Device as ControlDevice, Event as DrmEvent, FbCmd2Flags, Mode, ModeTypeFlags, PageFlipFlags};
use smithay::reexports::gbm;
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;
use smithay::utils::DeviceFd;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use xkbcommon::xkb;

/// The platform that drives the monitors itself.
pub struct Session;

impl pleamar::Platform for Session {
    fn run(self: Box<Self>, surfaces: Vec<Surface>, _extra_height: u32, _instance: wgpu::Instance, to_render: Sender<ToRender>) {
        if let Err(e) = run(surfaces, to_render.clone()) {
            eprintln!("session · {e}");
            let _ = to_render.send(ToRender::Quit);
            std::thread::sleep(Duration::from_millis(300));
            std::process::exit(1);
        }
        let _ = to_render.send(ToRender::Quit);
        std::thread::sleep(Duration::from_millis(300));
        std::process::exit(0);
    }
}

/// Which of a monitor's buffers is on screen and which is on its way.
#[derive(Default)]
struct Flips {
    on_screen: Option<usize>,
    pending: Option<usize>,
    /// Whether the monitor has been given its mode with a first frame.
    set: bool,
}

struct Monitor {
    name: String,
    crtc: crtc::Handle,
    size: (u32, u32),
    /// Where it is on the desktop, left to right.
    x: i32,
    /// What is shown on it: its surfaces, put together.
    screen: Screen,
    flips: Arc<Mutex<Flips>>,
    mhz: i32,
}

/// A monitor's buffers on the card: three, what each frame is put together
/// in and shown with a page flip.
struct DrmOutput {
    drm: DrmDeviceFd,
    gbm: Arc<Mutex<gbm::Device<DrmDeviceFd>>>,
    connector: connector::Handle,
    crtc: crtc::Handle,
    mode: Mode,
    size: (u32, u32),
    name: String,
    buffers: Vec<Buffer>,
    failed: bool,
    flips: Arc<Mutex<Flips>>,
}

struct Buffer {
    _bo: gbm::BufferObject<()>,
    fb: framebuffer::Handle,
    texture: wgpu::Texture,
}

impl DrmOutput {
    fn make_buffers(&mut self, device: &wgpu::Device, modifiers: &[u64]) -> Result<(), String> {
        let (w, h) = self.size;
        let wanted: Vec<u64> = if modifiers.is_empty() { vec![0] } else { modifiers.to_vec() };
        for _ in 0..3 {
            let bo = self
                .gbm
                .lock()
                .unwrap()
                .create_buffer_object_with_modifiers2::<()>(w, h, gbm::Format::Xrgb8888, wanted.iter().map(|m| gbm::Modifier::from(*m)), gbm::BufferObjectFlags::SCANOUT | gbm::BufferObjectFlags::RENDERING)
                .map_err(|e| format!("the card did not make a buffer of {w}×{h}: {e}"))?;
            let fd = bo.fd_for_plane(0).map_err(|e| format!("no handle for the buffer: {e}"))?;
            let modifier: u64 = bo.modifier().into();
            let texture = pleamar::gpu::Gpu::import_dmabuf(
                device,
                fd,
                (w, h),
                modifier,
                bo.stride_for_plane(0),
                bo.offset(0),
                wgpu::TextureUses::COLOR_TARGET,
                wgpu::TextureUsages::RENDER_ATTACHMENT,
                wgpu::TextureUses::UNINITIALIZED,
            )?;
            let flags = if bo.modifier() == gbm::Modifier::Invalid { FbCmd2Flags::empty() } else { FbCmd2Flags::MODIFIERS };
            let fb = self.drm.add_planar_framebuffer(&bo, flags).map_err(|e| format!("the monitor does not take the buffer: {e}"))?;
            self.buffers.push(Buffer { _bo: bo, fb, texture });
        }
        println!("session · {}: {w}×{h} at {} Hz, three buffers of the card (modifier {:#x})", self.name, self.mode.vrefresh(), wanted[0]);
        Ok(())
    }
}

impl Output for DrmOutput {
    fn buffer(&mut self, device: &wgpu::Device, modifiers: &[u64]) -> Option<(usize, wgpu::Texture)> {
        if self.failed {
            return None;
        }
        if self.buffers.is_empty() {
            if let Err(e) = self.make_buffers(device, modifiers) {
                eprintln!("session · {}: {e}", self.name);
                self.failed = true;
                return None;
            }
        }
        let f = self.flips.lock().unwrap();
        (0..self.buffers.len()).find(|k| f.on_screen != Some(*k) && f.pending != Some(*k)).map(|k| (k, self.buffers[k].texture.clone()))
    }

    fn show(&mut self, which: usize, done: wgpu::SubmissionIndex, device: &wgpu::Device, _: &wgpu::Queue, anew: bool) -> bool {
        // The monitor shows what is in the buffer when it flips: it has to be put together by then.
        let _ = device.poll(wgpu::PollType::Wait { submission_index: Some(done), timeout: Some(Duration::from_millis(100)) });
        let mut f = self.flips.lock().unwrap();
        if anew {
            *f = Flips::default();
        }
        let fb = self.buffers[which].fb;
        if !f.set {
            match self.drm.set_crtc(self.crtc, Some(fb), (0, 0), &[self.connector], Some(self.mode)) {
                Ok(()) => {
                    f.set = true;
                    f.on_screen = Some(which);
                }
                Err(e) => eprintln!("session · {}: the monitor did not take its first frame: {e}", self.name),
            }
            false
        } else {
            match self.drm.page_flip(self.crtc, fb, PageFlipFlags::EVENT, None) {
                Ok(()) => {
                    f.pending = Some(which);
                    true
                }
                Err(e) => {
                    eprintln!("session · {}: a frame did not go to the screen: {e}", self.name);
                    false
                }
            }
        }
    }
}

/// Everything the loop holds.
struct State {
    session: LibSeatSession,
    drm: DrmDeviceFd,
    monitors: Vec<Monitor>,
    libinput: Libinput,
    to_render: Sender<ToRender>,
    keymap: xkb::State,
    pointer: (f64, f64),
    cursor: Option<gbm::BufferObject<()>>,
    scroll: f64,
    /// Who has the pointer: the scene, or a program's surface (layer-shell).
    hit: Hit,
    /// The program's surface a button was pressed on: it keeps the pointer until it is let go.
    grab: Option<u64>,
    /// The program's surface that took the keyboard when clicked.
    key_client: Option<u64>,
    quit: bool,
}

fn run(surfaces: Vec<Surface>, to_render: Sender<ToRender>) -> Result<(), String> {
    let (mut session, notifier) = LibSeatSession::new().map_err(|e| format!("there is no seat to take ({e:?}): start it from a TTY of its own, logged in there"))?;
    let seat = session.seat();
    println!("session · seat {seat}");
    let card = primary_gpu(&seat)
        .ok()
        .flatten()
        .or_else(|| all_gpus(&seat).ok()?.into_iter().next())
        .ok_or("there is no graphics card")?;
    let fd = session.open(&card, OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK).map_err(|e| format!("{} could not be opened: {e:?}", card.display()))?;
    let drm = DrmDeviceFd::new(DeviceFd::from(fd));
    println!("session · card {}", card.display());
    let gbm = Arc::new(Mutex::new(gbm::Device::new(drm.clone()).map_err(|e| format!("no buffers on the card (gbm): {e}"))?));

    // The monitors that are connected, each with its preferred mode and a
    // controller of its own, left to right in the order the card lists them.
    let res = drm.resource_handles().map_err(|e| format!("the card says nothing of its monitors: {e}"))?;
    let mut monitors: Vec<Monitor> = Vec::new();
    let mut x = 0;
    for &conn in res.connectors() {
        let Ok(info) = drm.get_connector(conn, true) else { continue };
        if info.state() != connector::State::Connected {
            continue;
        }
        let Some(mode) = info.modes().iter().find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED)).or(info.modes().first()).copied() else { continue };
        let used: Vec<crtc::Handle> = monitors.iter().map(|m| m.crtc).collect();
        let Some(crtc) = info
            .encoders()
            .iter()
            .filter_map(|e| drm.get_encoder(*e).ok())
            .flat_map(|e| res.filter_crtcs(e.possible_crtcs()))
            .find(|c| !used.contains(c))
        else {
            continue;
        };
        let name = format!("{}-{}", info.interface().as_str(), info.interface_id());
        let (w, h) = mode.size();
        println!("session · monitor {name}: {w}×{h} at {} Hz", mode.vrefresh());
        let size = (w as u32, h as u32);
        let flips: Arc<Mutex<Flips>> = Default::default();
        let output = DrmOutput { drm: drm.clone(), gbm: gbm.clone(), connector: conn, crtc, mode, size, name: name.clone(), buffers: Vec::new(), failed: false, flips: flips.clone() };
        let screen = screen::screen(name.clone(), size, Box::new(output));
        monitors.push(Monitor { name, crtc, size, x, screen, flips, mhz: mode.vrefresh() as i32 * 1000 });
        x += w as i32;
    }
    if monitors.is_empty() {
        return Err("no monitor is connected".into());
    }
    // Left to right as `PLEAMAR_MONITORS` says («DP-3,HDMI-A-1»); the ones it
    // does not name, after, in the card's order.
    if let Ok(order) = std::env::var("PLEAMAR_MONITORS") {
        let names: Vec<&str> = order.split(',').map(str::trim).collect();
        monitors.sort_by_key(|m| names.iter().position(|n| *n == m.name).unwrap_or(names.len()));
        let mut x = 0;
        for m in &mut monitors {
            m.x = x;
            x += m.size.0 as i32;
        }
        println!("session · monitors, left to right: {}", monitors.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(", "));
    }

    // The scene's surfaces on the monitors. Its own: its copies (`screens:
    // each`) one per monitor, in order; without copies, on the first. The
    // named ones —a bar, a corner, a panel—: where they say, and put together
    // over or under the scene's by their level.
    let cursor_kind = Arc::new(Mutex::new(Cursor::Normal));
    let mut id = 1000;
    let names: Vec<String> = monitors.iter().map(|m| m.name.clone()).collect();
    for (k, s) in surfaces.iter().enumerate() {
        let on: Vec<usize> = match &s.screens {
            Screens::Number(n) => vec![*n],
            Screens::Named(want) => names.iter().enumerate().filter(|(_, n)| want.contains(n)).map(|(i, _)| i).collect(),
            Screens::All if !s.name.is_empty() => (0..monitors.len()).collect(),
            Screens::All => vec![0],
        };
        for which in on {
            let Some(m) = monitors.get(which) else { continue };
            let taken = s.name.is_empty() && m.screen.0.lock().unwrap().layers.iter().any(|l| l.main);
            if taken {
                continue;
            }
            id += 1;
            let layer = screen::layer(id, k, s, m.size);
            let size = (layer.rect[2] as u32, layer.rect[3] as u32);
            if !s.name.is_empty() {
                println!("session · the surface '{}' on {}: {}×{} at {},{}", s.name, m.name, size.0, size.1, layer.rect[0], layer.rect[1]);
            }
            m.screen.0.lock().unwrap().layers.push(layer);
            let _ = to_render.send(ToRender::Sheet(Box::new(NewSheet {
                id,
                target: Target::Frames(Box::new(LayerFrames::new(m.screen.clone(), id, size, to_render.clone()))),
                window: Box::new(LayerWindow { screen: m.screen.clone(), sheet: id, cursor: cursor_kind.clone() }),
                scale: 1.0,
                size,
                mhz: m.mhz,
                name: m.name.clone(),
                view: View { surface: k, popup: None, origin: s.origin, size: (size.0 as f32, size.1 as f32) },
            })));
        }
    }
    // A surface that changes level or edge while running (Marea's).
    {
        let screens: Vec<Screen> = monitors.iter().map(|m| m.screen.clone()).collect();
        let again = screens.clone();
        pleamar::provide_layer_hooks(pleamar::LayerHooks {
            relayer: Box::new(move |which, level| {
                for sc in &screens {
                    let mut st = sc.0.lock().unwrap();
                    let mut any = false;
                    for l in st.layers.iter_mut().filter(|l| l.surface == which) {
                        l.level = level;
                        any = true;
                    }
                    if any {
                        st.dirty = true;
                        sc.1.notify_all();
                    }
                }
            }),
            reanchor: Box::new(move |which, anchor| {
                for sc in &again {
                    let mut st = sc.0.lock().unwrap();
                    let size = st.size;
                    let mut any = false;
                    for l in st.layers.iter_mut().filter(|l| l.surface == which && !l.main) {
                        l.anchor = anchor;
                        l.rect = screen::place((l.rect[2] as u32, l.rect[3] as u32), anchor, l.margin, size);
                        any = true;
                    }
                    if any {
                        st.dirty = true;
                        sc.1.notify_all();
                    }
                }
            }),
        });
    }

    // The keyboard as the system has it set up, for the scene and for its windows.
    let keymap = keymap()?;
    pleamar::set_host_keymap(keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1));
    let keymap = xkb::State::new(&keymap);
    let _ = to_render.send(ToRender::KeyboardFocus(true));

    let mut libinput = Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
    libinput.udev_assign_seat(&seat).map_err(|()| "the input devices could not be taken")?;
    let input = LibinputInputBackend::new(libinput.clone());

    let mut event_loop: EventLoop<State> = EventLoop::try_new().map_err(|e| e.to_string())?;
    let h = event_loop.handle();
    h.insert_source(input, |event, _, state: &mut State| state.input(event)).map_err(|e| e.to_string())?;
    h.insert_source(notifier, |event, _, state: &mut State| state.session_event(event)).map_err(|e| e.to_string())?;
    h.insert_source(Generic::new(drm.clone(), Interest::READ, LoopMode::Level), |_, _, state: &mut State| {
        state.flipped();
        Ok(PostAction::Continue)
    })
    .map_err(|e| e.to_string())?;

    let first = monitors.first().map_or((0.0, 0.0), |m| (m.size.0 as f64 / 2.0, m.size.1 as f64 / 2.0));
    layers::register(monitors.iter().map(|m| (MonitorInfo { name: m.name.clone(), size: m.size, x: m.x, mhz: m.mhz }, m.screen.clone())).collect());
    let mut state = State { session, drm, monitors, libinput, to_render, keymap, pointer: first, cursor: None, scroll: 0.0, hit: Hit::Scene(None), grab: None, key_client: None, quit: false };
    state.make_cursor(&gbm);
    state.move_pointer(0.0, 0.0);
    println!("session · running: Ctrl+Alt+Backspace leaves");
    while !state.quit {
        event_loop.dispatch(Some(Duration::from_millis(500)), &mut state).map_err(|e| e.to_string())?;
    }
    println!("session · leaving");
    Ok(())
}

/// The system's keyboard layout: `XKB_DEFAULT_LAYOUT` if set, else what
/// `localectl` says the X11 layout is, else the default one.
fn keymap() -> Result<xkb::Keymap, String> {
    let from_env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let mut layout = from_env("XKB_DEFAULT_LAYOUT").unwrap_or_default();
    let mut variant = from_env("XKB_DEFAULT_VARIANT").unwrap_or_default();
    let options = from_env("XKB_DEFAULT_OPTIONS");
    if layout.is_empty() {
        if let Ok(out) = std::process::Command::new("localectl").arg("status").output() {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                if let Some(v) = line.trim().strip_prefix("X11 Layout:") {
                    layout = v.trim().to_owned();
                }
                if let Some(v) = line.trim().strip_prefix("X11 Variant:") {
                    variant = v.trim().to_owned();
                }
            }
        }
    }
    println!("session · keyboard: {}{}", if layout.is_empty() { "the default" } else { &layout }, if variant.is_empty() { String::new() } else { format!(" ({variant})") });
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    xkb::Keymap::new_from_names(&context, "", "", &layout, &variant, options, xkb::COMPILE_NO_FLAGS).ok_or_else(|| format!("the keyboard layout '{layout}' could not be read"))
}

impl State {
    /// The pointer moved by that much: across the monitors, left to right.
    fn move_pointer(&mut self, dx: f64, dy: f64) {
        let width: i32 = self.monitors.iter().map(|m| m.size.0 as i32).sum();
        let (px, py) = (self.pointer.0 + dx, self.pointer.1 + dy);
        let px = px.clamp(0.0, (width - 1).max(0) as f64);
        let on = self.monitors.iter().position(|m| px >= m.x as f64 && px < (m.x + m.size.0 as i32) as f64).unwrap_or(0);
        let py = py.clamp(0.0, self.monitors[on].size.1 as f64 - 1.0);
        self.pointer = (px, py);
        for (k, m) in self.monitors.iter().enumerate() {
            let (cx, cy) = if k == on { ((px - m.x as f64) as i32, py as i32) } else { (-64, -64) };
            #[allow(deprecated)]
            let _ = self.drm.move_cursor(m.crtc, (cx, cy));
        }
        let m = &self.monitors[on];
        let (mx, my) = (px - m.x as f64, py);
        let hit = {
            let st = m.screen.0.lock().unwrap();
            match self.grab {
                // Held: the one it was pressed on keeps it, wherever it goes.
                Some(id) => match st.clients.iter().find(|c| c.id == id) {
                    Some(c) => Hit::Client(id, (mx - c.rect[0] as f64, my - c.rect[1] as f64)),
                    None => self.hit,
                },
                None => screen::pointer_at(&st, (mx, my)),
            }
        };
        self.point(hit);
    }

    /// The pointer goes to whoever takes it now, and leaves whoever had it.
    fn point(&mut self, hit: Hit) {
        match hit {
            Hit::Scene(at) => {
                if matches!(self.hit, Hit::Client(..)) {
                    layers::tell(ToLayers::PointerOut);
                }
                let _ = self.to_render.send(ToRender::Pointer(at));
            }
            Hit::Client(id, (x, y)) => {
                if matches!(self.hit, Hit::Scene(Some(_))) {
                    let _ = self.to_render.send(ToRender::Pointer(None));
                }
                layers::tell(ToLayers::Pointer { id, x, y });
            }
        }
        self.hit = hit;
    }

    /// Where the keys go: a program's surface that takes all of it; else the
    /// one clicked that takes it on demand; else the scene.
    fn key_owner(&self) -> Option<u64> {
        let all = self.monitors.iter().find_map(|m| screen::keyboard_taker(&m.screen.0.lock().unwrap()));
        let alive = |id: u64| self.monitors.iter().any(|m| m.screen.0.lock().unwrap().clients.iter().any(|c| c.id == id && !c.pieces.is_empty()));
        all.or(self.key_client.filter(|id| alive(*id)))
    }

    /// An arrow for the pointer, on the card's cursor plane: moving the mouse
    /// does not repaint the scene.
    fn make_cursor(&mut self, gbm: &Arc<Mutex<gbm::Device<DrmDeviceFd>>>) {
        let Ok(mut bo) = gbm.lock().unwrap().create_buffer_object::<()>(64, 64, gbm::Format::Argb8888, gbm::BufferObjectFlags::CURSOR | gbm::BufferObjectFlags::WRITE) else {
            eprintln!("session · no cursor on the card: the pointer will not be seen");
            return;
        };
        let mut px = vec![0u8; 64 * 64 * 4];
        // The usual arrow: white, with a dark edge.
        let arrow: [&str; 19] = [
            "X", "XX", "X.X", "X..X", "X...X", "X....X", "X.....X", "X......X", "X.......X", "X........X", "X.........X", "X..........X", "X......XXXXX", "X...X..X", "X..XX..X",
            "X.X  X..X", "XX   X..X", "X     X..X", "      XXX",
        ];
        for (y, row) in arrow.iter().enumerate() {
            for (x, c) in row.chars().enumerate() {
                let v: [u8; 4] = match c {
                    'X' => [20, 20, 20, 255],
                    '.' => [255, 255, 255, 255],
                    _ => continue,
                };
                let i = (y * 64 + x) * 4;
                px[i..i + 4].copy_from_slice(&v);
            }
        }
        if bo.write(&px).is_err() {
            return;
        }
        for m in &self.monitors {
            #[allow(deprecated)]
            if let Err(e) = self.drm.set_cursor2(m.crtc, Some(&bo), (0, 0)) {
                eprintln!("session · {}: no cursor ({e})", m.name);
            }
        }
        self.cursor = Some(bo);
    }

    /// Page flips done: the frame on its way is on screen, and the monitor can
    /// be put together again.
    fn flipped(&mut self) {
        let Ok(events) = self.drm.receive_events() else { return };
        for e in events {
            if let DrmEvent::PageFlip(e) = e {
                for m in self.monitors.iter().filter(|m| m.crtc == e.crtc) {
                    {
                        let mut f = m.flips.lock().unwrap();
                        if let Some(p) = f.pending.take() {
                            f.on_screen = Some(p);
                        }
                    }
                    screen::landed(&m.screen, &self.to_render);
                }
            }
        }
    }

    fn session_event(&mut self, e: SessionEvent) {
        match e {
            SessionEvent::PauseSession => {
                println!("session · another TTY has the screen");
                self.libinput.suspend();
                for m in &self.monitors {
                    m.flips.lock().unwrap().pending = None;
                    let mut st = m.screen.0.lock().unwrap();
                    st.paused = true;
                    st.idle = true;
                    m.screen.1.notify_all();
                }
            }
            SessionEvent::ActivateSession => {
                println!("session · the screen is ours again");
                if self.libinput.resume().is_err() {
                    eprintln!("session · the input devices did not come back");
                }
                for m in &self.monitors {
                    let mut st = m.screen.0.lock().unwrap();
                    st.paused = false;
                    st.anew = true;
                    st.dirty = true;
                    m.screen.1.notify_all();
                }
                if let Some(bo) = self.cursor.take() {
                    for m in &self.monitors {
                        #[allow(deprecated)]
                        let _ = self.drm.set_cursor2(m.crtc, Some(&bo), (0, 0));
                    }
                    self.cursor = Some(bo);
                }
                let _ = self.to_render.send(ToRender::Repaint);
            }
        }
    }

    fn input(&mut self, event: InputEvent<LibinputInputBackend>) {
        match event {
            InputEvent::PointerMotion { event } => self.move_pointer(event.delta_x(), event.delta_y()),
            InputEvent::PointerMotionAbsolute { event } => {
                let width: i32 = self.monitors.iter().map(|m| m.size.0 as i32).sum();
                let height = self.monitors.first().map_or(1, |m| m.size.1 as i32);
                let p = event.position_transformed((width, height).into());
                let (dx, dy) = (p.x - self.pointer.0, p.y - self.pointer.1);
                self.move_pointer(dx, dy);
            }
            InputEvent::PointerButton { event } => {
                let down = event.state() == ButtonState::Pressed;
                if let Hit::Client(id, _) = self.hit {
                    if down {
                        self.grab = Some(id);
                        let takes = self.monitors.iter().any(|m| screen::takes_keyboard_on_click(&m.screen.0.lock().unwrap(), id));
                        self.set_key_client(if takes { Some(id) } else { None });
                    } else {
                        self.grab = None;
                    }
                    layers::tell(ToLayers::Button { code: event.button_code(), down });
                    if !down {
                        self.move_pointer(0.0, 0.0);
                    }
                    return;
                }
                if down {
                    self.set_key_client(None);
                }
                let b = match event.button_code() {
                    0x110 => 0,
                    0x111 => 1,
                    0x112 => 2,
                    _ => return,
                };
                let _ = self.to_render.send(ToRender::Button(b, event.state() == ButtonState::Pressed));
            }
            InputEvent::PointerAxis { event } => {
                // A wheel in notches; a touchpad, a notch every 15 px of finger.
                let notches = match (event.source(), event.amount_v120(Axis::Vertical), event.amount(Axis::Vertical)) {
                    (AxisSource::Wheel, Some(v), _) => -v / 120.0,
                    (_, _, Some(a)) => {
                        self.scroll += a;
                        let n = (self.scroll / 15.0).trunc();
                        self.scroll -= n * 15.0;
                        -n
                    }
                    _ => 0.0,
                };
                if notches != 0.0 {
                    if matches!(self.hit, Hit::Client(..)) {
                        layers::tell(ToLayers::Wheel(notches as f32));
                    } else {
                        let _ = self.to_render.send(ToRender::Wheel(notches as f32));
                    }
                }
            }
            InputEvent::Keyboard { event } => self.key(event.key_code(), event.state() == KeyState::Pressed),
            _ => {}
        }
    }

    fn key(&mut self, code: xkb::Keycode, down: bool) {
        let sym = self.keymap.key_get_one_sym(code);
        let name = xkb::keysym_get_name(sym);
        let typed = if down { Some(self.keymap.key_get_utf8(code)).filter(|t| !t.is_empty() && !t.chars().any(char::is_control)) } else { None };
        let active = |s: &xkb::State, m: &str| s.mod_name_is_active(m, xkb::STATE_MODS_EFFECTIVE);
        let mods = Mods { ctrl: active(&self.keymap, xkb::MOD_NAME_CTRL), alt: active(&self.keymap, xkb::MOD_NAME_ALT), shift: active(&self.keymap, xkb::MOD_NAME_SHIFT), logo: active(&self.keymap, xkb::MOD_NAME_LOGO) };
        self.keymap.update_key(code, if down { xkb::KeyDirection::Down } else { xkb::KeyDirection::Up });
        let evdev = code.raw().saturating_sub(8);
        if down {
            // The way out, whatever the scene is doing.
            if mods.ctrl && mods.alt && name == "BackSpace" {
                self.quit = true;
                return;
            }
            // Ctrl+Alt+F1…F12: the keymap already says it as «switch to VT n».
            if let Some(vt) = name.strip_prefix("XF86Switch_VT_").and_then(|n| n.parse::<i32>().ok()) {
                if let Err(e) = self.session.change_vt(vt) {
                    eprintln!("session · could not go to TTY {vt}: {e:?}");
                }
                return;
            }
            if let Some(id) = self.key_owner() {
                layers::tell(ToLayers::Key { id, code: evdev, down: true });
                return;
            }
            let _ = self.to_render.send(ToRender::Key(name, typed, mods, evdev));
        } else {
            // A key let go goes where it went down; to both, if that is not known.
            if let Some(id) = self.key_owner() {
                layers::tell(ToLayers::Key { id, code: evdev, down: false });
            }
            let _ = self.to_render.send(ToRender::KeyReleased(name, evdev));
        }
    }

    fn set_key_client(&mut self, id: Option<u64>) {
        if self.key_client.is_some() && id.is_none() {
            layers::tell(ToLayers::KeyboardBack);
        }
        self.key_client = id;
    }
}
