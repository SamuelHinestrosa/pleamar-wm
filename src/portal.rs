//! Sharing the screen, as the session's own portal: what a program in a
//! call (Discord, a browser, OBS) asks `xdg-desktop-portal` for, answered here
//! (`org.freedesktop.impl.portal.ScreenCast`) instead of by a portal of
//! another desktop's. The pictures are the monitors' own —the same road as a
//! screenshot (`layers::capture`), taken when the monitor changes, not on a
//! clock— and they leave by PipeWire, one stream per session.
//!
//! What is shared is chosen in the window manager's scene: the portal asks it
//! (`win.picking`) and it answers with `pick` —a monitor, or a window, which
//! is then read from its own buffers, covered or on another workspace—.
//!
//! Two threads: D-Bus (zbus), which answers the portal, and PipeWire, which
//! owns the streams and asks the monitors for their pictures.

use crate::layers;
use pipewire as pw;
use pw::spa;
use spa::pod::{Object, Pod, Property, PropertyFlags, Value};
use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use pleamar::scene::{NestEvent, Picked, ToRender, WindowPicture};
use std::sync::mpsc::Sender;
use std::sync::Mutex;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value as ZValue};

/// The name the portal asks for (`pleamar.portal` says it).
const NAME: &str = "org.freedesktop.impl.portal.desktop.pleamar";
const PATH: &str = "/org/freedesktop/portal/desktop";

/// Its pictures' numbers: far from the ones the Wayland side gives, so
/// `hand_picture` knows which are these.
const FIRST: u64 = 1 << 62;
static NEXT: AtomicU64 = AtomicU64::new(FIRST);

/// What is shared: a monitor, or a window of the scene's (its slot).
#[derive(Clone, Copy, PartialEq)]
enum Source {
    Monitor(usize),
    Window(usize),
}

/// What the D-Bus side, the monitors and the render tell the PipeWire thread.
enum Msg {
    /// A stream of that; its node and its size, once PipeWire gives it one.
    /// `cursor`: the pointer drawn into the pictures (the program asked for it).
    Start { session: String, source: Source, cursor: bool, reply: async_channel::Sender<Option<(u32, (u32, u32))>> },
    Stop { session: String },
    /// A picture of a monitor it asked for, taken (BGRx, rows with no padding).
    Picture { id: u64, pixels: Option<Vec<u8>> },
    /// A shared window drew: its picture.
    Window { session: String, picture: WindowPicture },
    /// A look at the pointer, for the streams that draw it.
    Tick,
}

/// How many streams draw the pointer, and the thread that looks at it for
/// them: asleep while there are none.
static WITH_POINTER: AtomicU64 = AtomicU64::new(0);
static LOOKER: std::sync::OnceLock<std::thread::Thread> = std::sync::OnceLock::new();

fn pointer_streams(more: bool) {
    if more {
        WITH_POINTER.fetch_add(1, Ordering::AcqRel);
        if let Some(t) = LOOKER.get() {
            t.unpark();
        }
    } else {
        let _ = WITH_POINTER.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
    }
}

static TO_PW: Mutex<Option<pw::channel::Sender<Msg>>> = Mutex::new(None);
/// The scene's render: to ask it to choose, and for the windows' pictures.
static TO_RENDER: Mutex<Option<Sender<ToRender>>> = Mutex::new(None);
/// Who waits for the scene's choice (one at a time).
static CHOOSING: Mutex<Option<async_channel::Sender<Option<Picked>>>> = Mutex::new(None);

fn render(m: ToRender) {
    if let Some(tx) = TO_RENDER.lock().unwrap().as_ref() {
        let _ = tx.send(m);
    }
}

/// The scene's answer (`pick`): to whoever asked, and the scene stops choosing.
pub fn picked(what: Option<Picked>) {
    render(ToRender::Nest(NestEvent::Pick(0)));
    if let Some(tx) = CHOOSING.lock().unwrap().take() {
        let _ = tx.try_send(what);
    }
}

fn tell(m: Msg) {
    if let Some(tx) = TO_PW.lock().unwrap().as_ref() {
        let _ = tx.send(m);
    }
}

/// A picture that came back from a monitor: whether it was one of these.
pub fn deliver(id: u64, pixels: Option<Vec<u8>>) -> bool {
    if id < FIRST {
        return false;
    }
    tell(Msg::Picture { id, pixels });
    true
}

