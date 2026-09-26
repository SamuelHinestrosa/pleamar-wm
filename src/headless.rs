//! `pleamar-wm headless scene.plm [options]`: the session's way of painting
//! —frames lent to the render, three of them, each painted again only where
//! something changed— with no screen at all. After `PLEAMAR_HEADLESS_AT`
//! seconds (8 by default) the frame shown goes to a PNG
//! (`PLEAMAR_HEADLESS_PNG`, by default /tmp/pleamar-headless.png). It is how
//! the session's painting is checked without a TTY: against the same run with
//! `PLEAMAR_FULL_REPAINT=1`.

use pleamar::scene::{Cursor, Keyboard, Surface, ToRender};
use pleamar::wgpu;
use pleamar::{Frames, NewSheet, PlatformWindow, Target, View};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

pub struct Headless;

struct HeadlessFrames {
    flipper: Option<std::sync::mpsc::Sender<wgpu::SubmissionIndex>>,
    size: (u32, u32),
    textures: Vec<wgpu::Texture>,
    shown: Option<usize>,
    started: Instant,
    written: bool,
    path: String,
    to_render: Sender<ToRender>,
    id: u32,
}

impl Frames for HeadlessFrames {
    fn acquire(&mut self, device: &wgpu::Device, _: &[u64]) -> Option<(usize, wgpu::Texture)> {
        if self.textures.is_empty() {
            for _ in 0..3 {
                self.textures.push(device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("headless frame"),
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
        // In turn, as a monitor's: never the one on screen.
        let k = self.shown.map_or(0, |s| (s + 1) % 3);
        Some((k, self.textures[k].clone()))
    }

    fn present(&mut self, which: usize, done: wgpu::SubmissionIndex, device: &wgpu::Device, queue: &wgpu::Queue) {
        // As the session: a thread of its own waits for the card and "flips".
        if self.flipper.is_none() {
            let (tx, rx) = std::sync::mpsc::channel::<wgpu::SubmissionIndex>();
            let (device, to_render, id) = (device.clone(), self.to_render.clone(), self.id);
            let _ = std::thread::Builder::new().name("flips HEADLESS-1".into()).spawn(move || {
                let mut next = Instant::now();
                for done in rx {
                    let _ = device.poll(wgpu::PollType::Wait { submission_index: Some(done), timeout: Some(Duration::from_millis(200)) });
                    // A monitor at 60 Hz: the flip lands on the next refresh.
                    next = (next + Duration::from_micros(16_667)).max(Instant::now());
                    std::thread::sleep(next.saturating_duration_since(Instant::now()));
                    let _ = to_render.send(ToRender::Frame(id));
                }
            });
            self.flipper = Some(tx);
        }
        self.shown = Some(which);
        let at = std::env::var("PLEAMAR_HEADLESS_AT").ok().and_then(|v| v.parse::<f32>().ok()).unwrap_or(8.0);
        if !self.written && self.started.elapsed().as_secs_f32() >= at {
            self.written = true;
            let path = self.path.clone();
            let _ = device.poll(wgpu::PollType::Wait { submission_index: Some(done.clone()), timeout: Some(Duration::from_millis(200)) });
            match write_png(device, queue, &self.textures[which], self.size, &path) {
                Ok(()) => println!("headless · the frame shown went to {path}"),
                Err(e) => eprintln!("headless · {e}"),
            }
        }
        if let Some(tx) = &self.flipper {
            let _ = tx.send(done);
        }
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
    let index = queue.submit(Some(encoder.finish()));
    out.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    let _ = device.poll(wgpu::PollType::Wait { submission_index: Some(index), timeout: Some(Duration::from_secs(2)) });
    let data = out.slice(..).get_mapped_range().map_err(|e| format!("{e:?}"))?;
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for px in data[(y * row) as usize..(y * row + w * 4) as usize].chunks_exact(4) {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
    }
    image::save_buffer(path, &rgba, w, h, image::ColorType::Rgba8).map_err(|e| e.to_string())
}

struct Nothing;

impl PlatformWindow for Nothing {
    fn update_input_region(&self, _: &[[i32; 4]]) {}
    fn cursor(&self, _: Cursor) {}
    fn keyboard(&self, _: Keyboard) {}
}

impl pleamar::Platform for Headless {
    fn run(self: Box<Self>, surfaces: Vec<Surface>, _: u32, _: wgpu::Instance, to_render: Sender<ToRender>) {
        let size = (1920u32, 1080u32);
        // `PLEAMAR_HEADLESS_SCREENS=2`: two monitors, each copy of the scene on its own.
        let screens = std::env::var("PLEAMAR_HEADLESS_SCREENS").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(1);
        let png = std::env::var("PLEAMAR_HEADLESS_PNG").unwrap_or_else(|_| "/tmp/pleamar-headless.png".into());
        for m in 0..screens {
            let Some((k, s)) = surfaces.iter().enumerate().find(|(_, s)| s.name.is_empty() && matches!(s.screens, pleamar::scene::Screens::Number(n) if n == m)).or_else(|| (m == 0).then(|| surfaces.iter().enumerate().find(|(_, s)| s.name.is_empty())).flatten()) else { continue };
            let id = 7000 + m as u32;
            let path = if m == 0 { png.clone() } else { png.replace(".png", &format!("-{m}.png")) };
            println!("headless · monitor {m}: the scene's surface {k}, from {:?} in its plane", s.origin);
            let frames = HeadlessFrames { flipper: None, size, textures: Vec::new(), shown: None, started: Instant::now(), written: false, to_render: to_render.clone(), id, path };
            let _ = to_render.send(ToRender::Sheet(Box::new(NewSheet {
                id,
                target: Target::Frames(Box::new(frames)),
                window: Box::new(Nothing),
                scale: 1.0,
                size,
                mhz: 60_000,
                name: format!("HEADLESS-{}", m + 1),
                view: View { surface: k, popup: None, origin: s.origin, size: (size.0 as f32, size.1 as f32) },
            })));
        }
        // At the hour of the picture everything is painted, so that a monitor
        // where nothing moves shows its frame all the same.
        let at = std::env::var("PLEAMAR_HEADLESS_AT").ok().and_then(|v| v.parse::<f32>().ok()).unwrap_or(8.0);
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
