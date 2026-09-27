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
use crate::config;
use crate::layers::{self, MonitorInfo, ToLayers};
use crate::screen::{self, Hit, LayerFrames, LayerWindow, Output, Screen};
use pleamar::{NewSheet, Target, View};
use smithay::backend::drm::DrmDeviceFd;
use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, GestureBeginEvent, GestureEndEvent, GesturePinchUpdateEvent, GestureSwipeUpdateEvent, InputEvent, KeyState, KeyboardKeyEvent, PointerAxisEvent,
    PointerButtonEvent, PointerMotionEvent,
};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session as _};
use smithay::backend::udev::{all_gpus, primary_gpu, UdevBackend, UdevEvent};
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
        // Done: pleamar stops the render and leaves (see `provide_before_quit`).
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
    /// Where it is on the desktop: its top left corner.
    x: i32,
    y: i32,
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

    fn show(&mut self, which: usize, done: pleamar::Sent, device: &wgpu::Device, _: &wgpu::Queue, anew: bool) -> bool {
        // The monitor shows what is in the buffer when it flips: it has to be put together by then.
        done.wait(device, Duration::from_millis(100));
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
    /// The cursor's shapes on the card, each with its hot spot, and which is shown.
    cursors: Vec<(Cursor, gbm::BufferObject<()>, (i32, i32))>,
    shown: Option<Cursor>,
    /// What the scene asks for, and what a program's surface under the pointer does.
    scene_cursor: Cursor,
    program_cursor: Cursor,
    scroll: f64,
    /// A swipe on the touchpad under way: how many fingers, and how far they went;
    /// a pinch: how many, and how much bigger or smaller.
    swipe: Option<(u32, f64, f64)>,
    pinch: Option<(u32, f64)>,
    last_touch: std::time::Instant,
    /// Who has the pointer: the scene, or a program's surface (layer-shell).
    hit: Hit,
    /// The program's surface a button was pressed on: it keeps the pointer until it is let go.
    grab: Option<u64>,
    /// The program's surface that took the keyboard when clicked.
    key_client: Option<u64>,
    /// What is needed to put monitors up when they are plugged in: the card's
    /// buffers, the scene's surfaces, and the sheets given to the render.
    gbm: Arc<Mutex<gbm::Device<DrmDeviceFd>>>,
    surfaces: Vec<Surface>,
    cursor_kind: Arc<Mutex<Cursor>>,
    sheets: Vec<u32>,
    next_sheet: u32,
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
    // controller of its own, left to right as `PLEAMAR_MONITORS` says.
    let mut monitors: Vec<Monitor> = Vec::new();
    for (name, conn, mode, crtcs) in connected(&drm) {
        let used: Vec<crtc::Handle> = monitors.iter().map(|m| m.crtc).collect();
        let Some(crtc) = crtcs.into_iter().find(|c| !used.contains(c)) else { continue };
        monitors.push(make_monitor(&drm, &gbm, name, conn, mode, crtc));
    }
    if monitors.is_empty() {
        return Err("no monitor is connected".into());
    }
    place_monitors(&mut monitors);

    // The scene's surfaces on the monitors.
    let cursor_kind = Arc::new(Mutex::new(Cursor::Normal));
    let mut next_sheet = 1000;
    let sheets = give_sheets(&monitors, &surfaces, &to_render, &cursor_kind, &mut next_sheet);
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
                        st.changed_all = true;
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
                        st.changed_all = true;
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
    // How keys repeat, for the scene's fields (the programs are told by the compositor).
    let (rate, delay) = config::get().repeat();
    let _ = to_render.send(ToRender::KeyRepeat(Some((delay, (1000 / rate).max(1)))));

    let mut libinput = Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
    libinput.udev_assign_seat(&seat).map_err(|()| "the input devices could not be taken")?;
    let input = LibinputInputBackend::new(libinput.clone());

    let mut event_loop: EventLoop<State> = EventLoop::try_new().map_err(|e| e.to_string())?;
    let h = event_loop.handle();
    h.insert_source(input, |event, _, state: &mut State| state.input(event)).map_err(|e| e.to_string())?;
    // A monitor plugged in or out: the card's device changes.
    match UdevBackend::new(&seat) {
        Ok(udev) => {
            h.insert_source(udev, |event, _, state: &mut State| {
                if let UdevEvent::Changed { .. } = event {
                    state.rescan();
                }
            })
            .map_err(|e| e.to_string())?;
        }
        Err(e) => eprintln!("session · monitors plugged in later will not be seen ({e})"),
    }
    h.insert_source(notifier, |event, _, state: &mut State| state.session_event(event)).map_err(|e| e.to_string())?;
    h.insert_source(Generic::new(drm.clone(), Interest::READ, LoopMode::Level), |_, _, state: &mut State| {
        state.flipped();
        Ok(PostAction::Continue)
    })
    .map_err(|e| e.to_string())?;

    let first = monitors.first().map_or((0.0, 0.0), |m| (m.x as f64 + m.size.0 as f64 / 2.0, m.y as f64 + m.size.1 as f64 / 2.0));
    layers::set_card(drm.clone());
    layers::register(monitors.iter().map(|m| (MonitorInfo { name: m.name.clone(), size: m.size, x: m.x, y: m.y, mhz: m.mhz }, m.screen.clone())).collect());
    let mut state = State { session, drm, monitors, libinput, to_render, keymap, pointer: first, cursors: Vec::new(), shown: None, scene_cursor: Cursor::Normal, program_cursor: Cursor::Normal, scroll: 0.0, swipe: None, pinch: None, last_touch: std::time::Instant::now(), hit: Hit::Scene(None), grab: None, key_client: None, gbm: gbm.clone(), surfaces, cursor_kind, sheets, next_sheet, quit: false };
    state.make_cursors(&gbm);
    // The cursor the scene and the programs ask for, whenever it changes.
    let (cursor_tx, cursor_rx) = smithay::reexports::calloop::channel::channel::<(bool, Cursor)>();
    layers::set_cursor_sink(cursor_tx);
    event_loop
        .handle()
        .insert_source(cursor_rx, |event, _, state: &mut State| {
            if let smithay::reexports::calloop::channel::Event::Msg((from_program, c)) = event {
                if from_program {
                    state.program_cursor = c;
                } else {
                    state.scene_cursor = c;
                }
                state.show_cursor();
            }
        })
        .map_err(|e| e.to_string())?;
    state.move_pointer(0.0, 0.0);
    println!("session · running: Ctrl+Alt+Backspace leaves");
    while !state.quit {
        event_loop.dispatch(Some(Duration::from_millis(500)), &mut state).map_err(|e| e.to_string())?;
    }
    println!("session · leaving");
    Ok(())
}

