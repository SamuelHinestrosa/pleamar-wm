//! Sharing the screen, as the session's own portal: what a program in a
//! call (Discord, a browser, OBS) asks `xdg-desktop-portal` for, answered here
//! (`org.freedesktop.impl.portal.ScreenCast`) instead of by a portal of
//! another desktop's. The pictures are the monitors' own —the same road as a
//! screenshot (`layers::capture`), taken when the monitor changes, not on a
//! clock— and they leave by PipeWire, one stream per session.
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
use std::sync::Mutex;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value as ZValue};

/// The name the portal asks for (`pleamar.portal` says it).
const NAME: &str = "org.freedesktop.impl.portal.desktop.pleamar";
const PATH: &str = "/org/freedesktop/portal/desktop";

/// Its pictures' numbers: far from the ones the Wayland side gives, so
/// `hand_picture` knows which are these.
const FIRST: u64 = 1 << 62;
static NEXT: AtomicU64 = AtomicU64::new(FIRST);

/// What the D-Bus side and the monitors tell the PipeWire thread.
enum Msg {
    /// A stream for that monitor; its node, once PipeWire gives it one.
    Start { session: String, monitor: usize, reply: async_channel::Sender<Option<u32>> },
    Stop { session: String },
    /// A picture it asked for, taken (BGRx, rows with no padding).
    Picture { id: u64, pixels: Option<Vec<u8>> },
}

static TO_PW: Mutex<Option<pw::channel::Sender<Msg>>> = Mutex::new(None);

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
pub fn start() {
    let (tx, rx) = pw::channel::channel::<Msg>();
    *TO_PW.lock().unwrap() = Some(tx);
    let _ = std::thread::Builder::new().name("portal-pw".into()).spawn(move || {
        if let Err(e) = pipewire_thread(rx) {
            eprintln!("portal · no PipeWire ({e}): the screen cannot be shared");
        }
    });
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
        match self.sessions.lock().unwrap().get_mut(session_handle.as_str()) {
            Some(c) => {
                c.cursor = cursor;
                (0, HashMap::new())
            }
            None => (2, HashMap::new()),
        }
    }

    async fn start(&self, _handle: OwnedObjectPath, session_handle: OwnedObjectPath, _app_id: String, _parent_window: String, _options: HashMap<String, OwnedValue>) -> Answer {
        let session = session_handle.to_string();
        if !self.sessions.lock().unwrap().contains_key(&session) {
            return (2, HashMap::new());
        }
        let monitors = layers::monitors();
        // For now the first monitor; choosing is Marea's, next.
        let monitor = 0;
        let Some(m) = monitors.get(monitor) else { return (2, HashMap::new()) };
        let (reply, answer) = async_channel::bounded(1);
        tell(Msg::Start { session: session.clone(), monitor, reply });
        let node = match answer.recv().await {
            Ok(Some(node)) => node,
            _ => return (2, HashMap::new()),
        };
        println!("portal · sharing {} (PipeWire node {node})", m.name);
        let size = ((m.size.0 as f64 / m.scale).round() as i32, (m.size.1 as f64 / m.scale).round() as i32);
        let props: HashMap<String, OwnedValue> = HashMap::from([
            ("position".to_owned(), owned(ZValue::from((m.x, m.y)))),
            ("size".to_owned(), owned(ZValue::from(size))),
            ("source_type".to_owned(), owned(ZValue::from(1u32))),
        ]);
        let streams = vec![(node, props)];
        (0, HashMap::from([("streams".to_owned(), owned(ZValue::from(streams)))]))
    }

    /// Monitors (1) for now; windows (2) come with Marea's picker.
    #[zbus(property)]
    fn available_source_types(&self) -> u32 {
        1
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
    monitor: usize,
    size: (u32, u32),
    /// The picture waiting to go out, and the one asked for, not yet taken.
    ready: Option<Vec<u8>>,
    asked: Option<u64>,
    streaming: bool,
    /// Who waits for its node (the D-Bus `Start`).
    reply: Option<async_channel::Sender<Option<u32>>>,
}

struct Cast {
    stream: pw::stream::StreamRc,
    _listener: pw::stream::StreamListener<Rc<RefCell<Feed>>>,
    feed: Rc<RefCell<Feed>>,
}

/// The next picture of its monitor: the first at once, the rest when
/// something on it changes (nothing moves, nothing is sent).
fn ask(feed: &mut Feed, at_once: bool) {
    if feed.asked.is_some() {
        return;
    }
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    feed.asked = Some(id);
    // Its owner is nobody's: every change counts (the scene's are said as 0).
    layers::capture(feed.monitor, id, [0, 0, feed.size.0 as i32, feed.size.1 as i32], !at_once, u64::MAX);
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
    let _attached = rx.attach(main.loop_(), move |msg| match msg {
        Msg::Start { session, monitor, reply } => {
            let Some(m) = layers::monitors().get(monitor).cloned() else {
                let _ = reply.try_send(None);
                return;
            };
            match open(&core, &session, monitor, m.size, (m.mhz.max(1000) as u32 + 500) / 1000, reply.clone()) {
                Ok(cast) => {
                    c.borrow_mut().insert(session, cast);
                }
                Err(e) => {
                    eprintln!("portal · a stream could not be made: {e}");
                    let _ = reply.try_send(None);
                }
            }
        }
        Msg::Stop { session } => {
            if let Some(cast) = c.borrow_mut().remove(&session) {
                if let Some(id) = cast.feed.borrow().asked {
                    layers::uncapture(id);
                }
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
                feed.ready = Some(px);
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

fn open(core: &pw::core::CoreRc, session: &str, monitor: usize, size: (u32, u32), hz: u32, reply: async_channel::Sender<Option<u32>>) -> Result<Cast, pw::Error> {
    let stream = pw::stream::StreamRc::new(
        core.clone(),
        "pleamar-wm",
        pw::properties::properties! {
            *pw::keys::MEDIA_CLASS => "Video/Source",
            *pw::keys::MEDIA_NAME => "pleamar-wm screen",
            *pw::keys::NODE_DESCRIPTION => session,
        },
    )?;
    let feed = Rc::new(RefCell::new(Feed { monitor, size, ready: None, asked: None, streaming: false, reply: Some(reply) }));
    let listener = stream
        .add_local_listener_with_user_data(feed.clone())
        .state_changed(|stream, feed, _, new| {
            let mut f = feed.borrow_mut();
            if let Some(reply) = f.reply.take() {
                match new {
                    pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming => {
                        let _ = reply.try_send(Some(stream.node_id()));
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
            }
        })
        .param_changed(|stream, feed, id, param| {
            if id != spa::param::ParamType::Format.as_raw() || param.is_none() {
                return;
            }
            let bytes = buffers(feed.borrow().size);
            if let Some(p) = Pod::from_bytes(&bytes) {
                let _ = stream.update_params(&mut [p]);
            }
        })
        .process(|stream, feed| {
            let Some(pixels) = feed.borrow_mut().ready.take() else { return };
            let (w, h) = feed.borrow().size;
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
    Ok(Cast { stream, _listener: listener, feed })
}
