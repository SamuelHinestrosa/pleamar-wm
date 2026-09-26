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
/// the desktop, and its refresh in mHz.
#[derive(Clone, Debug)]
pub struct MonitorInfo {
    pub name: String,
    pub size: (u32, u32),
    pub x: i32,
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
}

/// A program's surface on a monitor: where, at what level, and what it shows.
pub struct ClientLayer {
    pub id: u64,
    /// 0 background, 1 bottom, 2 top, 3 overlay.
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
static NEST: Mutex<Option<channel::Sender<ToLayers>>> = Mutex::new(None);

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

/// Buffers the programs destroyed: the monitors drop what they kept of them.
pub fn forget(buffers: &[u64]) {
    for (_, sc) in MONITORS.lock().unwrap().iter() {
        sc.0.lock().unwrap().forget.extend_from_slice(buffers);
    }
}