/// The monitors connected to the card now and not turned off in the
/// configuration: their name (`DP-3`), connector, the mode asked for (or the
/// one they prefer) and the controllers that can drive them.
fn connected(drm: &DrmDeviceFd) -> Vec<(String, connector::Handle, Mode, Vec<crtc::Handle>)> {
    let Ok(res) = drm.resource_handles() else { return Vec::new() };
    let mut out = Vec::new();
    for &conn in res.connectors() {
        let Ok(info) = drm.get_connector(conn, true) else { continue };
        if info.state() != connector::State::Connected {
            continue;
        }
        let name = format!("{}-{}", info.interface().as_str(), info.interface_id());
        let rule = config::get().monitor(&name);
        if rule.is_some_and(|r| r.off) {
            continue;
        }
        let Some(mode) = choose_mode(info.modes(), rule.map(|r| &r.mode)) else { continue };
        let crtcs: Vec<crtc::Handle> = info.encoders().iter().filter_map(|e| drm.get_encoder(*e).ok()).flat_map(|e| res.filter_crtcs(e.possible_crtcs())).collect();
        out.push((name, conn, mode, crtcs));
    }
    out
}

/// Its refresh in mHz, exactly (`vrefresh` rounds 164.997 up to 165).
fn refresh_mhz(m: &Mode) -> i32 {
    let (h, v) = (m.hsync().2 as i64, m.vsync().2 as i64);
    if h == 0 || v == 0 {
        return m.vrefresh() as i32 * 1000;
    }
    (m.clock() as i64 * 1_000_000 / (h * v)) as i32
}

