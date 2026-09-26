//! A monitor of pleamar-wm's own session, and what is shown on it: the
//! surfaces of the scene —the scene's own, and the named ones: a bar, a
//! corner, a panel— each painted by the render in textures of its own, and
//! put together here, in their places and in the order of their levels, into
//! the buffer that goes to the screen. What layer-shell and the compositor
//! underneath did, done by hand.
//!
//! Each surface paints on its own (and only where something changed); the
//! monitor is put together at most once per refresh, on a thread of its own,
//! when something new has been painted. Where it ends up —the card's buffers
//! with page flips, or textures for a picture with no screen— is its `Output`.

use pleamar::scene::{Cursor, Keyboard, Level, Surface, SurfaceAnchor, ToRender};
use pleamar::wgpu;
use pleamar::{Frames, PlatformWindow};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Where a monitor's frames end up.
pub trait Output: Send {
    /// A buffer to put the next frame together in, and which one; none if all
    /// are still on screen or on their way.
    fn buffer(&mut self, device: &wgpu::Device, modifiers: &[u64]) -> Option<(usize, wgpu::Texture)>;
    /// Show that one once `done` has been done. Whether a flip is now on its
    /// way —its landing will be told— or it is already shown.
    fn show(&mut self, which: usize, done: wgpu::SubmissionIndex, device: &wgpu::Device, queue: &wgpu::Queue, anew: bool) -> bool;
}

/// One surface of the scene on a monitor.
pub struct Layer {
    pub sheet: u32,
    pub surface: usize,
    /// Where it looks from in the scene's plane.
    pub origin: (f32, f32),
    /// Where it is on the monitor: x, y, width, height.
    pub rect: [i32; 4],
    pub level: Level,
    pub main: bool,
    pub anchor: SurfaceAnchor,
    pub margin: [i32; 4],
    /// The last frame it painted.
    pub latest: Option<wgpu::Texture>,
    /// Where it takes the pointer, from its corner (its zones).
    pub region: Vec<[i32; 4]>,
}

pub struct ScreenState {
    pub name: String,
    pub size: (u32, u32),
    pub layers: Vec<Layer>,
    /// Something new has been painted since the last time it was put together.
    pub dirty: bool,
    /// No flip on its way.
    pub idle: bool,
    pub paused: bool,
    /// Back from another TTY: the monitor has to be given its mode again.
    pub anew: bool,
    /// The sheets whose new frame goes with the next flip, and the ones
    /// waiting for the flip on its way to land.
    pub fresh: Vec<u32>,
    pub on_flip: Vec<u32>,
    pub modifiers: Vec<u64>,
    output: Option<Box<dyn Output>>,
    pub quit: bool,
}

pub type Screen = Arc<(Mutex<ScreenState>, Condvar)>;

pub fn screen(name: String, size: (u32, u32), output: Box<dyn Output>) -> Screen {
    Arc::new((
        Mutex::new(ScreenState { name, size, layers: Vec::new(), dirty: false, idle: true, paused: false, anew: true, fresh: Vec::new(), on_flip: Vec::new(), modifiers: Vec::new(), output: Some(output), quit: false }),
        Condvar::new(),
    ))
}

/// Where a surface goes on a monitor of that size: its size (0 is all of
/// it, minus the margins) and its anchor, with the margins from the edges
/// it is attached to. Top, right, bottom, left.
pub fn place(s_size: (u32, u32), anchor: SurfaceAnchor, margin: [i32; 4], (mw, mh): (u32, u32)) -> [i32; 4] {
    let [top, right, bottom, left] = margin;
    let w = if s_size.0 == 0 { mw as i32 - left - right } else { s_size.0 as i32 };
    let h = if s_size.1 == 0 { mh as i32 - top - bottom } else { s_size.1 as i32 };
    let [l, t, r, b] = anchor.attached_edges();
    let x = if l { left } else if r { mw as i32 - w - right } else { (mw as i32 - w) / 2 };
    let y = if t { top } else if b { mh as i32 - h - bottom } else { (mh as i32 - h) / 2 };
    [x, y, w.max(1), h.max(1)]
}

pub fn level_rank(l: Level) -> u8 {
    match l {
        Level::Background => 0,
        Level::Below => 1,
        Level::Above => 2,
        Level::Overlay => 3,
    }
}

/// A layer for a surface of the scene on a monitor.
pub fn layer(sheet: u32, k: usize, s: &Surface, monitor: (u32, u32)) -> Layer {
    let main = s.name.is_empty();
    let rect = place((s.width, s.height), s.anchor, s.margin, monitor);
    Layer { sheet, surface: k, origin: s.origin, rect, level: s.level, main, anchor: s.anchor, margin: s.margin, latest: None, region: Vec::new() }
}