/// Starts both threads. Without a session bus or PipeWire there is simply no
/// portal: the session goes on.
pub fn start(to_render: Sender<ToRender>) {
    *TO_RENDER.lock().unwrap() = Some(to_render);
    let (tx, rx) = pw::channel::channel::<Msg>();
    *TO_PW.lock().unwrap() = Some(tx);
    let _ = std::thread::Builder::new().name("portal-pw".into()).spawn(move || {
        if let Err(e) = pipewire_thread(rx) {
            eprintln!("portal · no PipeWire ({e}): the screen cannot be shared");
        }
    });
    if let Ok(h) = std::thread::Builder::new().name("portal-pointer".into()).spawn(|| loop {
        if WITH_POINTER.load(Ordering::Acquire) == 0 {
            std::thread::park();
            continue;
        }
        std::thread::sleep(std::time::Duration::from_millis(33));
        tell(Msg::Tick);
    }) {
        let _ = LOOKER.set(h.thread().clone());
    }
    let _ = std::thread::Builder::new().name("portal-dbus".into()).spawn(|| {
        let sessions: Sessions = Default::default();
        let built = zbus::blocking::connection::Builder::session()
            .and_then(|b| b.serve_at(PATH, ScreenCast { sessions }))
            .and_then(|b| b.name(NAME))
            .and_then(|b| b.build());
        match built {
            Ok(connection) => {
                println!("portal · {NAME}: sharing the screen is this session's");
                // The connection answers on its own thread; this one keeps it alive.
                loop {
                    std::thread::park();
                    let _ = &connection;
                }
            }
            Err(e) => eprintln!("portal · not on the session bus ({e})"),
        }
    });
}

// ── D-Bus: what the portal asks ───────────────────────────────────

/// What each session chose, by its object path.
#[derive(Default, Clone)]
struct Choices {
    /// What it may share: 1 monitors, 2 windows (both, 3).
    types: u32,
    cursor: u32,
}
type Sessions = std::sync::Arc<Mutex<HashMap<String, Choices>>>;

struct ScreenCast {
    sessions: Sessions,
}

type Answer = (u32, HashMap<String, OwnedValue>);

fn owned(v: ZValue<'_>) -> OwnedValue {
    OwnedValue::try_from(v).expect("a value with no file descriptors")
}

#[zbus::interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCast {
    async fn create_session(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: String,
        _options: HashMap<String, OwnedValue>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> Answer {
        let path = session_handle.to_string();
        println!("portal · {} asks to share the screen", if app_id.is_empty() { "a program" } else { &app_id });
        self.sessions.lock().unwrap().insert(path.clone(), Choices::default());
        let session = Session { path: path.clone(), sessions: self.sessions.clone() };
        if server.at(session_handle.as_ref(), session).await.is_err() {
            return (2, HashMap::new());
        }
        (0, HashMap::new())
    }

    async fn select_sources(&self, _handle: OwnedObjectPath, session_handle: OwnedObjectPath, _app_id: String, options: HashMap<String, OwnedValue>) -> Answer {
        let cursor = options.get("cursor_mode").and_then(|v| u32::try_from(v).ok()).unwrap_or(1);
        let types = options.get("types").and_then(|v| u32::try_from(v).ok()).unwrap_or(1) & 3;
        match self.sessions.lock().unwrap().get_mut(session_handle.as_str()) {
            Some(c) => {
                c.cursor = cursor;
                c.types = if types == 0 { 1 } else { types };
                (0, HashMap::new())
            }
            None => (2, HashMap::new()),
        }
    }

    async fn start(&self, _handle: OwnedObjectPath, session_handle: OwnedObjectPath, _app_id: String, _parent_window: String, _options: HashMap<String, OwnedValue>) -> Answer {
        let session = session_handle.to_string();
        let Some((types, cursor)) = self.sessions.lock().unwrap().get(&session).map(|c| (c.types, c.cursor == 2)) else {
            return (2, HashMap::new());
        };
        // The scene chooses: it is asked, and whoever asked before is let go.
        let (tx, choice) = async_channel::bounded(1);
        if let Some(old) = CHOOSING.lock().unwrap().replace(tx) {
            let _ = old.try_send(None);
        }
        render(ToRender::Nest(NestEvent::Pick(types)));
        // Not forever: a scene of one's own may not know how to choose.
        let chosen = futures_lite::future::or(async { choice.recv().await.ok().flatten() }, async {
            async_io::Timer::after(std::time::Duration::from_secs(120)).await;
            picked(None);
            None
        })
        .await;
        let source = match chosen {
            Some(Picked::Screen(n)) if types & 1 != 0 => Source::Monitor(n),
            Some(Picked::Window(slot)) if types & 2 != 0 => Source::Window(slot),
            // Turned down (or asked again by someone else): 1, the user said no.
            _ => return (1, HashMap::new()),
        };
        if !self.sessions.lock().unwrap().contains_key(&session) {
            return (2, HashMap::new());
        }
        let (reply, answer) = async_channel::bounded(1);
        tell(Msg::Start { session: session.clone(), source, cursor, reply });
        let Ok(Some((node, px))) = answer.recv().await else { return (2, HashMap::new()) };
        let props: HashMap<String, OwnedValue> = match source {
            Source::Monitor(n) => {
                let Some(m) = layers::monitors().get(n).cloned() else { return (2, HashMap::new()) };
                println!("portal · sharing {} (PipeWire node {node})", m.name);
                let size = ((m.size.0 as f64 / m.scale).round() as i32, (m.size.1 as f64 / m.scale).round() as i32);
                HashMap::from([
                    ("position".to_owned(), owned(ZValue::from((m.x, m.y)))),
                    ("size".to_owned(), owned(ZValue::from(size))),
                    ("source_type".to_owned(), owned(ZValue::from(1u32))),
                ])
            }
            Source::Window(slot) => {
                println!("portal · sharing window {slot} (PipeWire node {node})");
                HashMap::from([
                    ("size".to_owned(), owned(ZValue::from((px.0 as i32, px.1 as i32)))),
                    ("source_type".to_owned(), owned(ZValue::from(2u32))),
                ])
            }
        };
        let streams = vec![(node, props)];
        (0, HashMap::from([("streams".to_owned(), owned(ZValue::from(streams)))]))
    }

    /// Monitors (1) and windows (2).
    #[zbus(property)]
    fn available_source_types(&self) -> u32 {
        1 | 2
    }

    /// Hidden (1) and embedded (2): what programs ask for most.
    #[zbus(property)]
    fn available_cursor_modes(&self) -> u32 {
        1 | 2
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        5
    }
}

