//! A Wayland compositor inside the scene: `windows win max 6`.
//!
//! Other programs —a terminal, a browser— connect to it as they would to
//! Hyprland, and what they draw reaches the render as images, one per window.
//! Where each one goes, how big, how it arrives and how it leaves is the
//! scene's: here there is no layout, no animation and no decoration, only the
//! protocol. That is the point: the window manager is a `.plm` file, with
//! springs, rules and zones, and it reloads on save while the programs in it
//! keep running.
//!
//! It runs on its own thread with its own event loop (calloop), so a client
//! that floods it or stalls does not reach the render; the render does not
//! wait for it either, it paints the last image it has.
//!
//! What it does not do yet: dmabuf (programs that draw with the GPU are
//! started with Mesa's software GL, which hands over shared memory), XWayland,
//! and more than one scale.

use pleamar::scene::{DmabufPiece, NestEvent, PieceContent, ToNest, ToRender, WindowPiece};
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::{Buffer, Format, Fourcc, Modifier};
use smithay::delegate_dmabuf;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::wayland::compositor::{add_blocker, add_pre_commit_hook};
use smithay::wayland::dmabuf::{get_dmabuf, DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use std::collections::HashMap;
use smithay::delegate_compositor;
use smithay::delegate_cursor_shape;
use smithay::delegate_data_device;
use smithay::delegate_output;
use smithay::delegate_seat;
use smithay::delegate_shm;
use smithay::delegate_xdg_decoration;
use smithay::delegate_xdg_shell;
use smithay::desktop::{PopupKind, PopupManager};
use smithay::input::keyboard::{FilterResult, KeyboardHandle, XkbConfig};
use smithay::input::pointer::{AxisFrame, ButtonEvent, CursorImageStatus, MotionEvent, PointerHandle};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::calloop::channel::{self, Channel, Event as ChannelEvent};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_callback::WlCallback;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle, Resource};
use smithay::utils::{IsAlive, Logical, Point, Serial, Transform, SERIAL_COUNTER};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::tablet_manager::TabletSeatHandler;
use smithay::wayland::compositor::{
    get_parent, with_states, with_surface_tree_downward, BufferAssignment, CompositorClientState, CompositorHandler, CompositorState, SubsurfaceCachedState, SurfaceAttributes,
    TraversalAction,
};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::data_device::{ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::shell::xdg::decoration::{XdgDecorationHandler, XdgDecorationState};
use smithay::wayland::shell::xdg::{PopupSurface, PositionerState, SurfaceCachedState, ToplevelSurface, XdgShellHandler, XdgShellState, XdgToplevelSurfaceData};
use smithay::wayland::shm::{with_buffer_contents, ShmHandler, ShmState};
use smithay::wayland::socket::ListeningSocketSource;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What a surface last showed: its size, and either its pixels (BGRA,
/// premultiplied) or its buffer on the card. `changed`: the render does not
/// have it yet. `key` names the surface for the render, frame after frame.
#[derive(Default)]
struct Content {
    key: u64,
    size: (usize, usize),
    data: Vec<u8>,
    dmabuf: Option<(u64, Dmabuf)>,
    changed: bool,
}

struct Window {
    toplevel: ToplevelSurface,
    title: String,
    app: String,
    /// Where the window itself is inside its buffer: a program that draws
    /// its own shadow says where the shadow ends.
    geometry: [i32; 4],
    /// The surfaces the render was last told about: one that appears again
    /// is sent whole, even if it has not drawn anything new.
    sent: Vec<u64>,
    /// The monitor it is on: which copy of the scene lays it out.
    screen: usize,
}

/// Where the programs connect, and whether they can.
#[derive(Default)]
struct ClientState {
    compositor: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _: ClientId) {}
    fn disconnected(&self, _: ClientId, _: DisconnectReason) {}
}

struct State {
    dh: DisplayHandle,
    compositor: CompositorState,
    xdg: XdgShellState,
    /// Kept alive: without them their globals go away.
    _decorations: XdgDecorationState,
    _cursor_shapes: CursorShapeManagerState,
    shm: ShmState,
    seats: SeatState<State>,
    data_device: DataDeviceState,
    popups: PopupManager,
    _seat: Seat<State>,
    keyboard: KeyboardHandle<State>,
    pointer: PointerHandle<State>,
    output: Output,
    /// One per slot of the scene. A window that finds them all taken waits in `waiting`.
    slots: Vec<Option<Window>>,
    waiting: Vec<ToplevelSurface>,
    /// The order the scene lays them out in: the first is the one that leads.
    order: Vec<usize>,
    /// Where the next one goes: turning round, so that a slot just freed —whose
    /// image the scene may still be fading— is the last to be taken again.
    next_slot: usize,
    /// The size the scene wants for each slot, to answer a new window with it.
    asked: Vec<Option<(i32, i32)>>,
    focus: Option<usize>,
    /// The monitor the pointer is on: where a new window opens.
    on_screen: usize,
    /// Whether pleamar's own window has the keyboard: without it, no program does.
    host_focus: bool,
    pointer_on: Option<usize>,
    /// The windows whose image has to be made again after this round.
    dirty: Vec<WlSurface>,
    callbacks: Vec<WlCallback>,
    to_render: Sender<ToRender>,
    handle: LoopHandle<'static, State>,
    /// Frames on the card (linux-dmabuf), once the render has said what it can read.
    dmabuf: DmabufState,
    dmabuf_global: Option<DmabufGlobal>,
    /// Each program buffer by number, and the ones lent to the render until it has copied them.
    buffers: HashMap<ObjectId, u64>,
    lent: HashMap<u64, WlBuffer>,
    next_number: u64,
    socket: String,
    start: Instant,
    quit: bool,
}

/// Starts the compositor on its own thread. What it is told goes through the
/// returned sender; what it has to say arrives at the render as `ToRender::Nest`.
pub fn start(max: usize, to_render: Sender<ToRender>) -> Option<channel::Sender<ToNest>> {
    let (tx, rx) = channel::channel::<ToNest>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("windows".into())
        .spawn(move || {
            if let Err(e) = run(max, to_render, rx, ready_tx) {
                eprintln!("windows · the compositor stopped: {e}");
            }
        })
        .ok()?;
    ready_rx.recv_timeout(Duration::from_secs(3)).ok().flatten().map(|()| tx)
}

fn run(max: usize, to_render: Sender<ToRender>, rx: Channel<ToNest>, ready: std::sync::mpsc::Sender<Option<()>>) -> Result<(), String> {
    let mut event_loop: EventLoop<State> = EventLoop::try_new().map_err(|e| e.to_string())?;
    let display: Display<State> = Display::new().map_err(|e| e.to_string())?;
    let dh = display.handle();

    // A name of our own, not `wayland-1`: that one may be another compositor's,
    // and a program started by hand should not end up here by mistake.
    let (source, socket) = (1..64)
        .find_map(|k| {
            let name = format!("pleamar-{k}");
            ListeningSocketSource::with_name(&name).ok().map(|s| (s, name))
        })
        .ok_or("no free socket name")?;
    event_loop
        .handle()
        .insert_source(source, |stream, _, state: &mut State| {
            if let Err(e) = state.dh.insert_client(stream, Arc::new(ClientState::default())) {
                eprintln!("windows · a program could not connect: {e}");
            }
        })
        .map_err(|e| e.to_string())?;
    event_loop
        .handle()
        .insert_source(Generic::new(display, Interest::READ, Mode::Level), |_, display, state: &mut State| {
            // SAFETY: the display is not dropped while the loop runs: it lives in this source.
            unsafe {
                display.get_mut().dispatch_clients(state).map_err(std::io::Error::other)?;
            }
            Ok(PostAction::Continue)
        })
        .map_err(|e| e.to_string())?;
    event_loop
        .handle()
        .insert_source(rx, |event, _, state: &mut State| match event {
            ChannelEvent::Msg(m) => state.handle(m),
            ChannelEvent::Closed => state.quit = true,
        })
        .map_err(|e| e.to_string())?;

    let mut seats = SeatState::new();
    let mut seat = seats.new_wl_seat(&dh, "pleamar");
    // The keyboard as the user has it: the layout pleamar's own window was
    // given. If there is none yet, the system's default until it arrives.
    let keyboard = seat.add_keyboard(XkbConfig::default(), 400, 33).map_err(|e| e.to_string())?;
    let pointer = seat.add_pointer();
    let output = Output::new("pleamar".into(), PhysicalProperties { size: (0, 0).into(), subpixel: Subpixel::Unknown, make: "pleamar".into(), model: "windows".into() });
    let _global = output.create_global::<State>(&dh);
    let mode = OutputMode { size: (1280, 800).into(), refresh: 60_000 };
    output.change_current_state(Some(mode), Some(Transform::Normal), Some(Scale::Integer(1)), Some((0, 0).into()));
    output.set_preferred(mode);

    let mut state = State {
        compositor: CompositorState::new::<State>(&dh),
        xdg: XdgShellState::new::<State>(&dh),
        _decorations: XdgDecorationState::new::<State>(&dh),
        _cursor_shapes: CursorShapeManagerState::new::<State>(&dh),
        shm: ShmState::new::<State>(&dh, vec![]),
        data_device: DataDeviceState::new::<State>(&dh),
        popups: PopupManager::default(),
        seats,
        _seat: seat,
        keyboard,
        pointer,
        output,
        slots: (0..max).map(|_| None).collect(),
        waiting: Vec::new(),
        order: Vec::new(),
        next_slot: 0,
        asked: vec![None; max],
        focus: None,
        on_screen: 0,
        host_focus: true,
        pointer_on: None,
        dirty: Vec::new(),
        callbacks: Vec::new(),
        to_render,
        handle: event_loop.handle(),
        dmabuf: DmabufState::new(),
        dmabuf_global: None,
        buffers: HashMap::new(),
        lent: HashMap::new(),
        next_number: 0,
        socket: socket.clone(),
        start: Instant::now(),
        quit: false,
        dh,
    };
    if let Some(keymap) = pleamar::host_keymap() {
        let k = state.keyboard.clone();
        if let Err(e) = k.set_keymap_from_string(&mut state, keymap) {
            eprintln!("windows · the keyboard layout could not be copied: {e:?}");
        }
    }
    println!("windows · programs connect at WAYLAND_DISPLAY={socket}");
    let _ = state.to_render.send(ToRender::Nest(NestEvent::Socket(socket)));
    let _ = ready.send(Some(()));

    // A frame callback nobody answers leaves a program stopped: if the render
    // is not painting —a window that is not on the scene—, they are answered
    // anyway, slowly.
    let mut last_done = Instant::now();
    while !state.quit {
        event_loop.dispatch(Some(Duration::from_millis(100)), &mut state).map_err(|e| e.to_string())?;
        state.popups.cleanup();
        state.compose_dirty();
        if last_done.elapsed() > Duration::from_millis(250) && !state.callbacks.is_empty() {
            state.frame_done();
        }
        if !state.callbacks.is_empty() {
            last_done = last_done.min(Instant::now());
        } else {
            last_done = Instant::now();
        }
        let _ = state.dh.flush_clients();
    }
    Ok(())
}

impl State {
    fn time(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    fn tell(&self, e: NestEvent) {
        let _ = self.to_render.send(ToRender::Nest(e));
    }

    fn window_of(&self, surface: &WlSurface) -> Option<usize> {
        self.slots.iter().position(|w| w.as_ref().is_some_and(|w| w.toplevel.wl_surface() == surface))
    }

    fn handle(&mut self, m: ToNest) {
        if std::env::var_os("PLEAMAR_DEBUG_WINDOWS").is_some() && !matches!(m, ToNest::FrameDone) {
            eprintln!("windows · {m:?}");
        }
        let serial = SERIAL_COUNTER.next_serial();
        let time = self.time();
        match m {
            ToNest::Size(w, h) => {
                let mode = OutputMode { size: (w.max(1), h.max(1)).into(), refresh: 60_000 };
                self.output.change_current_state(Some(mode), None, None, None);
                self.output.set_preferred(mode);
            }
            ToNest::Configure { slot, w, h } => {
                if self.asked.get(slot) == Some(&Some((w, h))) {
                    return;
                }
                if let Some(a) = self.asked.get_mut(slot) {
                    *a = Some((w, h));
                }
                if let Some(Some(win)) = self.slots.get(slot) {
                    win.toplevel.with_pending_state(|s| s.size = Some((w.max(1), h.max(1)).into()));
                    if win.toplevel.is_initial_configure_sent() {
                        win.toplevel.send_pending_configure();
                    }
                }
            }
            ToNest::Pointer { slot, x, y } => {
                let Some(Some(win)) = self.slots.get(slot) else { return };
                let g = win.geometry;
                let at: Point<f64, Logical> = (g[0] as f64 + x, g[1] as f64 + y).into();
                let root = win.toplevel.wl_surface().clone();
                let under = self.surface_under(&root, g, at);
                self.pointer_on = Some(slot);
                let p = self.pointer.clone();
                p.motion(self, under.map(|(s, o)| (s, o.to_f64())), &MotionEvent { location: at, serial, time });
                p.frame(self);
            }
            ToNest::PointerOut => {
                if self.pointer_on.take().is_some() {
                    let p = self.pointer.clone();
                    p.motion(self, None, &MotionEvent { location: (0.0, 0.0).into(), serial, time });
                    p.frame(self);
                }
            }
            ToNest::Button { code, down } => {
                // Clicking outside a menu closes it, as it would on any desktop:
                // the program is told its popup is done.
                if down {
                    if let Some(slot) = self.pointer_on {
                        self.dismiss_popups_not_under(slot);
                        if self.focus != Some(slot) {
                            self.set_focus(Some(slot));
                        }
                    }
                }
                let p = self.pointer.clone();
                let state = if down { smithay::backend::input::ButtonState::Pressed } else { smithay::backend::input::ButtonState::Released };
                p.button(self, &ButtonEvent { serial, time, button: code, state });
                p.frame(self);
            }
            ToNest::Wheel(dy) => {
                let p = self.pointer.clone();
                // Up is positive in pleamar; in Wayland, down.
                let frame = AxisFrame::new(time)
                    .source(smithay::backend::input::AxisSource::Wheel)
                    .value(smithay::backend::input::Axis::Vertical, -dy * 15.0)
                    .v120(smithay::backend::input::Axis::Vertical, (-dy * 120.0) as i32);
                p.axis(self, frame);
                p.frame(self);
            }
            ToNest::Key { code, down } => {
                if self.focus.is_none() {
                    return;
                }
                // A key that arrives is pleamar's to give, whatever was said about its focus.
                if !self.host_focus {
                    self.handle(ToNest::HostFocus(true));
                }
                let k = self.keyboard.clone();
                let state = if down { smithay::backend::input::KeyState::Pressed } else { smithay::backend::input::KeyState::Released };
                k.input::<(), _>(self, (code + 8).into(), state, serial, time, |_, _, _| FilterResult::Forward);
            }
            ToNest::HostFocus(yes) => {
                self.host_focus = yes;
                let target = if yes { self.focus.and_then(|s| self.slots[s].as_ref()).map(|w| w.toplevel.wl_surface().clone()) } else { None };
                let k = self.keyboard.clone();
                k.set_focus(self, target, serial);
            }
            ToNest::Focus(slot) => self.set_focus(Some(slot)),
            ToNest::Close(slot) => {
                if let Some(Some(w)) = self.slots.get(slot) {
                    w.toplevel.send_close();
                }
            }
            ToNest::Promote(slot) => {
                if self.order.contains(&slot) {
                    self.order.retain(|s| *s != slot);
                    self.order.insert(0, slot);
                    self.tell(NestEvent::Order(self.order.clone()));
                }
            }
            ToNest::Launch(command) => self.launch(&command),
            ToNest::OnScreen(screen) => self.on_screen = screen,
            ToNest::Send(slot, screen) => {
                if let Some(Some(w)) = self.slots.get_mut(slot) {
                    if w.screen != screen {
                        w.screen = screen;
                        self.tell(NestEvent::Screen(slot, screen));
                    }
                }
            }
            ToNest::FrameDone => self.frame_done(),
            ToNest::Gpu { device, formats } => {
                if self.dmabuf_global.is_some() {
                    return;
                }
                let formats: Vec<Format> = formats.iter().filter_map(|(c, m)| Some(Format { code: Fourcc::try_from(*c).ok()?, modifier: Modifier::from(*m) })).collect();
                match DmabufFeedbackBuilder::new(device as libc::dev_t, formats).build() {
                    Ok(feedback) => {
                        self.dmabuf_global = Some(self.dmabuf.create_global_with_default_feedback::<State>(&self.dh, &feedback));
                        println!("windows · programs hand over their frames on the card");
                    }
                    Err(e) => eprintln!("windows · no frames on the card: {e}"),
                }
            }
            ToNest::Released(numbers) => {
                for n in numbers {
                    if let Some(b) = self.lent.remove(&n) {
                        b.release();
                    }
                }
            }
            ToNest::Quit => self.quit = true,
        }
    }

    /// A program, started so that it opens here and not on the desktop.
    fn launch(&self, command: &str) {
        let mut c = std::process::Command::new("sh");
        c.arg("-c").arg(command);
        c.env("WAYLAND_DISPLAY", &self.socket);
        // Without X: with DISPLAY a program that prefers X11 would open on the
        // real desktop instead of here.
        c.env_remove("DISPLAY");
        c.env("GDK_BACKEND", "wayland").env("QT_QPA_PLATFORM", "wayland").env("MOZ_ENABLE_WAYLAND", "1").env("SDL_VIDEODRIVER", "wayland");
        // Programs that draw with the GPU hand over their frames on the card
        // (dmabuf). If the card the scene is painted on cannot read them, with
        // Mesa's software GL they hand over shared memory instead.
        if self.dmabuf_global.is_none() {
            c.env("LIBGL_ALWAYS_SOFTWARE", "1");
            if std::path::Path::new("/usr/share/glvnd/egl_vendor.d/50_mesa.json").exists() {
                c.env("__EGL_VENDOR_LIBRARY_FILENAMES", "/usr/share/glvnd/egl_vendor.d/50_mesa.json");
            }
        }
        c.stdin(std::process::Stdio::null());
        match c.spawn() {
            Ok(mut child) => {
                // Reaped when it ends, so it does not linger as a zombie.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            Err(e) => eprintln!("windows · '{command}' could not start: {e}"),
        }
    }

    fn set_focus(&mut self, slot: Option<usize>) {
        let before = self.focus;
        self.focus = slot.filter(|s| self.slots.get(*s).is_some_and(Option::is_some));
        for (k, w) in self.slots.iter().enumerate() {
            let Some(w) = w else { continue };
            let active = Some(k) == self.focus;
            let changed = w.toplevel.with_pending_state(|s| {
                let has = s.states.contains(xdg_toplevel::State::Activated);
                if active && !has {
                    s.states.set(xdg_toplevel::State::Activated);
                } else if !active && has {
                    s.states.unset(xdg_toplevel::State::Activated);
                }
                has != active
            });
            if changed && w.toplevel.is_initial_configure_sent() {
                w.toplevel.send_pending_configure();
            }
        }
        let target = if self.host_focus { self.focus.and_then(|s| self.slots[s].as_ref()).map(|w| w.toplevel.wl_surface().clone()) } else { None };
        let k = self.keyboard.clone();
        k.set_focus(self, target, SERIAL_COUNTER.next_serial());
        if before != self.focus {
            self.tell(NestEvent::Focused(self.focus));
        }
    }

    /// The surface under a point of the window, in the window's surface
    /// coordinates: a menu first, then the window and its subsurfaces.
    fn surface_under(&self, root: &WlSurface, g: [i32; 4], at: Point<f64, Logical>) -> Option<(WlSurface, Point<i32, Logical>)> {
        let popups: Vec<(PopupKind, Point<i32, Logical>)> = PopupManager::popups_for_surface(root).collect();
        for (popup, offset) in popups.iter().rev() {
            let origin = Point::<i32, Logical>::from((g[0], g[1])) + *offset - popup.geometry().loc;
            if let Some(found) = hit_tree(popup.wl_surface(), at, (origin.x, origin.y)) {
                return Some(found);
            }
        }
        hit_tree(root, at, (0, 0))
    }

    fn dismiss_popups_not_under(&mut self, slot: usize) {
        let Some(Some(win)) = self.slots.get(slot) else { return };
        let root = win.toplevel.wl_surface().clone();
        let focused = self.pointer.current_focus();
        for (popup, _) in PopupManager::popups_for_surface(&root) {
            let inside = focused.as_ref().is_some_and(|f| {
                let mut s = f.clone();
                loop {
                    if &s == popup.wl_surface() {
                        break true;
                    }
                    match get_parent(&s) {
                        Some(p) => s = p,
                        None => break false,
                    }
                }
            });
            if !inside {
                if let PopupKind::Xdg(p) = &popup {
                    p.send_popup_done();
                }
            }
        }
    }

    fn frame_done(&mut self) {
        let t = self.time();
        for cb in self.callbacks.drain(..) {
            cb.done(t);
        }
    }

    /// A slot for a new window, if there is one free: turning round from the last one given.
    fn place(&mut self, toplevel: ToplevelSurface) {
        let n = self.slots.len();
        let Some(slot) = (0..n).map(|k| (self.next_slot + k) % n).find(|k| self.slots[*k].is_none()) else {
            self.waiting.push(toplevel);
            return;
        };
        self.next_slot = (slot + 1) % n.max(1);
        let (title, app) = with_states(toplevel.wl_surface(), |s| {
            let d = s.data_map.get::<XdgToplevelSurfaceData>().map(|d| d.lock().unwrap());
            d.map_or((String::new(), String::new()), |d| (d.title.clone().unwrap_or_default(), d.app_id.clone().unwrap_or_default()))
        });
        // Its size, the one the scene already has for that slot; tiled on all
        // sides, so it does not draw a shadow or round corners of its own: the
        // scene decides how it looks.
        let size = self.asked[slot];
        toplevel.with_pending_state(|s| {
            s.size = size.map(|(w, h)| (w.max(1), h.max(1)).into());
            for t in [xdg_toplevel::State::TiledLeft, xdg_toplevel::State::TiledRight, xdg_toplevel::State::TiledTop, xdg_toplevel::State::TiledBottom] {
                s.states.set(t);
            }
        });
        self.output.enter(toplevel.wl_surface());
        self.slots[slot] = Some(Window { toplevel, title: title.clone(), app: app.clone(), geometry: [0, 0, 0, 0], sent: Vec::new(), screen: self.on_screen });
        self.order.push(slot);
        self.tell(NestEvent::Opened { slot, title, app, screen: self.on_screen });
        self.tell(NestEvent::Order(self.order.clone()));
        self.set_focus(Some(slot));
    }

    fn forget(&mut self, toplevel: &ToplevelSurface) {
        self.waiting.retain(|t| t != toplevel);
        let Some(slot) = self.slots.iter().position(|w| w.as_ref().is_some_and(|w| &w.toplevel == toplevel)) else { return };
        self.slots[slot] = None;
        self.order.retain(|s| *s != slot);
        if self.pointer_on == Some(slot) {
            self.pointer_on = None;
        }
        self.tell(NestEvent::Closed(slot));
        self.tell(NestEvent::Order(self.order.clone()));
        if self.focus == Some(slot) {
            // The keyboard goes to the one that leads, as when a window closes anywhere.
            let next = self.order.first().copied();
            self.set_focus(next);
        }
        // One that was waiting takes its place.
        if !self.waiting.is_empty() {
            let t = self.waiting.remove(0);
            self.place(t);
        }
    }

    /// What every window touched in this round shows: its surface, its
    /// subsurfaces and its menus, each one a piece of its own, in order.
    fn compose_dirty(&mut self) {
        let dirty = std::mem::take(&mut self.dirty);
        let mut done: Vec<WlSurface> = Vec::new();
        for root in dirty {
            if done.contains(&root) || !root.alive() {
                continue;
            }
            done.push(root.clone());
            let Some(slot) = self.window_of(&root) else { continue };
            let Some((w, h)) = content_of(&root, |c| c.size) else { continue };
            if w == 0 || h == 0 {
                continue;
            }
            let g = with_states(&root, |s| s.cached_state.get::<SurfaceCachedState>().current().geometry);
            let g = g.map_or([0, 0, w as i32, h as i32], |r| [r.loc.x, r.loc.y, r.size.w, r.size.h]);
            let sent = self.slots[slot].as_ref().map(|w| w.sent.clone()).unwrap_or_default();
            let mut pieces = Vec::new();
            pieces_of(&root, (0, 0), &sent, &mut pieces);
            for (popup, offset) in PopupManager::popups_for_surface(&root) {
                let origin = Point::<i32, Logical>::from((g[0], g[1])) + offset - popup.geometry().loc;
                pieces_of(popup.wl_surface(), (origin.x, origin.y), &sent, &mut pieces);
            }
            if let Some(Some(win)) = self.slots.get_mut(slot) {
                win.geometry = g;
                win.sent = pieces.iter().map(|p| p.id).collect();
            }
            self.tell(NestEvent::Frame { slot, geometry: g, pieces });
        }
    }
}

/// Where a menu goes so that it fits inside its window, which is all of the
/// screen it can be seen on: flipped, slid or shrunk, as the program allows.
fn unconstrained(popup: &PopupSurface, positioner: PositionerState) -> smithay::utils::Rectangle<i32, Logical> {
    let kind = PopupKind::Xdg(popup.clone());
    let Ok(root) = smithay::desktop::find_popup_root_surface(&kind) else { return positioner.get_geometry() };
    let window = with_states(&root, |s| s.cached_state.get::<SurfaceCachedState>().current().geometry);
    let Some(window) = window else { return positioner.get_geometry() };
    // In the coordinates of the popup's parent: the window, seen from it.
    let parent = smithay::desktop::get_popup_toplevel_coords(&kind);
    let target = smithay::utils::Rectangle::new((-parent.x, -parent.y).into(), window.size);
    positioner.get_unconstrained_geometry(target)
}

/// What a surface keeps of its last buffer.
fn content_of<T>(s: &WlSurface, f: impl FnOnce(&Content) -> T) -> Option<T> {
    with_states(s, |st| st.data_map.get::<Mutex<Content>>().map(|p| f(&p.lock().unwrap())))
}

/// A surface and its subsurfaces as pieces, each at its place: with what it
/// shows if the render does not have it yet, or `Kept`.
fn pieces_of(root: &WlSurface, at: (i32, i32), sent: &[u64], out: &mut Vec<WindowPiece>) {
    with_surface_tree_downward(
        root,
        at,
        |s, states, &(x, y)| {
            let (dx, dy) = if s != root {
                let l = states.cached_state.get::<SubsurfaceCachedState>().current().location;
                (l.x, l.y)
            } else {
                (0, 0)
            };
            TraversalAction::DoChildren((x + dx, y + dy))
        },
        |s, states, &(x, y)| {
            let (dx, dy) = if s != root {
                let l = states.cached_state.get::<SubsurfaceCachedState>().current().location;
                (l.x, l.y)
            } else {
                (0, 0)
            };
            let Some(c) = states.data_map.get::<Mutex<Content>>() else { return };
            let mut c = c.lock().unwrap();
            if c.size.0 == 0 || c.size.1 == 0 {
                return;
            }
            let content = if c.changed || !sent.contains(&c.key) {
                c.changed = false;
                match &c.dmabuf {
                    Some((number, d)) => match dmabuf_piece(*number, d) {
                        Some(p) => PieceContent::Dmabuf(p),
                        None => PieceContent::Kept,
                    },
                    None => PieceContent::Pixels(c.data.clone()),
                }
            } else {
                PieceContent::Kept
            };
            out.push(WindowPiece { id: c.key, at: (x + dx, y + dy), size: (c.size.0 as u32, c.size.1 as u32), content });
        },
        |_, _, _| true,
    );
}

/// What the render needs to read a program's buffer on the card: its own copy
/// of the handle, and the layout.
fn dmabuf_piece(number: u64, d: &Dmabuf) -> Option<DmabufPiece> {
    let fd = d.handles().next()?.try_clone_to_owned().ok()?;
    Some(DmabufPiece {
        buffer: number,
        fd,
        fourcc: d.format().code as u32,
        modifier: d.format().modifier.into(),
        stride: d.strides().next()?,
        offset: d.offsets().next()?,
    })
}

/// The topmost surface of a tree under a point, and where that surface is:
/// measured against what each one last drew, which is what is seen.
fn hit_tree(root: &WlSurface, at: Point<f64, Logical>, origin: (i32, i32)) -> Option<(WlSurface, Point<i32, Logical>)> {
    let mut found = None;
    with_surface_tree_downward(
        root,
        origin,
        |s, states, &(x, y)| {
            let (dx, dy) = if s != root {
                let l = states.cached_state.get::<SubsurfaceCachedState>().current().location;
                (l.x, l.y)
            } else {
                (0, 0)
            };
            TraversalAction::DoChildren((x + dx, y + dy))
        },
        |s, states, &(x, y)| {
            let (dx, dy) = if s != root {
                let l = states.cached_state.get::<SubsurfaceCachedState>().current().location;
                (l.x, l.y)
            } else {
                (0, 0)
            };
            let (x, y) = (x + dx, y + dy);
            let Some(p) = states.data_map.get::<Mutex<Content>>() else { return };
            let (w, h) = p.lock().unwrap().size;
            if at.x >= x as f64 && at.y >= y as f64 && at.x < (x + w as i32) as f64 && at.y < (y + h as i32) as f64 {
                found = Some((s.clone(), Point::from((x, y))));
            }
        },
        |_, _, _| true,
    );
    found
}

/// A buffer's pixels, copied out of the program's memory so it can draw the next one.
fn read_buffer(buffer: &WlBuffer) -> Option<((usize, usize), Vec<u8>)> {
    with_buffer_contents(buffer, |ptr, len, d| {
        let (w, h, stride) = (d.width.max(0) as usize, d.height.max(0) as usize, d.stride.max(0) as usize);
        let opaque = match d.format {
            wl_shm::Format::Argb8888 => false,
            wl_shm::Format::Xrgb8888 => true,
            _ => return None,
        };
        let offset = d.offset.max(0) as usize;
        if offset + stride * h > len || stride < w * 4 {
            return None;
        }
        // SAFETY: smithay hands over the mapped pool, `len` bytes from `ptr`, and the range was checked.
        let src = unsafe { std::slice::from_raw_parts(ptr.add(offset), stride * h) };
        let mut data = Vec::with_capacity(w * h * 4);
        for row in src.chunks_exact(stride).take(h) {
            data.extend_from_slice(&row[..w * 4]);
        }
        if opaque {
            for px in data.chunks_exact_mut(4) {
                px[3] = 255;
            }
        }
        Some(((w, h), data))
    })
    .ok()
    .flatten()
}

// ── the protocols ────────────────────────────────────────────────

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor
    }

    /// A frame on the card is not taken until the program has finished
    /// drawing it: the commit waits for the buffer to be ready to read.
    fn new_surface(&mut self, surface: &WlSurface) {
        add_pre_commit_hook::<Self, _>(surface, |state, _, surface| {
            let pending = with_states(surface, |s| {
                s.cached_state.get::<SurfaceAttributes>().pending().buffer.as_ref().and_then(|a| match a {
                    BufferAssignment::NewBuffer(b) => get_dmabuf(b).cloned().ok(),
                    _ => None,
                })
            });
            let Some(d) = pending else { return };
            let Ok((blocker, source)) = d.generate_blocker(Interest::READ) else { return };
            let Some(client) = surface.client() else { return };
            let waiting = state.handle.insert_source(source, move |_, _, state: &mut State| {
                let dh = state.dh.clone();
                state.client_compositor_state(&client).blocker_cleared(state, &dh);
                Ok(())
            });
            if waiting.is_ok() {
                add_blocker(surface, blocker);
            }
        });
    }

    fn commit(&mut self, surface: &WlSurface) {
        let (buffer, callbacks) = with_states(surface, |states| {
            let mut guard = states.cached_state.get::<SurfaceAttributes>();
            let attrs = guard.current();
            (attrs.buffer.take(), std::mem::take(&mut attrs.frame_callbacks))
        });
        self.callbacks.extend(callbacks);
        // Each surface has its number for the render the first time it shows something.
        if !with_states(surface, |s| s.data_map.get::<Mutex<Content>>().is_some()) {
            self.next_number += 1;
            let key = self.next_number;
            with_states(surface, |s| {
                s.data_map.insert_if_missing_threadsafe(|| Mutex::new(Content { key, ..Content::default() }));
            });
        }
        match buffer {
            Some(BufferAssignment::NewBuffer(b)) => {
                let on_card = get_dmabuf(&b).ok().filter(|d| d.num_planes() == 1).cloned();
                let fresh = match on_card {
                    // On the card: lent to the render, and handed back once copied.
                    Some(d) => {
                        let number = match self.buffers.get(&b.id()) {
                            Some(n) => *n,
                            None => {
                                self.next_number += 1;
                                self.buffers.insert(b.id(), self.next_number);
                                self.next_number
                            }
                        };
                        let size = (d.width() as usize, d.height() as usize);
                        self.lent.insert(number, b);
                        Some((size, Vec::new(), Some((number, d))))
                    }
                    None => {
                        let p = read_buffer(&b);
                        b.release();
                        p.map(|(size, data)| (size, data, None))
                    }
                };
                let before = with_states(surface, |s| {
                    let mut c = s.data_map.get::<Mutex<Content>>().unwrap().lock().unwrap();
                    // One that never reached the render goes back now: a newer one takes its place.
                    let unsent = c.changed.then(|| c.dmabuf.as_ref().map(|(n, _)| *n)).flatten();
                    match fresh {
                        Some((size, data, dmabuf)) => {
                            c.size = size;
                            c.data = data;
                            c.dmabuf = dmabuf;
                        }
                        None => {
                            c.size = (0, 0);
                            c.data = Vec::new();
                            c.dmabuf = None;
                        }
                    }
                    c.changed = true;
                    unsent.filter(|n| c.dmabuf.as_ref().is_none_or(|(now, _)| now != n))
                });
                if let Some(b) = before.and_then(|n| self.lent.remove(&n)) {
                    b.release();
                }
            }
            Some(BufferAssignment::Removed) => with_states(surface, |s| {
                let mut c = s.data_map.get::<Mutex<Content>>().unwrap().lock().unwrap();
                c.size = (0, 0);
                c.data = Vec::new();
                c.dmabuf = None;
                c.changed = true;
            }),
            None => {}
        }
        self.popups.commit(surface);
        // Its root: the window it belongs to, or the menu.
        let mut root = surface.clone();
        while let Some(p) = get_parent(&root) {
            root = p;
        }
        // A window says nothing until it is answered: the first configure goes on its first commit.
        if let Some(slot) = self.window_of(&root) {
            let t = self.slots[slot].as_ref().unwrap().toplevel.clone();
            if !t.is_initial_configure_sent() {
                t.send_configure();
            }
            self.dirty.push(root);
        } else if let Some(PopupKind::Xdg(p)) = self.popups.find_popup(&root) {
            if !p.is_initial_configure_sent() {
                let _ = p.send_configure();
            }
            // A menu is drawn inside its window: that window's image changes.
            if let Ok(owner) = smithay::desktop::find_popup_root_surface(&PopupKind::Xdg(p)) {
                self.dirty.push(owner);
            }
        } else if let Some(t) = self.waiting.iter().find(|t| t.wl_surface() == &root).cloned() {
            if !t.is_initial_configure_sent() {
                t.send_configure();
            }
        }
    }
}

impl BufferHandler for State {
    /// A program's buffer is gone: what the render kept of it goes too.
    fn buffer_destroyed(&mut self, buffer: &WlBuffer) {
        if let Some(n) = self.buffers.remove(&buffer.id()) {
            self.lent.remove(&n);
            self.tell(NestEvent::Forget(vec![n]));
        }
    }
}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf
    }

    /// Accepted as it is: the render reads it when it is shown. One plane only, for now.
    fn dmabuf_imported(&mut self, _: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
        if dmabuf.num_planes() == 1 {
            let _ = notifier.successful::<State>();
        } else {
            notifier.failed();
        }
    }
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self.place(surface);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.forget(&surface);
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        let Some(slot) = self.window_of(surface.wl_surface()) else { return };
        let title = with_states(surface.wl_surface(), |s| s.data_map.get::<XdgToplevelSurfaceData>().and_then(|d| d.lock().unwrap().title.clone())).unwrap_or_default();
        if let Some(Some(w)) = self.slots.get_mut(slot) {
            w.title = title.clone();
        }
        self.tell(NestEvent::Title(slot, title));
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        let Some(slot) = self.window_of(surface.wl_surface()) else { return };
        let app = with_states(surface.wl_surface(), |s| s.data_map.get::<XdgToplevelSurfaceData>().and_then(|d| d.lock().unwrap().app_id.clone())).unwrap_or_default();
        if let Some(Some(w)) = self.slots.get_mut(slot) {
            w.app = app.clone();
        }
        self.tell(NestEvent::App(slot, app));
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        let geometry = unconstrained(&surface, positioner);
        surface.with_pending_state(|s| s.geometry = geometry);
        if let Err(e) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            eprintln!("windows · a menu could not be tracked: {e:?}");
        }
    }

    fn grab(&mut self, _: PopupSurface, _: WlSeat, _: Serial) {
        // No grab: a click outside the menu closes it (see `dismiss_popups_not_under`).
    }

    fn reposition_request(&mut self, surface: PopupSurface, positioner: PositionerState, token: u32) {
        let geometry = unconstrained(&surface, positioner);
        surface.with_pending_state(|s| {
            s.geometry = geometry;
            s.positioner = positioner;
        });
        surface.send_repositioned(token);
    }
}

