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

use crate::layers::{self, ClientLayer, ToLayers};
use pleamar::scene::{Cursor, Keyboard, Level, PieceContent, Surface, SurfaceAnchor, ToRender};
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
    fn show(&mut self, which: usize, done: pleamar::Sent, device: &wgpu::Device, queue: &wgpu::Queue, anew: bool) -> bool;
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
    /// The programs' surfaces on it (layer-shell): Marea, a bar, a wallpaper.
    pub clients: Vec<ClientLayer>,
    /// The programs' buffers it read the last time it was put together: lent
    /// until it no longer shows them.
    pub held: Vec<u64>,
    /// Buffers the programs destroyed, to drop what was kept of them.
    pub forget: Vec<u64>,
    output: Option<Box<dyn Output>>,
    pub quit: bool,
}

pub type Screen = Arc<(Mutex<ScreenState>, Condvar)>;

pub fn screen(name: String, size: (u32, u32), output: Box<dyn Output>) -> Screen {
    Arc::new((
        Mutex::new(ScreenState { name, size, layers: Vec::new(), dirty: false, idle: true, paused: false, anew: true, fresh: Vec::new(), on_flip: Vec::new(), modifiers: Vec::new(), clients: Vec::new(), held: Vec::new(), forget: Vec::new(), output: Some(output), quit: false }),
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

/// Who takes the pointer at a point of a monitor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hit {
    /// The scene, at that point of its plane (none: nowhere).
    Scene(Option<(f32, f32)>),
    /// A program's surface, at that point of it.
    Client(u64, (f64, f64)),
}

/// What is put together on a monitor, bottom to top: by level; within a
/// level the scene's own surface, then its named ones in the order they were
/// declared, then the programs' in the order they came.
enum Item {
    Scene(usize),
    Client(usize),
}

fn stacked(st: &ScreenState) -> Vec<Item> {
    let mut all: Vec<((u8, u8, usize), Item)> = Vec::new();
    // Locked, only the lock screen: nothing else is seen or touched.
    if layers::locked() {
        return st.clients.iter().enumerate().filter(|(_, c)| c.level >= 4).map(|(k, _)| Item::Client(k)).collect();
    }
    for (k, l) in st.layers.iter().enumerate() {
        all.push(((level_rank(l.level), if l.main { 0 } else { 1 }, l.surface), Item::Scene(k)));
    }
    for (k, c) in st.clients.iter().enumerate() {
        all.push(((c.level, 2, k), Item::Client(k)));
    }
    all.sort_by_key(|(key, _)| *key);
    all.into_iter().map(|(_, i)| i).collect()
}

/// The pointer at that point of a monitor: the highest surface that takes
/// it there —a named one only where it has zones, a program's where its
/// input region says—; the scene's own takes whatever is left above it.
pub fn pointer_at(st: &ScreenState, (x, y): (f64, f64)) -> Hit {
    let inside = |r: &[i32; 4], px: f64, py: f64| px >= r[0] as f64 && py >= r[1] as f64 && px < (r[0] + r[2]) as f64 && py < (r[1] + r[3]) as f64;
    let scene = |l: &Layer| Hit::Scene(Some((l.origin.0 + (x - l.rect[0] as f64) as f32, l.origin.1 + (y - l.rect[1] as f64) as f32)));
    for item in stacked(st).iter().rev() {
        match item {
            Item::Scene(k) => {
                let l = &st.layers[*k];
                if l.main {
                    return scene(l);
                }
                let (lx, ly) = (x - l.rect[0] as f64, y - l.rect[1] as f64);
                if l.latest.is_some() && inside(&l.rect, x, y) && l.region.iter().any(|b| lx >= b[0] as f64 && ly >= b[1] as f64 && lx < b[2] as f64 && ly < b[3] as f64) {
                    return scene(l);
                }
            }
            Item::Client(k) => {
                let c = &st.clients[*k];
                if c.takes(x, y) {
                    return Hit::Client(c.id, (x - c.rect[0] as f64, y - c.rect[1] as f64));
                }
            }
        }
    }
    Hit::Scene(None)
}

/// The program's surface that takes all the keyboard on this monitor, if one does.
pub fn keyboard_taker(st: &ScreenState) -> Option<u64> {
    // The highest that asks for it: the lock screen over everything.
    let locked = layers::locked();
    st.clients.iter().rev().filter(|c| c.keyboard == 1 && c.level >= 2 && !c.pieces.is_empty() && (!locked || c.level >= 4)).max_by_key(|c| c.level).map(|c| c.id)
}

/// Whether that program's surface takes the keyboard when clicked.
pub fn takes_keyboard_on_click(st: &ScreenState, id: u64) -> bool {
    st.clients.iter().any(|c| c.id == id && c.keyboard != 0)
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
struct Rect { r: vec4<f32>, f: vec4<f32> };
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
    let c = textureSample(t, s, v.uv);
    // A buffer without alpha (XRGB) covers, whatever its fourth byte holds.
    return select(c, vec4<f32>(c.rgb, 1.0), q.f.x > 0.5);
}
"#;

/// Dual-Kawase blur: four taps on the diagonals, a little further each pass.
const BLUR: &str = r#"
struct K { texel: vec2<f32>, offset: f32, pad: f32 };
@group(0) @binding(0) var t: texture_2d<f32>;
@group(0) @binding(1) var s: sampler;
@group(0) @binding(2) var<uniform> k: K;
struct V { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex fn vs(@builtin(vertex_index) i: u32) -> V {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var v: V;
    v.pos = vec4<f32>(p.x * 2.0 - 1.0, 1.0 - p.y * 2.0, 0.0, 1.0);
    v.uv = p;
    return v;
}
@fragment fn fs(v: V) -> @location(0) vec4<f32> {
    let o = (k.offset + 0.5) * k.texel;
    let c = textureSample(t, s, v.uv + vec2<f32>(o.x, o.y)) + textureSample(t, s, v.uv + vec2<f32>(-o.x, o.y))
          + textureSample(t, s, v.uv + vec2<f32>(o.x, -o.y)) + textureSample(t, s, v.uv + vec2<f32>(-o.x, -o.y));
    return vec4<f32>((c * 0.25).rgb, 1.0);
}
"#;

/// How much smaller what is behind is painted before blurring it, and how many passes.
const BLUR_DOWN: u32 = 4;
const BLUR_PASSES: u32 = 3;

/// Where a quad takes its pixels from.
enum Source {
    Texture(wgpu::Texture),
    /// A program's buffer on the card, by number.
    Buffer(u64),
    /// A program's pixels, copied into a texture of the surface's.
    Pixels(u64),
}

/// A bind group already made for a texture at a place: remade only when
/// either changes, not every time the monitor is put together.
struct Bound {
    texture: wgpu::Texture,
    uniform: [f32; 8],
    group: wgpu::BindGroup,
    /// The last time it was put together with it.
    used: u64,
}

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
            // What the render paints is premultiplied, and so is what the programs hand over.
            targets: &[Some(wgpu::ColorTargetState { format: wgpu::TextureFormat::Bgra8Unorm, blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING), write_mask: wgpu::ColorWrites::ALL })],
        }),
        multiview_mask: None,
        cache: None,
    });
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor { mag_filter: wgpu::FilterMode::Nearest, min_filter: wgpu::FilterMode::Nearest, ..Default::default() });
    let smooth = device.create_sampler(&wgpu::SamplerDescriptor {
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        ..Default::default()
    });
    let layout = pipeline.get_bind_group_layout(0);
    let blur_module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("blur"), source: wgpu::ShaderSource::Wgsl(BLUR.into()) });
    let blur_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("blur"),
        layout: None,
        vertex: wgpu::VertexState { module: &blur_module, entry_point: Some("vs"), compilation_options: Default::default(), buffers: &[] },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &blur_module,
            entry_point: Some("fs"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState { format: wgpu::TextureFormat::Bgra8Unorm, blend: None, write_mask: wgpu::ColorWrites::ALL })],
        }),
        multiview_mask: None,
        cache: None,
    });
    let blur_layout = blur_pipeline.get_bind_group_layout(0);
    // The two small textures the blur goes back and forth between, kept while their size holds.
    let mut blur_room: Option<[wgpu::Texture; 2]> = None;
    let mut bound: Vec<Bound> = Vec::new();
    let mut round = 0u64;
    // The programs' buffers read so far, and each surface's copied pixels.
    let mut buffers: std::collections::HashMap<u64, wgpu::Texture> = Default::default();
    let mut pixels: std::collections::HashMap<u64, wgpu::Texture> = Default::default();
    let debug = std::env::var_os("PLEAMAR_DEBUG_SCREEN").is_some();
    let (lock, cv) = &*screen;
    loop {
        let (quads, blurs, size, modifiers, fresh, anew, arrived, forget, shows, drew_clients, surfaces) = {
            let mut st = lock.lock().unwrap();
            while !st.quit && !(st.dirty && st.idle && !st.paused) {
                st = cv.wait_timeout(st, Duration::from_millis(500)).unwrap().0;
            }
            if st.quit {
                return;
            }
            st.dirty = false;
            let mut quads: Vec<(Source, [i32; 4], bool)> = Vec::new();
            // Before which quad what is behind gets blurred, and where (a program's glass).
            let mut blurs: Vec<(usize, Vec<[i32; 4]>)> = Vec::new();
            let mut arrived: Vec<(u64, Option<u64>, (u32, u32), PieceContent)> = Vec::new();
            let mut shows: Vec<u64> = Vec::new();
            let order = stacked(&st);
            for item in &order {
                match item {
                    Item::Scene(k) => {
                        let l = &st.layers[*k];
                        if let Some(t) = &l.latest {
                            quads.push((Source::Texture(t.clone()), l.rect, false));
                        }
                    }
                    Item::Client(k) => {
                        let c = &st.clients[*k];
                        if !c.blur.is_empty() && !c.pieces.is_empty() {
                            blurs.push((quads.len(), c.blur.iter().map(|b| [c.rect[0] + b[0], c.rect[1] + b[1], b[2], b[3]]).collect()));
                        }
                        for p in &c.pieces {
                            let r = [c.rect[0] + p.at.0, c.rect[1] + p.at.1, p.size.0 as i32, p.size.1 as i32];
                            let source = match p.buffer {
                                Some(b) => Source::Buffer(b),
                                None => Source::Pixels(p.key),
                            };
                            quads.push((source, r, p.opaque));
                        }
                    }
                }
            }
            // The copied pixels of every surface still there are kept, drawn
            // now or not: a wallpaper does not send them again after a lock.
            let mut surfaces: Vec<u64> = Vec::new();
            for c in &mut st.clients {
                for p in &mut c.pieces {
                    if let Some(content) = p.content.take() {
                        arrived.push((p.key, p.buffer, p.size, content));
                    }
                    shows.extend(p.buffer);
                    surfaces.push(p.key);
                }
            }
            if debug {
                eprintln!("screen · {}: {} surfaces of the scene, {} of programs, {} new", st.name, st.layers.len(), st.clients.len(), arrived.len());
            }
            let anew = std::mem::take(&mut st.anew);
            let drew_clients = !st.clients.is_empty();
            (quads, blurs, st.size, st.modifiers.clone(), std::mem::take(&mut st.fresh), anew, arrived, std::mem::take(&mut st.forget), shows, drew_clients, surfaces)
        };
        for b in forget {
            buffers.remove(&b);
        }
        // What the programs brought since the last time: their buffers on the
        // card are read where they are; their pixels, copied.
        for (key, buffer, (w, h), content) in arrived {
            match content {
                PieceContent::Dmabuf(d) => {
                    let Some(b) = buffer else { continue };
                    if buffers.contains_key(&b) {
                        continue;
                    }
                    match pleamar::gpu::Gpu::import_dmabuf(&device, d.fd, (w, h), d.modifier, d.stride, d.offset, wgpu::TextureUses::RESOURCE, wgpu::TextureUsages::TEXTURE_BINDING, wgpu::TextureUses::RESOURCE) {
                        Ok(t) => {
                            buffers.insert(b, t);
                        }
                        Err(e) => eprintln!("screen · a program's buffer could not be read: {e}"),
                    }
                }
                PieceContent::Pixels(data) => {
                    if data.len() < (w * h * 4) as usize {
                        continue;
                    }
                    let t = pixels.entry(key).or_insert_with(|| client_texture(&device, (w, h)));
                    if t.size().width != w || t.size().height != h {
                        *t = client_texture(&device, (w, h));
                    }
                    queue.write_texture(
                        wgpu::TexelCopyTextureInfo { texture: t, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                        &data,
                        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: None },
                        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                    );
                }
                PieceContent::Kept => {}
            }
        }
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
        round += 1;
        // Each quad's bind group, if what it shows is there.
        let mut groups: Vec<Option<usize>> = Vec::new();
        for (source, r, opaque) in &quads {
            let texture = match source {
                Source::Texture(t) => Some(t),
                Source::Buffer(b) => buffers.get(b),
                Source::Pixels(k) => pixels.get(k),
            };
            let Some(texture) = texture else {
                groups.push(None);
                continue;
            };
            let (w, h) = (size.0 as f32, size.1 as f32);
            let uniform = [r[0] as f32 / w, r[1] as f32 / h, (r[0] + r[2]) as f32 / w, (r[1] + r[3]) as f32 / h, if *opaque { 1.0 } else { 0.0 }, 0.0, 0.0, 0.0];
            let k = match bound.iter().position(|b| b.used != round && &b.texture == texture && b.uniform == uniform) {
                Some(k) => k,
                None => {
                    let buffer = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: 32, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
                    queue.write_buffer(&buffer, 0, as_bytes(&uniform));
                    let tv = texture.create_view(&Default::default());
                    let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: None,
                        layout: &layout,
                        entries: &[
                            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&tv) },
                            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
                            wgpu::BindGroupEntry { binding: 2, resource: buffer.as_entire_binding() },
                        ],
                    });
                    bound.push(Bound { texture: texture.clone(), uniform, group, used: round });
                    bound.len() - 1
                }
            };
            bound[k].used = round;
            groups.push(Some(k));
        }
        // In stretches: up to each glass, what is below it is put together; then
        // that is blurred where the glass asks, and the rest goes on top.
        let (w, h) = (size.0 as f32, size.1 as f32);
        let mut from = 0;
        let mut load = wgpu::LoadOp::Clear(wgpu::Color::BLACK);
        for (upto, glass) in blurs.iter().map(|(at, r)| (*at, Some(r))).chain(std::iter::once((quads.len(), None))) {
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("screen"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &view, depth_slice: None, resolve_target: None, ops: wgpu::Operations { load, store: wgpu::StoreOp::Store } })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&pipeline);
                for k in groups[from..upto].iter().flatten() {
                    pass.set_bind_group(0, &bound[*k].group, &[]);
                    pass.draw(0..4, 0..1);
                }
            }
            load = wgpu::LoadOp::Load;
            from = upto;
            let Some(glass) = glass else { continue };
            // The box around the glass, on the monitor.
            let b = glass.iter().fold([i32::MAX, i32::MAX, i32::MIN, i32::MIN], |a, r| [a[0].min(r[0]), a[1].min(r[1]), a[2].max(r[0] + r[2]), a[3].max(r[1] + r[3])]);
            let b = [b[0].max(0), b[1].max(0), b[2].min(size.0 as i32), b[3].min(size.1 as i32)];
            let (bw, bh) = (b[2] - b[0], b[3] - b[1]);
            if bw <= 0 || bh <= 0 {
                continue;
            }
            let small = ((bw as u32 / BLUR_DOWN).max(1), (bh as u32 / BLUR_DOWN).max(1));
            if blur_room.as_ref().is_none_or(|r| r[0].size().width != small.0 || r[0].size().height != small.1) {
                let make = || {
                    device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("blur"),
                        size: wgpu::Extent3d { width: small.0, height: small.1, depth_or_array_layers: 1 },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::Bgra8Unorm,
                        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                        view_formats: &[],
                    })
                };
                blur_room = Some([make(), make()]);
            }
            let room = blur_room.as_ref().expect("just made");
            let views = [room[0].create_view(&Default::default()), room[1].create_view(&Default::default())];
            let group_for = |layout: &wgpu::BindGroupLayout, view: &wgpu::TextureView, sampler: &wgpu::Sampler, uniform: &[f32; 8]| {
                let buffer = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: 32, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
                queue.write_buffer(&buffer, 0, as_bytes(uniform));
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: None,
                    layout,
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(view) },
                        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
                        wgpu::BindGroupEntry { binding: 2, resource: buffer.as_entire_binding() },
                    ],
                })
            };
            // What is below, again, smaller: only the box.
            let below: Vec<wgpu::BindGroup> = quads[..upto]
                .iter()
                .zip(&groups)
                .filter_map(|((_, r, opaque), g)| {
                    let t = &bound[(*g)?].texture;
                    let (fx, fy) = (bw as f32, bh as f32);
                    let u = [(r[0] - b[0]) as f32 / fx, (r[1] - b[1]) as f32 / fy, (r[0] + r[2] - b[0]) as f32 / fx, (r[1] + r[3] - b[1]) as f32 / fy, if *opaque { 1.0 } else { 0.0 }, 0.0, 0.0, 0.0];
                    Some(group_for(&layout, &t.create_view(&Default::default()), &smooth, &u))
                })
                .collect();
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("behind the glass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &views[0], depth_slice: None, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store } })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&pipeline);
                for g in &below {
                    pass.set_bind_group(0, g, &[]);
                    pass.draw(0..4, 0..1);
                }
            }
            // Back and forth, a little further each time.
            let texel = [1.0 / small.0 as f32, 1.0 / small.1 as f32];
            for n in 0..BLUR_PASSES as usize {
                let g = group_for(&blur_layout, &views[n % 2], &smooth, &[texel[0], texel[1], n as f32, 0.0, 0.0, 0.0, 0.0, 0.0]);
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("blur"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &views[(n + 1) % 2], depth_slice: None, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store } })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&blur_pipeline);
                pass.set_bind_group(0, &g, &[]);
                pass.draw(0..3, 0..1);
            }
            // And onto the monitor, only where the glass is.
            let blurred = &views[BLUR_PASSES as usize % 2];
            let back = group_for(&layout, blurred, &smooth, &[b[0] as f32 / w, b[1] as f32 / h, b[2] as f32 / w, b[3] as f32 / h, 1.0, 0.0, 0.0, 0.0]);
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("glass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &view, depth_slice: None, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store } })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &back, &[]);
            for r in glass {
                let x0 = r[0].clamp(0, size.0 as i32);
                let y0 = r[1].clamp(0, size.1 as i32);
                let x1 = (r[0] + r[2]).clamp(0, size.0 as i32);
                let y1 = (r[1] + r[3]).clamp(0, size.1 as i32);
                if x1 > x0 && y1 > y0 {
                    pass.set_scissor_rect(x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32);
                    pass.draw(0..4, 0..1);
                }
            }
        }
        // What has not been shown for a while is not kept (a surface's frames
        // take turns, so one that was not used this time may be the next).
        bound.retain(|b| round - b.used < 8);
        queue.submit(Some(encoder.finish()));
        let done = pleamar::Sent::after(&queue);
        let flying = output.show(which, done, &device, &queue, anew);
        // The buffers read before and no longer shown go back to their programs
        // (`show` waited for the card to finish with them).
        let released: Vec<u64> = {
            let mut st = lock.lock().unwrap();
            let gone: Vec<u64> = st.held.iter().copied().filter(|b| !shows.contains(b)).collect();
            st.held = shows;
            if flying {
                st.idle = false;
                st.on_flip.extend(fresh);
            } else {
                for id in fresh {
                    let _ = to_render.send(ToRender::Frame(id));
                }
            }
            gone
        };
        buffers.retain(|b, _| !released.contains(b));
        pixels.retain(|k, _| surfaces.contains(k));
        if !released.is_empty() {
            layers::tell(ToLayers::Released(released));
        }
        if drew_clients {
            layers::tell(ToLayers::FrameDone);
        }
    }
}

fn client_texture(device: &wgpu::Device, (w, h): (u32, u32)) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("a program's pixels"),
        size: wgpu::Extent3d { width: w.max(1), height: h.max(1), depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Bgra8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
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

fn as_bytes(v: &[f32; 8]) -> &[u8] {
    // SAFETY: eight f32 are thirty-two bytes, laid out as the uniform wants them.
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, 32) }
}