/// A session the portal opened: closing it stops its stream.
struct Session {
    path: String,
    sessions: Sessions,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Session")]
impl Session {
    async fn close(&self, #[zbus(object_server)] server: &zbus::ObjectServer) {
        self.sessions.lock().unwrap().remove(&self.path);
        // Closed while the scene was still choosing (the program gave up): it stops.
        if CHOOSING.lock().unwrap().is_some() {
            picked(None);
        }
        tell(Msg::Stop { session: self.path.clone() });
        let _ = server.remove::<Session, _>(self.path.as_str()).await;
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }
}

// ── PipeWire: the streams ─────────────────────────────────────────

/// A stream's side of things, shared with its callbacks.
struct Feed {
    source: Source,
    size: (u32, u32),
    /// The picture waiting to go out, and the one asked for, not yet taken.
    ready: Option<(Vec<u8>, (u32, u32))>,
    /// The size the stream agreed on with whoever watches: a picture of
    /// another size waits for the next agreement (a window that grew).
    agreed: Option<(u32, u32)>,
    /// The pointer drawn into it: the last picture without it, to draw it
    /// again where it has moved to, and the move it was drawn at.
    cursor: bool,
    clean: Option<(Vec<u8>, (u32, u32))>,
    moves: u64,
    asked: Option<u64>,
    streaming: bool,
    /// Who waits for its node (the D-Bus `Start`).
    reply: Option<async_channel::Sender<Option<(u32, (u32, u32))>>>,
}

impl Feed {
    /// A new picture: kept as it is, and with the pointer on it if it goes.
    fn take(&mut self, mut pixels: Vec<u8>, size: (u32, u32)) {
        if self.cursor {
            self.clean = Some((pixels.clone(), size));
            self.moves = layers::pointer_seen().map_or(0, |p| p.moves);
            pointer_onto(self.source, &mut pixels, size);
        }
        self.ready = Some((pixels, size));
    }

