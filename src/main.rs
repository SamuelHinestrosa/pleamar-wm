//! pleamar-wm — pleamar with a Wayland compositor inside: a scene that says
//! `windows win max 6` holds other programs' windows, and where they go is the
//! scene's. Everything else —the command line, the scenes, the reloads— is
//! pleamar's own.

mod nest;
mod headless;
mod layers;
mod probe;
mod screen;
mod session;

fn main() {
    pleamar::provide_windows(|max, to_render| {
        let tx = nest::start(max, to_render)?;
        Some(Box::new(move |m| {
            let _ = tx.send(m);
        }))
    });
    // `pleamar-wm session scene.plm [options]`: a session of its own, from a
    // TTY, without a compositor underneath. Otherwise, pleamar as ever.
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("probe") {
        if let Err(e) = probe::run() {
            eprintln!("probe · {e}");
            std::process::exit(1);
        }
        return;
    }
    // `pleamar-wm headless scene.plm [options]`: the session's painting with no screen, to check it.
    if args.first().map(String::as_str) == Some("headless") {
        args.remove(0);
        let scene = if args.first().is_some_and(|a| !a.starts_with("--")) { args.remove(0) } else { "examples/session.plm".into() };
        pleamar::provide_platform(Box::new(headless::Headless));
        let mut options = vec!["--scene".to_owned(), scene, "--no-hud".to_owned()];
        options.extend(args);
        pleamar::run_with(options);
        return;
    }
    if args.first().map(String::as_str) == Some("session") {
        args.remove(0);
        let scene = if args.first().is_some_and(|a| !a.starts_with("--")) { args.remove(0) } else { "examples/session.plm".into() };
        pleamar::provide_platform(Box::new(session::Session));
        let mut options = vec!["--scene".to_owned(), scene, "--no-hud".to_owned()];
        options.extend(args);
        pleamar::run_with(options);
    } else {
        pleamar::run();
    }
}