/// The mode asked for, among the ones the monitor has: that size at the
/// refresh closest to the one asked (the most it has, if none is asked), or
/// its biggest at its most, or the one it prefers.
fn choose_mode(modes: &[Mode], wish: Option<&config::ModeWish>) -> Option<Mode> {
    let preferred = modes.iter().find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED)).or(modes.first()).copied();
    match wish {
        Some(config::ModeWish::Exact(w, h, hz)) => {
            let same = modes.iter().filter(|m| m.size() == (*w as u16, *h as u16));
            let best = if *hz > 0.0 { same.min_by_key(|m| (refresh_mhz(m) - (hz * 1000.0) as i32).abs()) } else { same.max_by_key(|m| refresh_mhz(m)) };
            if best.is_none() {
                eprintln!("session · there is no {w}×{h} mode: the preferred one instead");
            }
            best.copied().or(preferred)
        }
        Some(config::ModeWish::Highest) => modes.iter().max_by_key(|m| (m.size().0 as u32 * m.size().1 as u32, refresh_mhz(m))).copied(),
        _ => preferred,
    }
}

/// A monitor put up: its buffers on the card and the one that puts it together.
fn make_monitor(drm: &DrmDeviceFd, gbm: &Arc<Mutex<gbm::Device<DrmDeviceFd>>>, name: String, conn: connector::Handle, mode: Mode, crtc: crtc::Handle) -> Monitor {
    let (w, h) = mode.size();
    println!("session · monitor {name}: {w}×{h} at {:.2} Hz", refresh_mhz(&mode) as f64 / 1000.0);
    let size = (w as u32, h as u32);
    let flips: Arc<Mutex<Flips>> = Default::default();
    let output = DrmOutput { drm: drm.clone(), gbm: gbm.clone(), connector: conn, crtc, mode, size, name: name.clone(), buffers: Vec::new(), failed: false, flips: flips.clone() };
    let screen = screen::screen(name.clone(), size, Box::new(output));
    Monitor { name, crtc, size, x: 0, y: 0, screen, flips, mhz: refresh_mhz(&mode) }
}

/// Where the configuration puts them; the ones it does not place, left to
/// right after them, as `PLEAMAR_MONITORS` says («DP-3,HDMI-A-1») and then in
/// the card's order. Numbered left to right, top to bottom.
fn place_monitors(monitors: &mut [Monitor]) {
    if let Ok(order) = std::env::var("PLEAMAR_MONITORS") {
        let names: Vec<&str> = order.split(',').map(str::trim).collect();
        monitors.sort_by_key(|m| names.iter().position(|n| *n == m.name).unwrap_or(names.len()));
    }
    let placed: Vec<Option<(i32, i32)>> = monitors.iter().map(|m| config::get().monitor(&m.name).and_then(|r| r.at)).collect();
    let mut x = monitors.iter().zip(&placed).filter_map(|(m, p)| p.map(|(px, _)| px + m.size.0 as i32)).max().unwrap_or(0);
    for (m, p) in monitors.iter_mut().zip(&placed) {
        (m.x, m.y) = match p {
            Some(at) => *at,
            None => {
                let at = (x, 0);
                x += m.size.0 as i32;
                at
            }
        };
    }
    monitors.sort_by_key(|m| (m.x, m.y));
    println!("session · monitors: {}", monitors.iter().map(|m| format!("{} at {},{}", m.name, m.x, m.y)).collect::<Vec<_>>().join(", "));
}

/// The scene's surfaces on the monitors, as sheets for the render. Its own:
/// its copies (`screens: each`) one per monitor, in order; without copies, on
/// the first. The named ones —a bar, a corner, a panel—: where they say, and
/// put together over or under the scene's by their level.
fn give_sheets(monitors: &[Monitor], surfaces: &[Surface], to_render: &Sender<ToRender>, cursor_kind: &Arc<Mutex<Cursor>>, next: &mut u32) -> Vec<u32> {
    let mut given = Vec::new();
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
            *next += 1;
            let id = *next;
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
            given.push(id);
        }
    }
    given
}