    /// The pointer moved and nothing else: the last picture again, with it
    /// where it is now. Whether there is one to send.
    fn pointer_moved(&mut self) -> bool {
        let moves = layers::pointer_seen().map_or(0, |p| p.moves);
        if !self.cursor || !self.streaming || moves == self.moves {
            return false;
        }
        let Some((pixels, size)) = self.clean.clone() else { return false };
        self.take(pixels, size);
        true
    }
}

/// The pointer drawn onto a picture of a monitor or a window, where it is on
/// it (BGRx; the pointer's picture is premultiplied).
fn pointer_onto(source: Source, pixels: &mut [u8], (w, h): (u32, u32)) {
    let Some(p) = layers::pointer_seen() else { return };
    let Some(picture) = p.picture else { return };
    let monitors = layers::monitors();
    // Where its tip is, in the picture's pixels.
    let tip = match source {
        Source::Monitor(n) => monitors.get(n).map(|m| ((p.at.0 - m.x as f64) * m.scale, (p.at.1 - m.y as f64) * m.scale)),
        Source::Window(slot) => layers::shown(slot).and_then(|(name, r)| {
            let m = monitors.iter().find(|m| m.name == name)?;
            let (x, y) = ((p.at.0 - m.x as f64) * m.scale - r[0] as f64, (p.at.1 - m.y as f64) * m.scale - r[1] as f64);
            (r[2] > 0 && r[3] > 0).then(|| (x * w as f64 / r[2] as f64, y * h as f64 / r[3] as f64))
        }),
    };
    let Some((tx, ty)) = tip else { return };
    if tx < 0.0 || ty < 0.0 || tx >= w as f64 || ty >= h as f64 {
        return;
    }
    let (image, (hx, hy)) = &*picture;
    let (ox, oy) = (tx as i32 - hx, ty as i32 - hy);
    for y in 0..64i32 {
        let py = oy + y;
        if py < 0 || py >= h as i32 {
            continue;
        }
        for x in 0..64i32 {
            let px = ox + x;
            if px < 0 || px >= w as i32 {
                continue;
            }
            let s = ((y * 64 + x) * 4) as usize;
            let a = image[s + 3] as u32;
            if a == 0 {
                continue;
            }
            let d = ((py as u32 * w + px as u32) * 4) as usize;
            for k in 0..3 {
                pixels[d + k] = (image[s + k] as u32 + pixels[d + k] as u32 * (255 - a) / 255).min(255) as u8;
            }
        }
    }
}

struct Cast {
    stream: pw::stream::StreamRc,
    _listener: pw::stream::StreamListener<Rc<RefCell<Feed>>>,
    feed: Rc<RefCell<Feed>>,
}

/// The next picture of its monitor: the first at once, the rest when
/// something on it changes (nothing moves, nothing is sent).
fn ask(feed: &mut Feed, at_once: bool) {
    // A window's pictures come by themselves, each time it draws.
    let Source::Monitor(monitor) = feed.source else { return };
    if feed.asked.is_some() {
        return;
    }
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    feed.asked = Some(id);
    // Its owner is nobody's: every change counts (the scene's are said as 0).
    layers::capture(monitor, id, [0, 0, feed.size.0 as i32, feed.size.1 as i32], !at_once, u64::MAX);
}

fn pod(object: Object) -> Vec<u8> {
    spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(object)).map(|(c, _)| c.into_inner()).unwrap_or_default()
}

fn prop(key: u32, value: Value) -> Property {
    Property { key, flags: PropertyFlags::empty(), value }
}

/// What it offers: BGRx at the monitor's size, at any pace up to its own.
fn format(size: (u32, u32), hz: u32) -> Vec<u8> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    pod(Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: vec![
            prop(FormatProperties::MediaType.as_raw(), Value::Id(Id(MediaType::Video.as_raw()))),
            prop(FormatProperties::MediaSubtype.as_raw(), Value::Id(Id(MediaSubtype::Raw.as_raw()))),
            prop(FormatProperties::VideoFormat.as_raw(), Value::Id(Id(spa::param::video::VideoFormat::BGRx.as_raw()))),
            prop(FormatProperties::VideoSize.as_raw(), Value::Rectangle(Rectangle { width: size.0, height: size.1 })),
            prop(
                FormatProperties::VideoFramerate.as_raw(),
                Value::Choice(spa::pod::ChoiceValue::Fraction(Choice(ChoiceFlags::empty(), ChoiceEnum::Range { default: Fraction { num: hz, denom: 1 }, min: Fraction { num: 0, denom: 1 }, max: Fraction { num: hz.max(1), denom: 1 } }))),
            ),
        ],
    })
}

