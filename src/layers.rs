//! What the compositor inside and the monitors share when there are monitors
//! of our own (the session, or headless): which monitors there are, and the
//! programs' layer-shell surfaces —Marea, a bar, a wallpaper— that are put
//! together on them next to the scene's.
//!
//! The compositor tells a monitor what a program's surface shows
//! (`show`); the monitor reads it straight from the program's buffer when it
//! puts itself together, and hands the buffer back once it no longer needs it
//! (`ToLayers::Released`). The input on those surfaces goes from the session to
//! the compositor directly, without passing through the scene.

use crate::screen::Screen;
use pleamar::scene::PieceContent;
use smithay::reexports::calloop::channel;
use std::sync::Mutex;

/// A monitor as the programs see it: its name (`DP-3`), size, where it is on
/// the desktop (its top left corner), and its refresh in mHz.
#[derive(Clone, Debug)]
pub struct MonitorInfo {
    pub name: String,
    pub size: (u32, u32),
    pub x: i32,
    pub y: i32,
    pub mhz: i32,
}

/// What the session and the monitors tell the compositor inside.
#[derive(Debug)]
pub enum ToLayers {
    /// The pointer on a program's surface, in its coordinates.
    Pointer { id: u64, x: f64, y: f64 },
    /// No longer on any.
    PointerOut,
    /// A button (the evdev code) or the wheel, on the one under the pointer.
    Button { code: u32, down: bool },
    Wheel(f32),
    /// A key for that one: it takes the keyboard if it did not have it.
    Key { id: u64, code: u32, down: bool },
    /// The keyboard goes back to the windows.
    KeyboardBack,
    /// A monitor was put together with what the programs had drawn: they may draw again.
    FrameDone,
    /// Buffers a monitor no longer reads.
    Released(Vec<u64>),
    /// The monitors changed (one was plugged in or out): see `monitors()`.
    Monitors,
    /// A picture a program asked for (wlr-screencopy): its pixels, BGRA,
    /// row after row with no padding; none if it could not be taken.
    Captured { id: u64, pixels: Option<Vec<u8>> },
    /// The mouse moved, as it moved (a game that locked the pointer reads
    /// this): accelerated and not, and when, in µs.
    Relative { dx: f64, dy: f64, ux: f64, uy: f64, utime: u64 },
    /// Someone touched something: not idle.
    Activity,
    /// Monitors went on or off (see `powered`).
    Power,
}

/// What a program holding the pointer asks of it: that it stays where it is
/// (a game looking around), or inside its window (on the desktop, x, y, w, h).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hold {
    Locked,
    Confined(Option<[i32; 4]>),
}

static HOLD: Mutex<Option<Hold>> = Mutex::new(None);

/// Which monitors have a fullscreen window: there, other programs' bars step aside.
pub fn set_fullscreen(on: &[bool]) {
    for (k, (_, sc)) in MONITORS.lock().unwrap().iter().enumerate() {
        let yes = on.get(k).copied().unwrap_or(false);
        let mut st = sc.0.lock().unwrap();
        if st.fullscreen != yes {
            st.fullscreen = yes;
            st.dirty = true;
            st.changed_all = true;
            sc.1.notify_all();
        }
    }
}

pub fn set_pointer_hold(h: Option<Hold>) {
    *HOLD.lock().unwrap() = h;
}

pub fn pointer_hold() -> Option<Hold> {
    *HOLD.lock().unwrap()
}

/// Whether a program keeps the screen awake (idle-inhibit: a video playing).
static INHIBITED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_inhibited(yes: bool) {
    INHIBITED.store(yes, std::sync::atomic::Ordering::Relaxed);
}

pub fn inhibited() -> bool {
    INHIBITED.load(std::sync::atomic::Ordering::Relaxed)
}