/// The layers in the order they are put together: by level, and within a
/// level the scene's own first, then in the order they were declared.
pub fn in_order(layers: &[Layer]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..layers.len()).collect();
    order.sort_by_key(|k| (level_rank(layers[*k].level), !layers[*k].main, layers[*k].surface));
    order
}

/// The pointer at that point of a monitor: which surface takes it —the
/// highest with a zone there; the scene's own if none—, and the point in
/// the scene's plane.
pub fn pointer_at(st: &ScreenState, (x, y): (f64, f64)) -> Option<(f32, f32)> {
    let order = in_order(&st.layers);
    let inside = |r: &[i32; 4], px: f64, py: f64| px >= r[0] as f64 && py >= r[1] as f64 && px < (r[0] + r[2]) as f64 && py < (r[1] + r[3]) as f64;
    let hit = order.iter().rev().map(|k| &st.layers[*k]).find(|l| {
        if l.main {
            return false;
        }
        let (lx, ly) = (x - l.rect[0] as f64, y - l.rect[1] as f64);
        l.latest.is_some() && inside(&l.rect, x, y) && l.region.iter().any(|b| lx >= b[0] as f64 && ly >= b[1] as f64 && lx < b[2] as f64 && ly < b[3] as f64)
    });
    let l = hit.or_else(|| st.layers.iter().find(|l| l.main))?;
    Some((l.origin.0 + (x - l.rect[0] as f64) as f32, l.origin.1 + (y - l.rect[1] as f64) as f32))
}

/// A surface's frames: two textures of its own, lent in turn to the render;
/// the last one painted is what the monitor puts together.
pub struct LayerFrames {
    pub screen: Screen,
    pub sheet: u32,
    pub size: (u32, u32),
    pub to_render: Sender<ToRender>,
    textures: Vec<wgpu::Texture>,
    latest: Option<usize>,
}

impl LayerFrames {
    pub fn new(screen: Screen, sheet: u32, size: (u32, u32), to_render: Sender<ToRender>) -> Self {
        LayerFrames { screen, sheet, size, to_render, textures: Vec::new(), latest: None }
    }
}

impl Frames for LayerFrames {
    fn acquire(&mut self, device: &wgpu::Device, modifiers: &[u64]) -> Option<(usize, wgpu::Texture)> {
        {
            let mut st = self.screen.0.lock().unwrap();
            if st.paused {
                return None;
            }
            if st.modifiers.is_empty() {
                st.modifiers = modifiers.to_vec();
            }
        }
        if self.textures.is_empty() {
            for _ in 0..2 {
                self.textures.push(device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("a surface's frame"),
                    size: wgpu::Extent3d { width: self.size.0.max(1), height: self.size.1.max(1), depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                }));
            }
        }
        let k = self.latest.map_or(0, |l| 1 - l);
        Some((k, self.textures[k].clone()))
    }

    fn present(&mut self, which: usize, _done: wgpu::SubmissionIndex, device: &wgpu::Device, queue: &wgpu::Queue) {
        self.latest = Some(which);
        let (lock, cv) = &*self.screen;
        let mut st = lock.lock().unwrap();
        if let Some(l) = st.layers.iter_mut().find(|l| l.sheet == self.sheet) {
            l.latest = Some(self.textures[which].clone());
        }
        st.fresh.push(self.sheet);
        st.dirty = true;
        // The first frame starts the one that puts the monitor together.
        if let Some(output) = st.output.take() {
            let (screen, device, queue, to_render) = (self.screen.clone(), device.clone(), queue.clone(), self.to_render.clone());
            let name = st.name.clone();
            let _ = std::thread::Builder::new().name(format!("screen {name}")).spawn(move || compose_loop(screen, output, device, queue, to_render));
        }
        cv.notify_all();
    }
}

/// A surface's sheet as the render sees it: its zones are where it takes the
/// pointer; the rest goes to what is below.
pub struct LayerWindow {
    pub screen: Screen,
    pub sheet: u32,
    pub cursor: Arc<Mutex<Cursor>>,
}

impl PlatformWindow for LayerWindow {
    fn update_input_region(&self, boxes: &[[i32; 4]]) {
        let mut st = self.screen.0.lock().unwrap();
        if let Some(l) = st.layers.iter_mut().find(|l| l.sheet == self.sheet) {
            l.region = boxes.to_vec();
        }
    }
    fn cursor(&self, c: Cursor) {
        *self.cursor.lock().unwrap() = c;
    }
    fn keyboard(&self, _: Keyboard) {}
}