/// How its buffers are: one block of the whole picture, in shared memory.
fn buffers(size: (u32, u32)) -> Vec<u8> {
    let stride = size.0 as i32 * 4;
    pod(Object {
        type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![
            prop(spa::sys::SPA_PARAM_BUFFERS_buffers, Value::Choice(spa::pod::ChoiceValue::Int(Choice(ChoiceFlags::empty(), ChoiceEnum::Range { default: 4, min: 2, max: 8 })))),
            prop(spa::sys::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
            prop(spa::sys::SPA_PARAM_BUFFERS_size, Value::Int(stride * size.1 as i32)),
            prop(spa::sys::SPA_PARAM_BUFFERS_stride, Value::Int(stride)),
            prop(spa::sys::SPA_PARAM_BUFFERS_dataType, Value::Choice(spa::pod::ChoiceValue::Int(Choice(ChoiceFlags::empty(), ChoiceEnum::Flags { default: 1 << spa::sys::SPA_DATA_MemFd, flags: vec![] })))),
        ],
    })
}

fn pipewire_thread(rx: pw::channel::Receiver<Msg>) -> Result<(), pw::Error> {
    pw::init();
    let main = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&main, None)?;
    let core = context.connect_rc(None)?;
    let casts: Rc<RefCell<HashMap<String, Cast>>> = Default::default();
    let c = casts.clone();
    // Windows chosen whose first picture has not come yet: its size is the stream's.
    let waiting: RefCell<HashMap<String, (usize, bool, async_channel::Sender<Option<(u32, (u32, u32))>>)>> = RefCell::new(HashMap::new());
    let _attached = rx.attach(main.loop_(), move |msg| match msg {
        Msg::Start { session, source: Source::Monitor(n), cursor, reply } => {
            let Some(m) = layers::monitors().get(n).cloned() else {
                let _ = reply.try_send(None);
                return;
            };
            match open(&core, &session, Source::Monitor(n), cursor, m.size, (m.mhz.max(1000) as u32 + 500) / 1000, reply.clone()) {
                Ok(cast) => {
                    c.borrow_mut().insert(session, cast);
                }
                Err(e) => {
                    eprintln!("portal · a stream could not be made: {e}");
                    let _ = reply.try_send(None);
                }
            }
        }
        Msg::Start { session, source: Source::Window(slot), cursor, reply } => {
            // Its pictures, each time it draws, from the render to here.
            let (tx, pictures) = std::sync::mpsc::channel::<WindowPicture>();
            render(ToRender::WatchWindow(slot, Some(tx)));
            let to_pw = TO_PW.lock().unwrap().clone();
            let name = session.clone();
            let _ = std::thread::Builder::new().name("portal-window".into()).spawn(move || {
                let Some(to_pw) = to_pw else { return };
                for picture in pictures {
                    if to_pw.send(Msg::Window { session: name.clone(), picture }).is_err() {
                        return;
                    }
                }
            });
            waiting.borrow_mut().insert(session, (slot, cursor, reply));
        }
        Msg::Window { session, picture } => {
            let first = waiting.borrow_mut().remove(&session);
            if let Some((slot, cursor, reply)) = first {
                match open(&core, &session, Source::Window(slot), cursor, picture.size, 60, reply.clone()) {
                    Ok(cast) => {
                        cast.feed.borrow_mut().take(picture.pixels, picture.size);
                        c.borrow_mut().insert(session, cast);
                    }
                    Err(e) => {
                        eprintln!("portal · a stream could not be made: {e}");
                        let _ = reply.try_send(None);
                        render(ToRender::WatchWindow(slot, None));
                    }
                }
                return;
            }
            let casts = c.borrow();
            let Some(cast) = casts.get(&session) else { return };
            let mut feed = cast.feed.borrow_mut();
            // Another size: the stream says so, and whoever watches takes the new one.
            if picture.size != feed.size {
                feed.size = picture.size;
                let bytes = format(picture.size, 60);
                if let Some(p) = Pod::from_bytes(&bytes) {
                    let _ = cast.stream.update_params(&mut [p]);
                }
            }
            feed.take(picture.pixels, picture.size);
            let streaming = feed.streaming;
            drop(feed);
            if streaming {
                let _ = cast.stream.trigger_process();
            }
        }
        // The pointer moving over what does not change.
        Msg::Tick => {
            for cast in c.borrow().values() {
                let moved = cast.feed.borrow_mut().pointer_moved();
                if moved {
                    let _ = cast.stream.trigger_process();
                }
            }
        }
        Msg::Stop { session } => {
            let first = waiting.borrow_mut().remove(&session);
            if let Some((slot, _, reply)) = first {
                render(ToRender::WatchWindow(slot, None));
                let _ = reply.try_send(None);
            }
            if let Some(cast) = c.borrow_mut().remove(&session) {
                let feed = cast.feed.borrow();
                if let Some(id) = feed.asked {
                    layers::uncapture(id);
                }
                if let Source::Window(slot) = feed.source {
                    render(ToRender::WatchWindow(slot, None));
                }
                if feed.cursor {
                    pointer_streams(false);
                }
                drop(feed);
                let _ = cast.stream.disconnect();
                println!("portal · stopped sharing");
            }
        }
        Msg::Picture { id, pixels } => {
            let casts = c.borrow();
            let Some(cast) = casts.values().find(|k| k.feed.borrow().asked == Some(id)) else { return };
            let mut feed = cast.feed.borrow_mut();
            feed.asked = None;
            if let Some(px) = pixels {
                let size = feed.size;
                feed.take(px, size);
                drop(feed);
                let _ = cast.stream.trigger_process();
                feed = cast.feed.borrow_mut();
            }
            if feed.streaming {
                ask(&mut feed, false);
            }
        }
    });
    main.run();
    Ok(())
}