impl XdgDecorationHandler for State {
    // The scene draws the frame: the programs are asked not to.
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|s| s.decoration_mode = Some(DecorationMode::ServerSide));
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }
    fn request_mode(&mut self, toplevel: ToplevelSurface, _: DecorationMode) {
        self.new_decoration(toplevel);
    }
    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.new_decoration(toplevel);
    }
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<State> {
        &mut self.seats
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        // What is copied goes with the keyboard: the focused program is offered it.
        let client = focused.and_then(|s| self.dh.get_client(s.id()).ok());
        smithay::wayland::selection::data_device::set_data_device_focus(&self.dh, seat, client);
    }

    /// The cursor the program asks for, by its name: pleamar draws its own of
    /// the same kind. One drawn by the program itself is taken as the arrow.
    fn cursor_image(&mut self, _: &Seat<Self>, image: CursorImageStatus) {
        use smithay::input::pointer::CursorIcon as I;
        let kind = match image {
            CursorImageStatus::Named(I::Text | I::VerticalText) => pleamar::scene::Cursor::Text,
            CursorImageStatus::Named(I::Pointer) => pleamar::scene::Cursor::Hand,
            CursorImageStatus::Named(I::Grab) => pleamar::scene::Cursor::Grab,
            CursorImageStatus::Named(I::Grabbing | I::Move) => pleamar::scene::Cursor::Grabbing,
            _ => pleamar::scene::Cursor::Normal,
        };
        self.tell(NestEvent::Cursor(kind));
    }
}

impl SelectionHandler for State {
    type SelectionUserData = ();
}

impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device
    }
}

impl ClientDndGrabHandler for State {}
impl ServerDndGrabHandler for State {}

impl OutputHandler for State {}
impl TabletSeatHandler for State {}

delegate_compositor!(State);
delegate_shm!(State);
delegate_xdg_shell!(State);
delegate_xdg_decoration!(State);
delegate_seat!(State);
delegate_data_device!(State);
delegate_output!(State);
delegate_cursor_shape!(State);
delegate_dmabuf!(State);