const SHADER: &str = r#"
struct Rect { r: vec4<f32> };
@group(0) @binding(0) var t: texture_2d<f32>;
@group(0) @binding(1) var s: sampler;
@group(0) @binding(2) var<uniform> q: Rect;
struct V { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex fn vs(@builtin(vertex_index) i: u32) -> V {
    let c = vec2<f32>(f32(i & 1u), f32((i >> 1u) & 1u));
    let p = mix(q.r.xy, q.r.zw, c);
    var v: V;
    v.pos = vec4<f32>(p.x * 2.0 - 1.0, 1.0 - p.y * 2.0, 0.0, 1.0);
    v.uv = c;
    return v;
}
@fragment fn fs(v: V) -> @location(0) vec4<f32> {
    return textureSample(t, s, v.uv);
}
"#;

/// Puts the monitor together whenever something new has been painted and
/// the last flip has landed: at most once per refresh.
fn compose_loop(screen: Screen, mut output: Box<dyn Output>, device: wgpu::Device, queue: wgpu::Queue, to_render: Sender<ToRender>) {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("screen"), source: wgpu::ShaderSource::Wgsl(SHADER.into()) });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("screen"),
        layout: None,
        vertex: wgpu::VertexState { module: &module, entry_point: Some("vs"), compilation_options: Default::default(), buffers: &[] },
        primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs"),
            compilation_options: Default::default(),
            // What the render paints is premultiplied.
            targets: &[Some(wgpu::ColorTargetState { format: wgpu::TextureFormat::Bgra8Unorm, blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING), write_mask: wgpu::ColorWrites::ALL })],
        }),
        multiview_mask: None,
        cache: None,
    });
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor { mag_filter: wgpu::FilterMode::Nearest, min_filter: wgpu::FilterMode::Nearest, ..Default::default() });
    let (lock, cv) = &*screen;
    loop {
        let (layers, size, modifiers, fresh, anew) = {
            let mut st = lock.lock().unwrap();
            while !st.quit && !(st.dirty && st.idle && !st.paused) {
                st = cv.wait_timeout(st, Duration::from_millis(500)).unwrap().0;
            }
            if st.quit {
                return;
            }
            st.dirty = false;
            let order = in_order(&st.layers);
            let layers: Vec<(wgpu::Texture, [i32; 4])> = order.iter().filter_map(|k| st.layers[*k].latest.clone().map(|t| (t, st.layers[*k].rect))).collect();
            if std::env::var_os("PLEAMAR_DEBUG_SCREEN").is_some() {
                eprintln!("screen · {}: {:?}", st.name, layers.iter().map(|(t, r)| (r, t.size().width, t.size().height)).collect::<Vec<_>>());
            }
            let anew = std::mem::take(&mut st.anew);
            (layers, st.size, st.modifiers.clone(), std::mem::take(&mut st.fresh), anew)
        };
        let Some((which, target)) = output.buffer(&device, &modifiers) else {
            // Nothing to put it together in yet: again as soon as there is.
            let mut st = lock.lock().unwrap();
            st.dirty = true;
            st.fresh.extend(fresh);
            drop(st);
            std::thread::sleep(Duration::from_millis(2));
            continue;
        };
        let view = target.create_view(&Default::default());
        let mut encoder = device.create_command_encoder(&Default::default());
        let groups: Vec<wgpu::BindGroup> = layers
            .iter()
            .map(|(t, r)| {
                let (w, h) = (size.0 as f32, size.1 as f32);
                let rect = [r[0] as f32 / w, r[1] as f32 / h, (r[0] + r[2]) as f32 / w, (r[1] + r[3]) as f32 / h];
                let uniform = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: 16, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
                queue.write_buffer(&uniform, 0, bytemuck_f32(&rect));
                let tv = t.create_view(&Default::default());
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: None,
                    layout: &pipeline.get_bind_group_layout(0),
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&tv) },
                        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
                        wgpu::BindGroupEntry { binding: 2, resource: uniform.as_entire_binding() },
                    ],
                })
            })
            .collect();
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("screen"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&pipeline);
            for g in &groups {
                pass.set_bind_group(0, g, &[]);
                pass.draw(0..4, 0..1);
            }
        }
        let done = queue.submit(Some(encoder.finish()));
        let flying = output.show(which, done, &device, &queue, anew);
        let mut st = lock.lock().unwrap();
        if flying {
            st.idle = false;
            st.on_flip.extend(fresh);
        } else {
            for id in fresh {
                let _ = to_render.send(ToRender::Frame(id));
            }
        }
    }
}

/// A flip has landed on this monitor: the sheets whose frame it carried may
/// paint the next one, and the monitor can be put together again.
pub fn landed(screen: &Screen, to_render: &Sender<ToRender>) {
    let (lock, cv) = &**screen;
    let mut st = lock.lock().unwrap();
    st.idle = true;
    for id in st.on_flip.drain(..) {
        let _ = to_render.send(ToRender::Frame(id));
    }
    cv.notify_all();
}

fn bytemuck_f32(v: &[f32; 4]) -> &[u8] {
    // SAFETY: four f32 are sixteen bytes, laid out as the uniform wants them.
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, 16) }
}