/// The keyboard layout: the configuration's (or Hyprland's), else
/// `XKB_DEFAULT_LAYOUT`, else what `localectl` says the X11 layout is, else
/// the default one.
fn keymap() -> Result<xkb::Keymap, String> {
    let from_env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let k = &config::get().keyboard;
    let mut layout = k.layout.clone().or_else(|| from_env("XKB_DEFAULT_LAYOUT")).unwrap_or_default();
    let mut variant = k.variant.clone().or_else(|| from_env("XKB_DEFAULT_VARIANT")).unwrap_or_default();
    let options = k.options.clone().or_else(|| from_env("XKB_DEFAULT_OPTIONS"));
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
    /// The monitors again, after one was plugged in or out: the ones that are
    /// still there go on as they were; a new one is put up, one that left is
    /// let go, and the scene's surfaces are given again for the new row
    /// (which copy is on which monitor may have changed).
    fn rescan(&mut self) {
        let now = connected(&self.drm);
        let names: Vec<&str> = now.iter().map(|c| c.0.as_str()).collect();
        let before: Vec<String> = self.monitors.iter().map(|m| m.name.clone()).collect();
        let (kept, gone): (Vec<Monitor>, Vec<Monitor>) = std::mem::take(&mut self.monitors).into_iter().partition(|m| names.contains(&m.name.as_str()));
        self.monitors = kept;
        for m in gone {
            println!("session · monitor {} unplugged", m.name);
            let mut st = m.screen.0.lock().unwrap();
            st.quit = true;
            drop(st);
            m.screen.1.notify_all();
            let _ = self.drm.set_crtc(m.crtc, None, (0, 0), &[], None);
        }
        for (name, conn, mode, crtcs) in now {
            if self.monitors.iter().any(|m| m.name == name) {
                continue;
            }
            let used: Vec<crtc::Handle> = self.monitors.iter().map(|m| m.crtc).collect();
            let Some(crtc) = crtcs.into_iter().find(|c| !used.contains(c)) else {
                eprintln!("session · {name}: no controller left to drive it");
                continue;
            };
            let m = make_monitor(&self.drm, &self.gbm, name, conn, mode, crtc);
            println!("session · monitor {} plugged in", m.name);
            self.monitors.push(m);
        }
        if self.monitors.iter().map(|m| m.name.clone()).collect::<Vec<_>>() == before {
            return;
        }
        place_monitors(&mut self.monitors);
        if self.monitors.is_empty() {
            return;
        }
        // The scene's surfaces, given again for the new row of monitors.
        for id in std::mem::take(&mut self.sheets) {
            let _ = self.to_render.send(ToRender::SheetGone(id));
        }
        for m in &self.monitors {
            let mut st = m.screen.0.lock().unwrap();
            st.layers.clear();
            st.changed_all = true;
            st.dirty = true;
        }
        self.sheets = give_sheets(&self.monitors, &self.surfaces, &self.to_render, &self.cursor_kind, &mut self.next_sheet);
        layers::register(self.monitors.iter().map(|m| (MonitorInfo { name: m.name.clone(), size: m.size, x: m.x, y: m.y, mhz: m.mhz }, m.screen.clone())).collect());
        layers::tell(ToLayers::Monitors);
        // The cursor on every monitor, and the pointer within them.
        self.shown = None;
        self.show_cursor();
    }

    /// The pointer moved by that much: across the monitors as they are
    /// placed. Off all of them, it stays at the edge of the nearest.
    fn move_pointer(&mut self, dx: f64, dy: f64) {
        let (mut px, mut py) = (self.pointer.0 + dx, self.pointer.1 + dy);
        // Confined by a program: inside its window.
        if let Some(layers::Hold::Confined(Some(r))) = layers::pointer_hold() {
            px = px.clamp(r[0] as f64, (r[0] + r[2]).max(r[0] + 1) as f64 - 1.0);
            py = py.clamp(r[1] as f64, (r[1] + r[3]).max(r[1] + 1) as f64 - 1.0);
        }
        let clamp = |m: &Monitor| (px.clamp(m.x as f64, (m.x + m.size.0 as i32) as f64 - 1.0), py.clamp(m.y as f64, (m.y + m.size.1 as i32) as f64 - 1.0));
        let Some((on, (px, py))) = self
            .monitors
            .iter()
            .enumerate()
            .map(|(k, m)| (k, clamp(m)))
            .min_by(|a, b| {
                let d = |(x, y): (f64, f64)| (x - px).powi(2) + (y - py).powi(2);
                d(a.1).total_cmp(&d(b.1))
            })
        else {
            return;
        };
        self.pointer = (px, py);
        let hot = self.shown.and_then(|c| self.cursors.iter().find(|x| x.0 == c)).map_or((0, 0), |x| x.2);
        for (k, m) in self.monitors.iter().enumerate() {
            let (cx, cy) = if k == on { ((px - m.x as f64) as i32 - hot.0, (py - m.y as f64) as i32 - hot.1) } else { (-256, -256) };
            #[allow(deprecated)]
            let _ = self.drm.move_cursor(m.crtc, (cx, cy));
        }
        let m = &self.monitors[on];
        let (mx, my) = (px - m.x as f64, py - m.y as f64);
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
        let was_program = matches!(self.hit, Hit::Client(..));
        self.hit = hit;
        if was_program != matches!(hit, Hit::Client(..)) {
            self.show_cursor();
        }
    }

    /// Where the keys go: a program's surface that takes all of it; else the
    /// one clicked that takes it on demand; else the scene.
    fn key_owner(&self) -> Option<u64> {
        let all = self.monitors.iter().find_map(|m| screen::keyboard_taker(&m.screen.0.lock().unwrap()));
        // Only while it still asks for it: a card that closes gives it back.
        let alive = |id: u64| self.monitors.iter().any(|m| m.screen.0.lock().unwrap().clients.iter().any(|c| c.id == id && c.keyboard != 0 && !c.pieces.is_empty()));
        all.or(self.key_client.filter(|id| alive(*id)))
    }

    /// The cursor's shapes, on the card's cursor plane: moving the mouse does
    /// not repaint anything. From the system's cursor theme (XCURSOR_THEME, or
    /// what ~/.icons/default inherits), at XCURSOR_SIZE (24); an arrow of its
    /// own if there is none.
    fn make_cursors(&mut self, gbm: &Arc<Mutex<gbm::Device<DrmDeviceFd>>>) {
        let theme = std::env::var("XCURSOR_THEME").ok().filter(|t| !t.is_empty()).or_else(|| {
            let home = std::env::var("HOME").ok()?;
            let text = std::fs::read_to_string(format!("{home}/.icons/default/index.theme")).ok()?;
            text.lines().find_map(|l| l.trim().strip_prefix("Inherits=")).map(|v| v.split(',').next().unwrap_or("").trim().to_owned())
        });
        let size: u32 = std::env::var("XCURSOR_SIZE").ok().and_then(|v| v.parse().ok()).unwrap_or(24);
        let names: [(Cursor, &[&str]); 5] = [
            (Cursor::Normal, &["default", "left_ptr", "arrow"]),
            (Cursor::Hand, &["pointer", "hand2", "pointing_hand", "hand1"]),
            (Cursor::Text, &["text", "xterm", "ibeam"]),
            (Cursor::Grab, &["grab", "openhand", "hand1"]),
            (Cursor::Grabbing, &["grabbing", "closedhand", "fleur"]),
        ];
        let loaded = theme.as_deref().map(xcursor::CursorTheme::load);
        for (kind, candidates) in names {
            let image = loaded.as_ref().and_then(|t| {
                candidates.iter().find_map(|n| {
                    let path = t.load_icon(n)?;
                    let data = std::fs::read(path).ok()?;
                    let images = xcursor::parser::parse_xcursor(&data)?;
                    // The size closest to the one asked for, that fits the plane.
                    images.into_iter().filter(|i| i.width <= 64 && i.height <= 64).min_by_key(|i| (i.size as i64 - size as i64).abs())
                })
            });
            let Ok(mut bo) = gbm.lock().unwrap().create_buffer_object::<()>(64, 64, gbm::Format::Argb8888, gbm::BufferObjectFlags::CURSOR | gbm::BufferObjectFlags::WRITE) else {
                eprintln!("session · no cursor on the card: the pointer will not be seen");
                return;
            };
            let mut px = vec![0u8; 64 * 64 * 4];
            let hot = match &image {
                Some(i) => {
                    for y in 0..i.height as usize {
                        let row = &i.pixels_rgba[y * i.width as usize * 4..(y + 1) * i.width as usize * 4];
                        px[y * 64 * 4..y * 64 * 4 + row.len()].copy_from_slice(row);
                    }
                    (i.xhot as i32, i.yhot as i32)
                }
                None => {
                    if kind != Cursor::Normal {
                        continue;
                    }
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
                    (0, 0)
                }
            };
            if bo.write(&px).is_err() {
                continue;
            }
            self.cursors.push((kind, bo, hot));
        }
        println!("session · cursor: {} ({} shapes)", theme.as_deref().unwrap_or("its own arrow"), self.cursors.len());
        self.show_cursor();
    }

    /// The cursor of whoever has the pointer, if it is not the one shown.
    fn show_cursor(&mut self) {
        let want = if matches!(self.hit, Hit::Client(..)) { self.program_cursor } else { self.scene_cursor };
        // A shape the theme lacks is shown as the arrow.
        let want = if self.cursors.iter().any(|c| c.0 == want) { want } else { Cursor::Normal };
        if self.shown == Some(want) {
            return;
        }
        let Some((_, bo, hot)) = self.cursors.iter().find(|c| c.0 == want) else { return };
        for m in &self.monitors {
            #[allow(deprecated)]
            if let Err(e) = self.drm.set_cursor2(m.crtc, Some(bo), *hot) {
                eprintln!("session · {}: no cursor ({e})", m.name);
            }
        }
        self.shown = Some(want);
        // Its tip where the pointer is.
        self.move_pointer(0.0, 0.0);
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
                    st.changed_all = true;
                    m.screen.1.notify_all();
                }
                // The cursor again: another TTY may have left its own.
                self.shown = None;
                self.show_cursor();
                let _ = self.to_render.send(ToRender::Repaint);
            }
        }
    }

    fn input(&mut self, event: InputEvent<LibinputInputBackend>) {
        // Anything but a device coming or going is someone there.
        if !matches!(event, InputEvent::DeviceAdded { .. } | InputEvent::DeviceRemoved { .. }) {
            self.touched();
        }
        match event {
            InputEvent::PointerMotion { event } => {
                // As it moved, for a program that locked the pointer (a game).
                layers::tell(ToLayers::Relative { dx: event.delta_x(), dy: event.delta_y(), ux: event.delta_x_unaccel(), uy: event.delta_y_unaccel(), utime: smithay::backend::input::Event::time(&event) });
                match layers::pointer_hold() {
                    // Locked: the pointer stays where it is; only the motion is told.
                    Some(layers::Hold::Locked) => {}
                    _ => self.move_pointer(event.delta_x(), event.delta_y()),
                }
            }
            InputEvent::PointerMotionAbsolute { event } => {
                // A tablet or a virtual machine's pointer: over all of the desktop.
                let x0 = self.monitors.iter().map(|m| m.x).min().unwrap_or(0);
                let y0 = self.monitors.iter().map(|m| m.y).min().unwrap_or(0);
                let x1 = self.monitors.iter().map(|m| m.x + m.size.0 as i32).max().unwrap_or(1);
                let y1 = self.monitors.iter().map(|m| m.y + m.size.1 as i32).max().unwrap_or(1);
                let p = event.position_transformed((x1 - x0, y1 - y0).into());
                let (dx, dy) = (p.x + x0 as f64 - self.pointer.0, p.y + y0 as f64 - self.pointer.1);
                self.move_pointer(dx, dy);
            }
            InputEvent::DeviceAdded { mut device } => set_up_device(&mut device),
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
            // Swipes and pinches with three or more fingers are the scene's, by
            // name: `swipe3_down`, `swipe4_left`, `pinch3_in`… (two are the
            // programs' scrolling). One it does not declare does nothing.
            InputEvent::GestureSwipeBegin { event } => self.swipe = Some((event.fingers(), 0.0, 0.0)),
            InputEvent::GestureSwipeUpdate { event } => {
                if let Some(s) = &mut self.swipe {
                    s.1 += event.delta_x();
                    s.2 += event.delta_y();
                }
            }
            InputEvent::GestureSwipeEnd { event } => {
                if let Some((fingers, dx, dy)) = self.swipe.take() {
                    if !event.cancelled() && fingers >= 3 && dx.abs().max(dy.abs()) > 60.0 {
                        let way = if dx.abs() > dy.abs() { if dx > 0.0 { "right" } else { "left" } } else if dy > 0.0 { "down" } else { "up" };
                        self.gesture(format!("swipe{fingers}_{way}"));
                    }
                }
            }
            InputEvent::GesturePinchBegin { event } => self.pinch = Some((event.fingers(), 1.0)),
            InputEvent::GesturePinchUpdate { event } => {
                if let Some(p) = &mut self.pinch {
                    p.1 = event.scale();
                }
            }
            InputEvent::GesturePinchEnd { event } => {
                if let Some((fingers, scale)) = self.pinch.take() {
                    if !event.cancelled() && fingers >= 3 && !(0.8..=1.25).contains(&scale) {
                        self.gesture(format!("pinch{fingers}_{}", if scale < 1.0 { "in" } else { "out" }));
                    }
                }
            }
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
            let owner = self.key_owner();
            // Locked, a key goes to the lock screen or nowhere: never to the
            // scene or its windows behind it.
            if owner.is_none() && layers::locked() {
                return;
            }
            // Shortcuts (never what is typed): where each went, to find out why one does nothing.
            if mods.ctrl || mods.alt || mods.logo {
                println!("session · key {}{}{}{name} → {}", if mods.ctrl { "Ctrl+" } else { "" }, if mods.alt { "Alt+" } else { "" }, if mods.logo { "Super+" } else { "" }, owner.map_or("the scene".to_owned(), |id| format!("the program's surface {id}")));
            }
            if let Some(id) = owner {
                layers::tell(ToLayers::Key { id, code: evdev, down: true });
                return;
            }
            let _ = self.to_render.send(ToRender::Key(name, typed, mods, evdev));
        } else {
            // A key let go goes where it went down; to both, if that is not known.
            if let Some(id) = self.key_owner() {
                layers::tell(ToLayers::Key { id, code: evdev, down: false });
            }
            if layers::locked() {
                return;
            }
            let _ = self.to_render.send(ToRender::KeyReleased(name, evdev));
        }
    }

    /// Someone is there: the compositor tells whoever watches for idleness
    /// (not more than a few times a second).
    fn touched(&mut self) {
        if self.last_touch.elapsed() > Duration::from_millis(200) {
            self.last_touch = std::time::Instant::now();
            layers::tell(ToLayers::Activity);
        }
    }

    /// A touchpad gesture, to the scene as the event of that name.
    fn gesture(&self, name: String) {
        if layers::locked() {
            return;
        }
        println!("session · gesture {name}");
        let _ = self.to_render.send(ToRender::ExternalSignal(pleamar::scene::intern(&name), None));
    }

    fn set_key_client(&mut self, id: Option<u64>) {
        if self.key_client.is_some() && id.is_none() {
            layers::tell(ToLayers::KeyboardBack);
        }
        self.key_client = id;
    }
}