fn open(core: &pw::core::CoreRc, session: &str, source: Source, cursor: bool, size: (u32, u32), hz: u32, reply: async_channel::Sender<Option<(u32, (u32, u32))>>) -> Result<Cast, pw::Error> {
    let stream = pw::stream::StreamRc::new(
        core.clone(),
        "pleamar-wm",
        pw::properties::properties! {
            *pw::keys::MEDIA_CLASS => "Video/Source",
            *pw::keys::MEDIA_NAME => "pleamar-wm screen",
            *pw::keys::NODE_DESCRIPTION => session,
        },
    )?;
    let feed = Rc::new(RefCell::new(Feed { source, size, ready: None, agreed: None, cursor, clean: None, moves: 0, asked: None, streaming: false, reply: Some(reply) }));
    let listener = stream
        .add_local_listener_with_user_data(feed.clone())
        .state_changed(|stream, feed, _, new| {
            let mut f = feed.borrow_mut();
            if let Some(reply) = f.reply.take() {
                match new {
                    pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming => {
                        let _ = reply.try_send(Some((stream.node_id(), f.size)));
                    }
                    pw::stream::StreamState::Error(_) => {
                        let _ = reply.try_send(None);
                    }
                    _ => f.reply = Some(reply),
                }
            }
            f.streaming = matches!(new, pw::stream::StreamState::Streaming);
            if f.streaming {
                ask(&mut f, true);
                // A window's picture that came before anyone watched: it goes now.
                if f.ready.is_some() {
                    drop(f);
                    let _ = stream.trigger_process();
                }
            }
        })
        .param_changed(|stream, feed, id, param| {
            let Some(param) = param.filter(|_| id == spa::param::ParamType::Format.as_raw()) else { return };
            // The size agreed: the buffers are made for it, and a picture of
            // that size that was waiting goes now.
            let mut info = spa::param::video::VideoInfoRaw::default();
            if info.parse(param).is_err() {
                return;
            }
            let agreed = (info.size().width, info.size().height);
            feed.borrow_mut().agreed = Some(agreed);
            let bytes = buffers(agreed);
            if let Some(p) = Pod::from_bytes(&bytes) {
                let _ = stream.update_params(&mut [p]);
            }
            if feed.borrow().ready.as_ref().is_some_and(|(_, s)| *s == agreed) {
                let _ = stream.trigger_process();
            }
        })
        .process(|stream, feed| {
            let mut f = feed.borrow_mut();
            // Only a picture of the size agreed fits the buffers: another one
            // waits for its agreement (it was asked for already).
            let Some(agreed) = f.agreed else { return };
            if !f.ready.as_ref().is_some_and(|(_, s)| *s == agreed) {
                return;
            }
            let Some((pixels, (w, h))) = f.ready.take() else { return };
            drop(f);
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else { return };
            let stride = w as usize * 4;
            if let Some(dst) = data.data() {
                let n = (stride * h as usize).min(dst.len()).min(pixels.len());
                dst[..n].copy_from_slice(&pixels[..n]);
            }
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as i32;
            *chunk.size_mut() = (stride * h as usize) as u32;
        })
        .register()?;
    let bytes = format(size, hz);
    let mut params = [Pod::from_bytes(&bytes).ok_or(pw::Error::CreationFailed)?];
    stream.connect(spa::utils::Direction::Output, None, pw::stream::StreamFlags::DRIVER | pw::stream::StreamFlags::MAP_BUFFERS, &mut params)?;
    // Counted only once it is sure to be there: its `Stop` uncounts it.
    if cursor {
        pointer_streams(true);
    }
    Ok(Cast { stream, _listener: listener, feed })
}
