//! `pleamar-wm headless scene.plm [options]`: the session's way of showing a
//! scene —each surface painted in frames of its own, the monitor put together
//! from them— with no screen at all. After `PLEAMAR_HEADLESS_AT` seconds (8 by
//! default) what each monitor shows goes to a PNG (`PLEAMAR_HEADLESS_PNG`, by
//! default /tmp/pleamar-headless.png; `-1`, `-2`… for the others).
//! `PLEAMAR_HEADLESS_SCREENS=2` makes two monitors. It is how the session is
//! checked without a TTY. `PLEAMAR_HEADLESS_INPUT` is a mouse and a keyboard
//! that go where the session's real ones would (`route.rs`): to the scene, or
//! to a program's surface —Marea's finder, her catcher—.

use crate::screen::{self, LayerFrames, LayerWindow, Output, Screen};
use pleamar::scene::{Cursor, Screens, Surface, ToRender};
use pleamar::wgpu;
use pleamar::{NewSheet, Target, View};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct Headless;

/// A monitor with no screen: three textures, "flipped" at its refresh, and a picture.
struct Offscreen {
    size: (u32, u32),
    textures: Vec<wgpu::Texture>,
    shown: Option<usize>,
    started: Instant,
    at: f32,
    written: bool,
    taken: usize,
    path: String,
    /// Its monitor, once made: what it tells when a flip "lands".
    screen: Arc<std::sync::OnceLock<Screen>>,
    to_render: Sender<ToRender>,
    /// How long a refresh lasts on it.
    period: Duration,
}

impl Output for Offscreen {
    fn buffer(&mut self, device: &wgpu::Device, _: &[u64]) -> Option<(usize, wgpu::Texture)> {
        if self.textures.is_empty() {
            for _ in 0..3 {
                self.textures.push(device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("headless monitor"),
                    size: wgpu::Extent3d { width: self.size.0, height: self.size.1, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                    view_formats: &[],
                }));
            }
        }
        let k = self.shown.map_or(0, |s| (s + 1) % 3);
        Some((k, self.textures[k].clone()))
    }

    fn show(&mut self, which: usize, done: pleamar::Sent, device: &wgpu::Device, queue: &wgpu::Queue, _: bool) -> bool {
        done.wait(device, Duration::from_millis(200));
        self.shown = Some(which);
        if std::env::var_os("PLEAMAR_DEBUG_SCREEN").is_some() {
            eprintln!("headless · {} shown {which} at {:.3} s", self.path, self.started.elapsed().as_secs_f32());
        }
        // `PLEAMAR_HEADLESS_FRAMES=n`: n frames in a row from then on, -f0, -f1…
        let frames = std::env::var("PLEAMAR_HEADLESS_FRAMES").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(1);
        if self.taken < frames && self.started.elapsed().as_secs_f32() >= self.at {
            let path = if frames > 1 { self.path.replace(".png", &format!("-f{}.png", self.taken)) } else { self.path.clone() };
            self.taken += 1;
            self.written = true;
            match write_png(device, queue, &self.textures[which], self.size, &path) {
                Ok(()) => println!("headless · the frame shown went to {path}"),
                Err(e) => eprintln!("headless · {e}"),
            }
        }
        // The flip "lands" on the monitor's next refresh.
        if let Some(sc) = self.screen.get().cloned() {
            let (tx, period) = (self.to_render.clone(), self.period);
            std::thread::spawn(move || {
                std::thread::sleep(period);
                screen::landed(&sc, &tx);
            });
            return true;
        }
        false
    }
}

fn write_png(device: &wgpu::Device, queue: &wgpu::Queue, texture: &wgpu::Texture, (w, h): (u32, u32), path: &str) -> Result<(), String> {
    let row = (w * 4).div_ceil(256) * 256;
    let out = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: (row * h) as u64, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
    let mut encoder = device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo { texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        wgpu::TexelCopyBufferInfo { buffer: &out, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: None } },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    queue.submit(Some(encoder.finish()));
    let sent = pleamar::Sent::after(queue);
    let mapped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = mapped.clone();
    out.slice(..).map_async(wgpu::MapMode::Read, move |_| flag.store(true, std::sync::atomic::Ordering::Release));
    sent.wait(device, Duration::from_secs(2));
    let start = std::time::Instant::now();
    while !mapped.load(std::sync::atomic::Ordering::Acquire) && start.elapsed() < Duration::from_secs(2) {
        let _ = device.poll(wgpu::PollType::Poll);
        std::thread::sleep(Duration::from_micros(200));
    }
    let data = out.slice(..).get_mapped_range().map_err(|e| format!("{e:?}"))?;
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for px in data[(y * row) as usize..(y * row + w * 4) as usize].chunks_exact(4) {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
    }
    image::save_buffer(path, &rgba, w, h, image::ColorType::Rgba8).map_err(|e| e.to_string())
}