/// A program's surface on a monitor: where, at what level, and what it shows.
pub struct ClientLayer {
    pub id: u64,
    /// 0 background, 1 bottom, 2 top, 3 overlay, 4 the lock screen.
    pub level: u8,
    /// Where it is on the monitor.
    pub rect: [i32; 4],
    /// It and what hangs from it (subsurfaces, menus), from its corner.
    pub pieces: Vec<ClientPiece>,
    /// Where it takes the pointer, from its corner: in order, added or taken
    /// away (`true` adds). `None` is all of it.
    pub region: Option<Vec<(bool, [i32; 4])>>,
    /// 0 never takes the keyboard, 1 takes all of it, 2 takes it when clicked.
    pub keyboard: u8,
    /// Where what is behind it is shown blurred (ext-background-effect), from its corner.
    pub blur: Vec<[i32; 4]>,
    /// Which program it is (0: none known), so that its own changes do not
    /// count as «something changed behind» for its own pictures.
    pub owner: u64,
}

pub struct ClientPiece {
    /// The program's surface it comes from.
    pub key: u64,
    pub at: (i32, i32),
    pub size: (u32, u32),
    /// What it shows, if it is new; the monitor takes it when it puts itself together.
    pub content: Option<PieceContent>,
    /// The buffer on the card it shows, if it is one.
    pub buffer: Option<u64>,
    /// Without alpha (XRGB): whatever the alpha byte holds, it covers.
    pub opaque: bool,
}

impl ClientLayer {
    /// Whether it takes the pointer at that point of the monitor.
    pub fn takes(&self, x: f64, y: f64) -> bool {
        let (lx, ly) = (x - self.rect[0] as f64, y - self.rect[1] as f64);
        let inside = |r: &[i32; 4]| lx >= r[0] as f64 && ly >= r[1] as f64 && lx < (r[0] + r[2]) as f64 && ly < (r[1] + r[3]) as f64;
        let Some(root) = self.pieces.first() else { return false };
        // A menu hanging from it takes it wherever it is drawn.
        if self.pieces.iter().skip(1).any(|p| inside(&[p.at.0, p.at.1, p.size.0 as i32, p.size.1 as i32])) {
            return true;
        }
        if !inside(&[root.at.0, root.at.1, root.size.0 as i32, root.size.1 as i32]) {
            return false;
        }
        match &self.region {
            None => true,
            Some(rects) => rects.iter().fold(false, |on, (add, r)| if inside(r) { *add } else { on }),
        }
    }
}

