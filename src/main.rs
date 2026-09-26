//! pleamar-wm — pleamar with a Wayland compositor inside: a scene that says
//! `windows win max 6` holds other programs' windows, and where they go is the
//! scene's. Everything else —the command line, the scenes, the reloads— is
//! pleamar's own.

mod nest;

fn main() {
    pleamar::provide_windows(|max, to_render| {
        let tx = nest::start(max, to_render)?;
        Some(Box::new(move |m| {
            let _ = tx.send(m);
        }))
    });
    pleamar::run();
}
