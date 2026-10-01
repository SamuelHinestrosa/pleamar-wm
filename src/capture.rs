//! Pictures of one window, for the programs that want them: an overview with
//! a thumbnail of each window, a switcher, a recorder of one window. Spoken in
//! the standard protocols (ext-foreign-toplevel-image-capture-source to name
//! the window, ext-image-copy-capture to copy it), so a shell written for any
//! compositor that has them works here too.
//!
//! The pictures come from the render, which has every window on the card
//! (`WatchWindow`): one each time the window draws, read once and shared by
//! all who watch it. A frame a program asks for is handed only once there is
//! a picture newer than the last it got: a window that does not change costs
//! nothing, and its program waits, as the protocol says it should. A window on
//! a pool that is not shown still draws on the card, so it is still pictured.
//!
//! Only windows: the monitors are wlr-screencopy's (`Picture`). Memory only
//! (wl_shm, ARGB/XRGB): no dmabuf yet.

use super::*;
use smithay::reexports::wayland_protocols::ext::image_capture_source::v1::server::{
    ext_foreign_toplevel_image_capture_source_manager_v1::{self as source_manager, ExtForeignToplevelImageCaptureSourceManagerV1},
    ext_image_capture_source_v1::{self as capture_source, ExtImageCaptureSourceV1},
};
use smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::{
    ext_image_copy_capture_cursor_session_v1::{self as cursor_session, ExtImageCopyCaptureCursorSessionV1},
    ext_image_copy_capture_frame_v1::{self as copy_frame, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{self as copy_manager, ExtImageCopyCaptureManagerV1},
    ext_image_copy_capture_session_v1::{self as copy_session, ExtImageCopyCaptureSessionV1},
};
use std::sync::atomic::{AtomicBool, Ordering};

/// A window being watched for pictures: its latest, how many it has had
/// (so a session knows whether it has seen it), and how to stop watching.
struct Watch {
    slot: usize,
    who: u64,
    version: u64,
    latest: Option<WindowPicture>,
    stop: Arc<AtomicBool>,
}

/// A program's session on a window: the size it was told, the last picture
/// it was handed, and its frame, if it has one waiting.
struct Session {
    session: ExtImageCopyCaptureSessionV1,
    window: String,
    told: Option<(u32, u32)>,
    handed: u64,
    frame: Option<Frame>,
}

struct Frame {
    frame: ExtImageCopyCaptureFrameV1,
    buffer: Option<WlBuffer>,
    asked: bool,
}

pub(super) struct Thumbs {
    /// By the window's identifier (ext-foreign-toplevel-list's).
    watches: HashMap<String, Watch>,
    sessions: HashMap<u64, Session>,
    next: u64,
    /// Where the watchers hand their pictures to the compositor's loop.
    arrived: channel::Sender<(String, WindowPicture)>,
}

impl Thumbs {
    pub(super) fn new(arrived: channel::Sender<(String, WindowPicture)>) -> Thumbs {
        Thumbs { watches: HashMap::new(), sessions: HashMap::new(), next: 0, arrived }
    }
}

/// The globals, once the compositor is up.
pub(super) fn globals(dh: &DisplayHandle) {
    dh.create_global::<State, ExtForeignToplevelImageCaptureSourceManagerV1, _>(1, ());
    dh.create_global::<State, ExtImageCopyCaptureManagerV1, _>(1, ());
}

impl State {
    /// A picture of a watched window has arrived: it is its latest, and the
    /// sessions on it that were waiting for one get it.
    pub(super) fn thumb_arrived(&mut self, window: String, picture: WindowPicture) {
        let Some(w) = self.thumbs.watches.get_mut(&window) else { return };
        w.version += 1;
        w.latest = Some(picture);
        let ids: Vec<u64> = self.thumbs.sessions.iter().filter(|(_, s)| s.window == window).map(|(k, _)| *k).collect();
        for id in ids {
            self.thumb_deliver(id);
        }
    }

    /// The window is gone: its sessions are told to stop, and it is no longer watched.
    pub(super) fn thumb_closed(&mut self, window: &str) {
        for s in self.thumbs.sessions.values_mut().filter(|s| s.window == window) {
            if let Some(f) = s.frame.take() {
                f.frame.failed(copy_frame::FailureReason::Stopped);
            }
            s.session.stopped();
        }
        self.thumbs.sessions.retain(|_, s| s.window != window);
        self.unwatch(window);
    }

    /// The window by its identifier: which slot it is in now.
    fn slot_of(&self, window: &str) -> Option<usize> {
        self.slots.iter().position(|w| w.as_ref().is_some_and(|w| w.listed.identifier() == window))
    }

    fn watch(&mut self, window: &str) {
        if self.thumbs.watches.contains_key(window) {
            return;
        }
        let Some(slot) = self.slot_of(window) else { return };
        self.thumbs.next += 1;
        let who = crate::portal::THUMBS + self.thumbs.next;
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel::<WindowPicture>();
        let _ = self.to_render.send(ToRender::WatchWindow(slot, who, Some(tx)));
        let (arrived, name, quit) = (self.thumbs.arrived.clone(), window.to_owned(), stop.clone());
        let _ = std::thread::Builder::new().name("window pictures".into()).spawn(move || {
            for picture in rx {
                if quit.load(Ordering::Relaxed) || arrived.send((name.clone(), picture)).is_err() {
                    return;
                }
            }
        });
        self.thumbs.watches.insert(window.to_owned(), Watch { slot, who, version: 0, latest: None, stop });
    }

    fn unwatch(&mut self, window: &str) {
        if let Some(w) = self.thumbs.watches.remove(window) {
            w.stop.store(true, Ordering::Relaxed);
            let _ = self.to_render.send(ToRender::WatchWindow(w.slot, w.who, None));
        }
    }

    /// Nobody left on that window: it stops being watched.
    fn thumb_forget(&mut self, id: u64) {
        let Some(s) = self.thumbs.sessions.remove(&id) else { return };
        if !self.thumbs.sessions.values().any(|o| o.window == s.window) {
            self.unwatch(&s.window);
        }
    }

    /// A session's frame, if it was asked for and there is a picture it has
    /// not had: copied into its buffer. The size first, if it changed.
    fn thumb_deliver(&mut self, id: u64) {
        let Some(s) = self.thumbs.sessions.get_mut(&id) else { return };
        let Some(w) = self.thumbs.watches.get(&s.window) else { return };
        let Some(picture) = &w.latest else { return };
        if s.told != Some(picture.size) {
            let (pw, ph) = picture.size;
            s.session.buffer_size(pw, ph);
            s.session.shm_format(wl_shm::Format::Argb8888);
            s.session.shm_format(wl_shm::Format::Xrgb8888);
            s.session.done();
            s.told = Some(picture.size);
            // A frame made for the old size cannot take this one.
            if let Some(f) = s.frame.take_if(|f| f.asked) {
                f.frame.failed(copy_frame::FailureReason::BufferConstraints);
            }
            return;
        }
        if w.version <= s.handed || !s.frame.as_ref().is_some_and(|f| f.asked) {
            return;
        }
        let Some(f) = s.frame.take() else { return };
        let (pw, ph) = (picture.size.0 as usize, picture.size.1 as usize);
        let written = f.buffer.as_ref().is_some_and(|buffer| {
            with_buffer_contents_mut(buffer, |ptr, len, d| {
                let (stride, offset) = (d.stride.max(0) as usize, d.offset.max(0) as usize);
                if d.width as usize != pw || d.height as usize != ph || stride < pw * 4 || offset + stride * ph > len {
                    return false;
                }
                // SAFETY: the program's pool, `len` bytes from `ptr`, and the range was checked.
                let dst = unsafe { std::slice::from_raw_parts_mut(ptr.add(offset), stride * ph) };
                // BGRA in memory is ARGB8888 on a little-endian machine: row by row as it is.
                for y in 0..ph {
                    dst[y * stride..y * stride + pw * 4].copy_from_slice(&picture.pixels[y * pw * 4..(y + 1) * pw * 4]);
                }
                true
            })
            .unwrap_or(false)
        });
        if !written {
            f.frame.failed(copy_frame::FailureReason::BufferConstraints);
            return;
        }
        s.handed = w.version;
        f.frame.transform(smithay::reexports::wayland_server::protocol::wl_output::Transform::Normal);
        f.frame.damage(0, 0, pw as i32, ph as i32);
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: a valid timespec for the call to fill.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        let secs = ts.tv_sec as u64;
        f.frame.presentation_time((secs >> 32) as u32, secs as u32, ts.tv_nsec as u32);
        f.frame.ready();
    }
}

// ── the source: a window, named by its ext-foreign-toplevel-list handle ──

impl GlobalDispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ()> for State {
    fn bind(_: &mut Self, _: &DisplayHandle, _: &Client, resource: New<ExtForeignToplevelImageCaptureSourceManagerV1>, _: &(), init: &mut DataInit<'_, Self>) {
        init.init(resource, ());
    }
}

impl Dispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ()> for State {
    fn request(_: &mut Self, _: &Client, _: &ExtForeignToplevelImageCaptureSourceManagerV1, request: source_manager::Request, _: &(), _: &DisplayHandle, init: &mut DataInit<'_, Self>) {
        if let source_manager::Request::CreateSource { source, toplevel_handle } = request {
            // Its identifier, not its slot: a slot is given to another window once this one closes.
            let window = ForeignToplevelHandle::from_resource(&toplevel_handle).filter(|h| !h.is_closed()).map(|h| h.identifier()).unwrap_or_default();
            init.init(source, window);
        }
    }
}

impl Dispatch<ExtImageCaptureSourceV1, String> for State {
    fn request(_: &mut Self, _: &Client, _: &ExtImageCaptureSourceV1, _: capture_source::Request, _: &String, _: &DisplayHandle, _: &mut DataInit<'_, Self>) {}
}

// ── the copies ──

impl GlobalDispatch<ExtImageCopyCaptureManagerV1, ()> for State {
    fn bind(_: &mut Self, _: &DisplayHandle, _: &Client, resource: New<ExtImageCopyCaptureManagerV1>, _: &(), init: &mut DataInit<'_, Self>) {
        init.init(resource, ());
    }
}

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for State {
    fn request(state: &mut Self, _: &Client, _: &ExtImageCopyCaptureManagerV1, request: copy_manager::Request, _: &(), _: &DisplayHandle, init: &mut DataInit<'_, Self>) {
        match request {
            copy_manager::Request::CreateSession { session, source, .. } => {
                state.thumbs.next += 1;
                let id = state.thumbs.next;
                let session = init.init(session, id);
                // A source that is not a window of ours (another kind, or one already closed).
                let window = source.data::<String>().cloned().unwrap_or_default();
                if window.is_empty() || state.slot_of(&window).is_none() {
                    session.stopped();
                    return;
                }
                state.thumbs.sessions.insert(id, Session { session, window: window.clone(), told: None, handed: 0, frame: None });
                state.watch(&window);
                // Its size, at once if a picture is already there; if not, with the first.
                state.thumb_deliver(id);
            }
            // The pointer on its own is not drawn into window pictures: a session that never gives one.
            copy_manager::Request::CreatePointerCursorSession { session, .. } => {
                init.init(session, ());
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureCursorSessionV1, ()> for State {
    fn request(state: &mut Self, _: &Client, _: &ExtImageCopyCaptureCursorSessionV1, request: cursor_session::Request, _: &(), _: &DisplayHandle, init: &mut DataInit<'_, Self>) {
        if let cursor_session::Request::GetCaptureSession { session } = request {
            state.thumbs.next += 1;
            let session = init.init(session, state.thumbs.next);
            session.stopped();
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, u64> for State {
    fn request(state: &mut Self, _: &Client, resource: &ExtImageCopyCaptureSessionV1, request: copy_session::Request, id: &u64, _: &DisplayHandle, init: &mut DataInit<'_, Self>) {
        match request {
            copy_session::Request::CreateFrame { frame } => {
                let frame = init.init(frame, *id);
                let Some(s) = state.thumbs.sessions.get_mut(id) else {
                    frame.failed(copy_frame::FailureReason::Stopped);
                    return;
                };
                if s.frame.is_some() {
                    resource.post_error(copy_session::Error::DuplicateFrame, "a frame of this session already exists");
                    return;
                }
                s.frame = Some(Frame { frame, buffer: None, asked: false });
            }
            copy_session::Request::Destroy => state.thumb_forget(*id),
            _ => {}
        }
    }

    fn destroyed(state: &mut Self, _: ClientId, _: &ExtImageCopyCaptureSessionV1, id: &u64) {
        state.thumb_forget(*id);
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, u64> for State {
    fn request(state: &mut Self, _: &Client, resource: &ExtImageCopyCaptureFrameV1, request: copy_frame::Request, id: &u64, _: &DisplayHandle, _: &mut DataInit<'_, Self>) {
        let mine = |s: &Session| s.frame.as_ref().is_some_and(|f| f.frame == *resource);
        match request {
            copy_frame::Request::AttachBuffer { buffer } => {
                if let Some(f) = state.thumbs.sessions.get_mut(id).filter(|s| mine(s)).and_then(|s| s.frame.as_mut()) {
                    f.buffer = Some(buffer);
                }
            }
            copy_frame::Request::Capture => {
                let Some(f) = state.thumbs.sessions.get_mut(id).filter(|s| mine(s)).and_then(|s| s.frame.as_mut()) else { return };
                if f.asked {
                    resource.post_error(copy_frame::Error::AlreadyCaptured, "this frame was already captured");
                    return;
                }
                if f.buffer.is_none() {
                    resource.post_error(copy_frame::Error::NoBuffer, "no buffer attached");
                    return;
                }
                f.asked = true;
                state.thumb_deliver(*id);
            }
            // Its frame let go before it was ready: the session may make another.
            copy_frame::Request::Destroy => {
                if let Some(s) = state.thumbs.sessions.get_mut(id).filter(|s| mine(s)) {
                    s.frame = None;
                }
            }
            _ => {}
        }
    }

    fn destroyed(state: &mut Self, _: ClientId, resource: &ExtImageCopyCaptureFrameV1, id: &u64) {
        if let Some(s) = state.thumbs.sessions.get_mut(id) {
            if s.frame.as_ref().is_some_and(|f| f.frame == *resource) {
                s.frame = None;
            }
        }
    }
}
