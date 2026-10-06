//! Where the mouse is on the whole desktop, for the programs of the session
//! that want to know (Marea dozes off, and wakes when the mouse moves
//! anywhere; her eyes follow it). Wayland does not tell a surface where the
//! pointer is when it is not over it —on purpose—, and Hyprland is asked
//! through its socket; here it is `cursor.sock`, next to the session's
//! other sockets (`PLEAMAR_SOCKETS`).
//!
//! Whoever connects gets a line, `x y` in units of the desktop, each time the
//! mouse moves, at most sixty times a second, and the first one at once. It
//! costs nothing while nobody listens: each listener is a thread that looks
//! sixty times a second at where the pointer was last seen, and goes when
//! its program closes the socket. The monitors, to place it, are in the
//! `desktop` file beside it, as `pleamar-wm hyprctl` reads them.

use std::io::Write;
use std::os::unix::net::UnixListener;
use std::sync::OnceLock;
use std::time::Duration;

static SERVING: OnceLock<()> = OnceLock::new();

/// Starts listening in `dir` (once): `dir/cursor.sock`.
pub fn serve(dir: &str) {
    let dir = dir.to_owned();
    SERVING.get_or_init(move || {
        let path = format!("{dir}/cursor.sock");
        let _ = std::fs::remove_file(&path);
        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("cursor · {path} could not be opened: {e}");
                return;
            }
        };
        let _ = std::thread::Builder::new().name("cursor".into()).spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = std::thread::Builder::new().name("cursor·one".into()).spawn(move || tell(stream));
            }
        });
    });
}

/// One listener: where the pointer is, each time it moves, until it goes.
fn tell(mut stream: std::os::unix::net::UnixStream) {
    let mut last = u64::MAX;
    loop {
        if let Some(p) = crate::layers::pointer_seen() {
            if p.moves != last {
                last = p.moves;
                if writeln!(stream, "{:.1} {:.1}", p.at.0, p.at.1).is_err() {
                    return;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(16));
    }
}