static MONITORS: Mutex<Vec<(MonitorInfo, Screen)>> = Mutex::new(Vec::new());
/// Whether monitors of our own are coming (a session, or headless): the
/// compositor inside may start before they are known, and has to wait for them.
static EXPECTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn expect_monitors() {
    EXPECTED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// The monitors, waiting a moment for them if they are coming and not here yet.
pub fn wait_monitors(most: std::time::Duration) -> Vec<MonitorInfo> {
    let start = std::time::Instant::now();
    while EXPECTED.load(std::sync::atomic::Ordering::Relaxed) && MONITORS.lock().unwrap().is_empty() && start.elapsed() < most {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    monitors()
}
static NEST: Mutex<Option<channel::Sender<ToLayers>>> = Mutex::new(None);
/// The card the session drives, for the compositor inside to import the
/// programs' sync points with (explicit sync).
static CARD: Mutex<Option<smithay::backend::drm::DrmDeviceFd>> = Mutex::new(None);
/// Where the cursor asked for goes: the session, which has the card's cursor
/// plane. `true` when a program's surface asks, `false` when the scene does.
static CURSOR: Mutex<Option<channel::Sender<(bool, pleamar::scene::Cursor)>>> = Mutex::new(None);

pub fn set_cursor_sink(tx: channel::Sender<(bool, pleamar::scene::Cursor)>) {
    *CURSOR.lock().unwrap() = Some(tx);
}

pub fn cursor(from_program: bool, c: pleamar::scene::Cursor) {
    if let Some(tx) = CURSOR.lock().unwrap().as_ref() {
        let _ = tx.send((from_program, c));
    }
}

/// Monitors turned on or off, as a program asks (wlr-output-power-management:
/// hypridle, swayidle, wlopm): which one (all, if none), and on or off.
static POWER: Mutex<Option<channel::Sender<(Option<usize>, bool)>>> = Mutex::new(None);

pub fn set_power_sink(tx: channel::Sender<(Option<usize>, bool)>) {
    *POWER.lock().unwrap() = Some(tx);
}

pub fn request_power(monitor: Option<usize>, on: bool) {
    if let Some(tx) = POWER.lock().unwrap().as_ref() {
        let _ = tx.send((monitor, on));
    }
}

/// Which monitors are on, as the session last said.
static POWERED: Mutex<Vec<bool>> = Mutex::new(Vec::new());

pub fn set_powered(on: Vec<bool>) {
    *POWERED.lock().unwrap() = on;
}

pub fn powered(monitor: usize) -> bool {
    POWERED.lock().unwrap().get(monitor).copied().unwrap_or(true)
}

pub fn set_card(fd: smithay::backend::drm::DrmDeviceFd) {
    *CARD.lock().unwrap() = Some(fd);
}

pub fn card() -> Option<smithay::backend::drm::DrmDeviceFd> {
    CARD.lock().unwrap().clone()
}
/// Whether a lock screen holds the session (ext-session-lock).
static LOCKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn locked() -> bool {
    LOCKED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Locked, the monitors show only the lock screen's surfaces; they are all
/// put together again at once.
pub fn set_locked(yes: bool) {
    LOCKED.store(yes, std::sync::atomic::Ordering::Relaxed);
    for (_, sc) in MONITORS.lock().unwrap().iter() {
        let mut st = sc.0.lock().unwrap();
        st.dirty = true;
        st.changed_all = true;
        drop(st);
        sc.1.notify_all();
    }
}

/// The monitors of the session, left to right.
pub fn register(monitors: Vec<(MonitorInfo, Screen)>) {
    *MONITORS.lock().unwrap() = monitors;
}

pub fn monitors() -> Vec<MonitorInfo> {
    MONITORS.lock().unwrap().iter().map(|(m, _)| m.clone()).collect()
}

pub fn set_nest(tx: channel::Sender<ToLayers>) {
    *NEST.lock().unwrap() = Some(tx);
}

/// Tells the compositor inside, if there is one.
pub fn tell(m: ToLayers) {
    if let Some(tx) = NEST.lock().unwrap().as_ref() {
        let _ = tx.send(m);
    }
}

/// A program's surface shows this now on that monitor (and on no other).
/// What the monitor has not read yet and is still shown is carried over; a
/// buffer it never got to read goes back. The ones it did read, it hands back
/// itself once it no longer shows them.
pub fn show(monitor: usize, layer: ClientLayer) {
    let mut unread = Vec::new();
    let mut layer = Some(layer);
    let id = layer.as_ref().map_or(0, |l| l.id);
    for (k, (_, sc)) in MONITORS.lock().unwrap().iter().enumerate() {
        let (lock, cv) = &**sc;
        let mut st = lock.lock().unwrap();
        let before = st.clients.iter().position(|c| c.id == id);
        let old = before.map(|i| st.clients.remove(i));
        let mut new = if k == monitor { layer.take() } else { None };
        if old.is_none() && new.is_none() {
            continue;
        }
        // What changes on the monitor: where it was and where it is, if it
        // moved, grew, changed level or came or went; else only the pieces
        // with something new.
        let same_place = match (&old, &new) {
            (Some(o), Some(n)) => o.rect == n.rect && o.level == n.level && o.pieces.len() == n.pieces.len() && o.pieces.iter().zip(&n.pieces).all(|(a, b)| a.at == b.at && a.size == b.size),
            _ => false,
        };
        if same_place {
            if let Some(n) = &new {
                for p in n.pieces.iter().filter(|p| p.content.is_some()) {
                    st.changed.push(([n.rect[0] + p.at.0, n.rect[1] + p.at.1, p.size.0 as i32, p.size.1 as i32], n.owner));
                }
            }
        } else {
            for l in old.iter().chain(new.iter()) {
                for p in &l.pieces {
                    st.changed.push(([l.rect[0] + p.at.0, l.rect[1] + p.at.1, p.size.0 as i32, p.size.1 as i32], l.owner));
                }
            }
        }
        for p in old.into_iter().flat_map(|o| o.pieces) {
            let Some(content) = p.content else { continue };
            if let Some(n) = new.as_mut().and_then(|n| n.pieces.iter_mut().find(|n| n.key == p.key && n.buffer == p.buffer && n.content.is_none())) {
                n.content = Some(content);
                continue;
            }
            if let (PieceContent::Dmabuf(_), Some(b)) = (&content, p.buffer) {
                if !st.held.contains(&b) {
                    unread.push(b);
                }
            }
        }
        if let Some(n) = new {
            let at = before.unwrap_or(st.clients.len()).min(st.clients.len());
            st.clients.insert(at, n);
        }
        st.dirty = true;
        cv.notify_all();
    }
    if !unread.is_empty() {
        tell(ToLayers::Released(unread));
    }
}

/// A program's surface is gone.
pub fn hide(id: u64) {
    let mut unread = Vec::new();
    for (_, sc) in MONITORS.lock().unwrap().iter() {
        let (lock, cv) = &**sc;
        let mut st = lock.lock().unwrap();
        if let Some(i) = st.clients.iter().position(|c| c.id == id) {
            let old = st.clients.remove(i);
            for p in &old.pieces {
                st.changed.push(([old.rect[0] + p.at.0, old.rect[1] + p.at.1, p.size.0 as i32, p.size.1 as i32], old.owner));
            }
            for p in old.pieces {
                if let (Some(PieceContent::Dmabuf(_)), Some(b)) = (&p.content, p.buffer) {
                    if !st.held.contains(&b) {
                        unread.push(b);
                    }
                }
            }
            st.dirty = true;
            cv.notify_all();
        }
    }
    if !unread.is_empty() {
        tell(ToLayers::Released(unread));
    }
}

/// A picture of that piece of that monitor (x, y, w, h), for a program:
/// taken the next time it is put together.
/// With `on_change` (copy_with_damage), not before something in that piece
/// changes: it does not make the monitor be put together, it waits for it.
pub fn capture(monitor: usize, id: u64, piece: [i32; 4], on_change: bool, owner: u64) {
    let monitors = MONITORS.lock().unwrap();
    let Some((_, sc)) = monitors.get(monitor) else {
        drop(monitors);
        tell(ToLayers::Captured { id, pixels: None });
        return;
    };
    let (lock, cv) = &**sc;
    let mut st = lock.lock().unwrap();
    st.captures.push((id, piece, on_change, owner));
    if !on_change {
        st.dirty = true;
        cv.notify_all();
    }
}

/// A picture no longer wanted: its program let it go before it was taken.
pub fn uncapture(id: u64) {
    for (_, sc) in MONITORS.lock().unwrap().iter() {
        sc.0.lock().unwrap().captures.retain(|c| c.0 != id);
    }
}

/// The monitors stop putting themselves together (the process is leaving):
/// each finishes what it is doing with the card and ends.
pub fn stop_all() {
    for (_, sc) in MONITORS.lock().unwrap().iter() {
        sc.0.lock().unwrap().quit = true;
        sc.1.notify_all();
    }
    std::thread::sleep(std::time::Duration::from_millis(150));
}

/// Buffers the programs destroyed: the monitors drop what they kept of them.
pub fn forget(buffers: &[u64]) {
    for (_, sc) in MONITORS.lock().unwrap().iter() {
        sc.0.lock().unwrap().forget.extend_from_slice(buffers);
    }
}