/// A mouse or a touchpad as the configuration says: its acceleration and
/// speed, and on a touchpad, tapping, natural scrolling and ignoring it while
/// typing.
fn set_up_device(device: &mut smithay::reexports::input::Device) {
    use smithay::reexports::input::{AccelProfile, DeviceCapability};
    if !device.has_capability(DeviceCapability::Pointer) {
        return;
    }
    let touchpad = device.config_tap_finger_count() > 0;
    let c = config::get();
    let p = if touchpad { &c.touchpad } else { &c.pointer };
    match p.accel.as_deref() {
        Some("flat") => {
            let _ = device.config_accel_set_profile(AccelProfile::Flat);
        }
        Some("adaptive") => {
            let _ = device.config_accel_set_profile(AccelProfile::Adaptive);
        }
        _ => {}
    }
    if let Some(s) = p.speed {
        let _ = device.config_accel_set_speed(s.clamp(-1.0, 1.0));
    }
    if let Some(n) = p.natural {
        let _ = device.config_scroll_set_natural_scroll_enabled(n);
    }
    if touchpad {
        // Tapping on unless it is said otherwise, as most desktops do.
        let _ = device.config_tap_set_enabled(p.tap.unwrap_or(true));
        if let Some(d) = p.dwt {
            let _ = device.config_dwt_set_enabled(d);
        }
    }
    println!("session · {}: {}", device.name(), if touchpad { "a touchpad" } else { "a pointer" });
}