impl pleamar::Platform for Headless {
    fn run(self: Box<Self>, surfaces: Vec<Surface>, _: u32, _: wgpu::Instance, to_render: Sender<ToRender>) {
        let size = (1920u32, 1080u32);
        // `PLEAMAR_HEADLESS_SCALE=1.5`: monitors of that scale, to check HiDPI.
        let scale = std::env::var("PLEAMAR_HEADLESS_SCALE").ok().and_then(|v| v.parse::<f64>().ok()).filter(|s| *s > 0.0).unwrap_or(1.0);
        let unit_w = (size.0 as f64 / scale).round() as i32;
        let count = std::env::var("PLEAMAR_HEADLESS_SCREENS").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(1);
        let png = std::env::var("PLEAMAR_HEADLESS_PNG").unwrap_or_else(|_| "/tmp/pleamar-headless.png".into());
        let at = std::env::var("PLEAMAR_HEADLESS_AT").ok().and_then(|v| v.parse::<f32>().ok()).unwrap_or(8.0);
        // `PLEAMAR_HEADLESS_HZ=165,60`: each monitor's refresh (60 by default),
        // to see a fast one beside a slow one, as on a real desk.
        let hz: std::sync::Arc<Vec<f64>> = std::env::var("PLEAMAR_HEADLESS_HZ").unwrap_or_default().split(',').filter_map(|v| v.trim().parse::<f64>().ok()).filter(|v| *v > 1.0).collect::<Vec<f64>>().into();
        let mhz_of = move |m: usize| (hz.get(m).copied().unwrap_or(60.0) * 1000.0).round() as i32;
        let screens: Vec<Screen> = (0..count)
            .map(|m| {
                let path = if m == 0 { png.clone() } else { png.replace(".png", &format!("-{m}.png")) };
                let own = Arc::new(std::sync::OnceLock::new());
                let output = Offscreen { size, textures: Vec::new(), shown: None, started: Instant::now(), at, written: false, taken: 0, path, screen: own.clone(), to_render: to_render.clone(), period: Duration::from_secs_f64(1000.0 / mhz_of(m) as f64) };
                let sc = screen::screen(format!("HEADLESS-{}", m + 1), size, Box::new(output));
                let _ = own.set(sc.clone());
                sc
            })
            .collect();
        crate::layers::register(screens.iter().enumerate().map(|(m, sc)| (crate::layers::MonitorInfo { name: format!("HEADLESS-{}", m + 1), size, x: m as i32 * unit_w, y: 0, mhz: mhz_of(m), scale }, sc.clone())).collect());
        let cursor = Arc::new(Mutex::new(Cursor::Normal));
        let mut id = 7000;
        // Which sheets are on each monitor: to take one away, as if unplugged.
        let mut sheets_on: Vec<Vec<u32>> = vec![Vec::new(); count];
        for (k, s) in surfaces.iter().enumerate() {
            let on: Vec<usize> = match &s.screens {
                Screens::Number(n) => vec![*n],
                Screens::All if !s.name.is_empty() => (0..count).collect(),
                _ => vec![0],
            };
            for which in on {
                let Some(sc) = screens.get(which) else { continue };
                if s.name.is_empty() && sc.0.lock().unwrap().layers.iter().any(|l| l.main) {
                    continue;
                }
                id += 1;
                let layer = screen::layer(id, k, s, size, scale as f32);
                let lsize = (layer.rect[2] as u32, layer.rect[3] as u32);
                let units = layer.units;
                println!("headless · monitor {which}: surface {k} '{}' {}×{} at {},{}", s.name, lsize.0, lsize.1, layer.rect[0], layer.rect[1]);
                sc.0.lock().unwrap().layers.push(layer);
                sheets_on[which].push(id);
                let _ = to_render.send(ToRender::Sheet(Box::new(NewSheet {
                    id,
                    target: Target::Frames(Box::new(LayerFrames::new(sc.clone(), id, lsize, to_render.clone()))),
                    window: Box::new(LayerWindow { screen: sc.clone(), sheet: id, cursor: cursor.clone() }),
                    scale: scale as f32,
                    size: units,
                    mhz: mhz_of(which),
                    name: format!("HEADLESS-{}", which + 1),
                    view: View { surface: k, popup: None, origin: s.origin, size: (units.0 as f32, units.1 as f32) },
                })));
            }
        }
        // `PLEAMAR_HEADLESS_UNPLUG=s`: the last monitor goes away after that
        // long, as a session sees one unplugged: its surfaces are taken from the
        // render, and the compositor inside is told.
        if let Some(after) = std::env::var("PLEAMAR_HEADLESS_UNPLUG").ok().and_then(|v| v.parse::<f32>().ok()).filter(|_| count > 1) {
            let (tx, screens, sheets_on) = (to_render.clone(), screens.clone(), sheets_on.clone());
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs_f32(after));
                let last = screens.len() - 1;
                println!("headless · monitor HEADLESS-{} unplugged", last + 1);
                for id in &sheets_on[last] {
                    let _ = tx.send(ToRender::SheetGone(*id));
                }
                {
                    let (lock, cv) = &*screens[last];
                    lock.lock().unwrap().quit = true;
                    cv.notify_all();
                }
                crate::layers::register(screens[..last].iter().enumerate().map(|(m, sc)| (crate::layers::MonitorInfo { name: format!("HEADLESS-{}", m + 1), size, x: m as i32 * unit_w, y: 0, mhz: mhz_of(m), scale }, sc.clone())).collect());
                crate::layers::tell(crate::layers::ToLayers::Monitors);
            });
        }
        // `PLEAMAR_HEADLESS_INPUT="900,40@3000 down@3500 up@3600 key:Escape:1@4000"`:
        // a mouse and a keyboard, through the same road as the session's
        // (`route.rs`): to the scene or to a program's surface —a bar, Marea—
        // as the real ones would go. Points in units of the whole desktop; a
        // key by its keysym's name and evdev code (it types itself if the name
        // is one character).
        if let Ok(script) = std::env::var("PLEAMAR_HEADLESS_INPUT") {
            let (tx, screens) = (to_render.clone(), screens.clone());
            std::thread::spawn(move || {
                let start = Instant::now();
                let mut route = crate::route::Route::new(tx);
                for step in script.split_whitespace() {
                    let Some((what, ms)) = step.rsplit_once('@') else { continue };
                    let Ok(ms) = ms.parse::<u64>() else { continue };
                    if let Some(wait) = Duration::from_millis(ms).checked_sub(start.elapsed()) {
                        std::thread::sleep(wait);
                    }
                    match what {
                        "down" | "up" => {
                            route.button(&screens, 0x110, what == "down");
                        }
                        // The mouse's side buttons: back and forward.
                        "back" | "forward" => {
                            let code = if what == "back" { 0x113 } else { 0x114 };
                            route.button(&screens, code, true);
                            route.button(&screens, code, false);
                        }
                        // `wheel:-1` a notch down (as the session tells it: up is positive).
                        _ if what.starts_with("wheel:") => {
                            if let Ok(n) = what[6..].parse::<f32>() {
                                route.wheel(n);
                            }
                        }
                        _ if what.starts_with("key:") => {
                            let mut parts = what[4..].splitn(2, ':');
                            let name = parts.next().unwrap_or("");
                            let Some(code) = parts.next().and_then(|c| c.parse::<u32>().ok()) else { continue };
                            // `key:Super+Shift+q:16`: modifiers before the name.
                            let (mods_part, name) = name.rsplit_once('+').unwrap_or(("", name));
                            let mut mods = pleamar::scene::Mods::default();
                            for m in mods_part.split('+') {
                                match m.to_lowercase().as_str() {
                                    "super" => mods.logo = true,
                                    "ctrl" => mods.ctrl = true,
                                    "alt" => mods.alt = true,
                                    "shift" => mods.shift = true,
                                    _ => {}
                                }
                            }
                            let typed = Some(name.to_owned()).filter(|n| n.chars().count() == 1 && !mods.logo && !mods.ctrl && !mods.alt);
                            println!("headless · key {name} → {:?}", route.key_owner(&screens));
                            if std::env::var_os("PLEAMAR_DEBUG_WINDOWS").is_some() {
                                for (m, s) in screens.iter().enumerate() {
                                    for c in &s.0.lock().unwrap().clients {
                                        println!("headless ·   monitor {m}: surface {} level {} keyboard {} pieces {}", c.id, c.level, c.keyboard, c.pieces.len());
                                    }
                                }
                            }
                            route.key(&screens, name, typed, mods, code, true);
                            route.key(&screens, name, None, mods, code, false);
                        }
                        _ => {
                            let Some((x, y)) = what.split_once(',').and_then(|(x, y)| Some((x.parse::<f64>().ok()?, y.parse::<f64>().ok()?))) else { continue };
                            let m = ((x / unit_w as f64).floor().max(0.0) as usize).min(screens.len() - 1);
                            let (mx, my) = ((x - (m as i32 * unit_w) as f64) * scale, y * scale);
                            route.pointer(&screens[m], (mx, my));
                            println!("headless · pointer {x},{y} → {:?}", route.hit);
                        }
                    }
                }
            });
        }
        // At the hour of the picture everything is painted, so that a monitor
        // where nothing moves shows its frame all the same.
        let tx = to_render.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs_f32(at + 0.05));
            let _ = tx.send(ToRender::Repaint);
        });
        let _ = to_render.send(ToRender::KeyboardFocus(true));
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
}
