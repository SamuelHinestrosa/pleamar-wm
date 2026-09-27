//! pleamar-wm — pleamar with a Wayland compositor inside: a scene that says
//! `windows win max 6` holds other programs' windows, and where they go is the
//! scene's. Everything else —the command line, the scenes, the reloads— is
//! pleamar's own.

mod config;
mod nest;
mod headless;
mod layers;
mod probe;
mod route;
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
    // `pleamar-wm hyprctl monitors|activewindow`: the session's desktop, said
    // the way Hyprland says it, for what used to ask Hyprland (Marea's
    // screenshots) and now runs here.
    if args.first().map(String::as_str) == Some("hyprctl") {
        std::process::exit(hyprctl(args.get(1).map(String::as_str).unwrap_or("")));
    }
    // `pleamar-wm config`: the session's configuration as it is understood.
    if args.first().map(String::as_str) == Some("config") {
        println!("{:#?}", config::get());
        return;
    }
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
        nest::set_scene_name(&scene);
        layers::expect_monitors();
        pleamar::provide_before_quit(Box::new(|| {
            layers::stop_all();
            nest::stop_launched();
        }));
        pleamar::provide_platform(Box::new(headless::Headless));
        let mut options = vec!["--scene".to_owned(), scene, "--no-hud".to_owned()];
        options.extend(args);
        pleamar::run_with(options);
        return;
    }
    if args.first().map(String::as_str) == Some("session") {
        args.remove(0);
        let scene = if args.first().is_some_and(|a| !a.starts_with("--")) { args.remove(0) } else { "examples/session.plm".into() };
        nest::set_scene_name(&scene);
        layers::expect_monitors();
        pleamar::provide_before_quit(Box::new(|| {
            layers::stop_all();
            nest::stop_launched();
        }));
        pleamar::provide_platform(Box::new(session::Session));
        let mut options = vec!["--scene".to_owned(), scene, "--no-hud".to_owned()];
        options.extend(args);
        pleamar::run_with(options);
    } else {
        pleamar::run();
    }
}

fn hyprctl(what: &str) -> i32 {
    let Some(Ok(text)) = nest::desktop_file().map(std::fs::read_to_string) else {
        eprintln!("pleamar-wm's session is not running");
        return 1;
    };
    match what {
        "monitors" => {
            for (id, line) in text.lines().filter_map(|l| l.strip_prefix("monitor ")).enumerate() {
                let f: Vec<&str> = line.split(' ').collect();
                let [name, w, h, x, y, mhz, focused, scale] = f[..] else { continue };
                let hz = mhz.parse::<f64>().unwrap_or(60_000.0) / 1000.0;
                let scale = scale.parse::<f64>().unwrap_or(1.0);
                println!("Monitor {name} (ID {id}):\n\t{w}x{h}@{hz:.5} at {x}x{y}\n\tscale: {scale:.2}\n\tfocused: {}\n", if focused == "1" { "yes" } else { "no" });
            }
            0
        }
        "activewindow" => {
            match text.lines().find_map(|l| l.strip_prefix("window ")) {
                Some(line) => {
                    let f: Vec<&str> = line.splitn(5, ' ').collect();
                    let [x, y, w, h, names] = f[..] else { return 1 };
                    let (app, title) = names.split_once('\t').unwrap_or((names, ""));
                    println!("Window 0 -> {title}:\n\tat: {x},{y}\n\tsize: {w},{h}\n\tclass: {app}\n\ttitle: {title}\n");
                }
                None => println!("Invalid"),
            }
            0
        }
        _ => {
            eprintln!("pleamar-wm hyprctl monitors | activewindow");
            1
        }
    }
}
