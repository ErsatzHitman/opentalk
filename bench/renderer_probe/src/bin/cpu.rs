//! CPU painter: egui meshes rasterized in software into a softbuffer (GDI) surface, no GPU API at all.
//!
//! winit window + `egui-winit` for input; a small barycentric triangle rasterizer for the meshes (flat-colour
//! fast path for solid fills, nearest-neighbour texture sampling for text). Premultiplied "over" blending in
//! gamma space, like egui's glow path. A real implementation would add damage rectangles and bilinear glyph
//! sampling; neither changes the memory or idle-CPU picture this probe is after.
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::epaint::{ClippedPrimitive, Primitive, Vertex};
use egui::{Color32, TextureId, ViewportId};
use renderer_probe::{page, report::Probe};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

/// Frame pacing when egui asks for continuous repaints (no vsync on a software path).
const FRAME: Duration = Duration::from_micros(16_667);

struct Texture {
    w: usize,
    h: usize,
    px: Vec<Color32>,
}

struct Gfx {
    window: Arc<Window>,
    surface: softbuffer::Surface<Arc<Window>, Arc<Window>>,
    state: egui_winit::State,
}

struct App {
    ctx: egui::Context,
    gfx: Option<Gfx>,
    textures: HashMap<TextureId, Texture>,
    probe: Probe,
    prev_cpu_ms: Option<f32>,
    dumped: bool,
    next_redraw: Option<Instant>,
    last_frame: Instant,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("OpenTalk probe (cpu)")
            .with_inner_size(LogicalSize::new(1200.0, 800.0))
            .with_min_inner_size(LogicalSize::new(760.0, 520.0))
            .with_decorations(false);
        let window = Arc::new(el.create_window(attrs).expect("window"));
        let context = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        let surface = softbuffer::Surface::new(&context, window.clone()).expect("softbuffer surface");
        let state = egui_winit::State::new(self.ctx.clone(), ViewportId::ROOT, &*window, Some(window.scale_factor() as f32), None, None);
        window.request_redraw();
        self.gfx = Some(Gfx { window, surface, state });
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(g) = self.gfx.as_mut() else { return };
        let resp = g.state.on_window_event(&g.window, &event);
        // RedrawRequested itself reports `repaint`; honouring it would loop at full speed.
        if resp.repaint && !matches!(event, WindowEvent::RedrawRequested) {
            g.window.request_redraw();
        }
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(_) => g.window.request_redraw(),
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        match self.next_redraw {
            Some(t) if Instant::now() >= t => {
                self.next_redraw = None;
                if let Some(g) = &self.gfx {
                    g.window.request_redraw();
                }
                el.set_control_flow(ControlFlow::Wait);
            }
            Some(t) => el.set_control_flow(ControlFlow::WaitUntil(t)),
            None => el.set_control_flow(ControlFlow::Wait),
        }
    }
}

impl App {
    fn redraw(&mut self) {
        let t0 = Instant::now();
        let Some(g) = self.gfx.as_mut() else { return };
        let size = g.window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else { return };
        let raw = g.state.take_egui_input(&g.window);
        let probe = &mut self.probe;
        let prev = self.prev_cpu_ms;
        let out = self.ctx.run_ui(raw, |ui| {
            if probe.frame(prev) {
                ui.ctx().request_repaint();
            }
            page::draw(ui);
        });
        g.state.handle_platform_output(&g.window, out.platform_output);

        for (id, delta) in out.textures_delta.set.iter().flat_map(|(id, ds)| ds.iter().map(move |d| (id, d))) {
            let egui::ImageData::Color(img) = &delta.image;
            let tex = self.textures.entry(*id).or_insert_with(|| Texture { w: 0, h: 0, px: Vec::new() });
            match delta.pos {
                None => {
                    tex.w = img.size[0];
                    tex.h = img.size[1];
                    tex.px = img.pixels.clone();
                }
                Some([x0, y0]) => {
                    for row in 0..img.size[1] {
                        let dst = (y0 + row) * tex.w + x0;
                        tex.px[dst..dst + img.size[0]].copy_from_slice(&img.pixels[row * img.size[0]..(row + 1) * img.size[0]]);
                    }
                }
            }
        }
        let ppp = out.pixels_per_point;
        let prims = self.ctx.tessellate(out.shapes, ppp);

        let (fw, fh) = (w.get() as usize, h.get() as usize);
        // Rasterize straight into the surface's own pixel memory: a second full-window framebuffer would double
        // the private working set (11.8 MB at 2100x1400).
        if g.surface.resize(w, h).is_ok() {
            if let Ok(mut buf) = g.surface.buffer_mut() {
                buf.fill(pack(Color32::from_rgb(24, 24, 27)));
                for p in &prims {
                    if let Primitive::Mesh(m) = &p.primitive {
                        raster_mesh(&mut buf, fw, fh, p, &m.vertices, &m.indices, self.textures.get(&m.texture_id), ppp);
                    }
                }
                if let Some(path) = std::env::var_os("PROBE_BMP") {
                    // Debug aid: dump the first frame as a top-down 32-bit BMP to check the rasterizer by eye.
                    if !self.dumped {
                        self.dumped = true;
                        let _ = write_bmp(std::path::Path::new(&path), &buf, fw, fh);
                    }
                }
                let _ = buf.present();
            }
        }
        for id in &out.textures_delta.free {
            self.textures.remove(id);
        }

        self.prev_cpu_ms = Some(t0.elapsed().as_secs_f32() * 1000.0);
        let delay = out.viewport_output.get(&ViewportId::ROOT).map(|v| v.repaint_delay).unwrap_or(Duration::MAX);
        self.next_redraw = if delay.is_zero() {
            Some((self.last_frame + FRAME).max(Instant::now()))
        } else if delay < Duration::from_secs(3600) {
            Some(Instant::now() + delay)
        } else {
            None
        };
        self.last_frame = Instant::now();
    }
}

fn write_bmp(path: &std::path::Path, px: &[u32], w: usize, h: usize) -> std::io::Result<()> {
    let mut out = Vec::with_capacity(54 + px.len() * 4);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((54 + px.len() * 4) as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(-(h as i32)).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]);
    for p in px {
        out.extend_from_slice(&p.to_le_bytes());
    }
    std::fs::write(path, out)
}

#[inline]
fn pack(c: Color32) -> u32 {
    ((c.r() as u32) << 16) | ((c.g() as u32) << 8) | c.b() as u32
}

/// `dst = src + dst * (1 - src.a)` per channel, `src` premultiplied.
#[inline]
fn over(dst: u32, sr: u32, sg: u32, sb: u32, sa: u32) -> u32 {
    let inv = 255 - sa;
    let r = sr + (((dst >> 16) & 0xFF) * inv + 127) / 255;
    let g = sg + (((dst >> 8) & 0xFF) * inv + 127) / 255;
    let b = sb + ((dst & 0xFF) * inv + 127) / 255;
    (r.min(255) << 16) | (g.min(255) << 8) | b.min(255)
}

#[allow(clippy::too_many_arguments)]
fn raster_mesh(fb: &mut [u32], fw: usize, fh: usize, prim: &ClippedPrimitive, verts: &[Vertex], idx: &[u32], tex: Option<&Texture>, ppp: f32) {
    let clip = prim.clip_rect;
    let cx0 = ((clip.min.x * ppp).floor().max(0.0)) as i32;
    let cy0 = ((clip.min.y * ppp).floor().max(0.0)) as i32;
    let cx1 = ((clip.max.x * ppp).ceil().min(fw as f32)) as i32;
    let cy1 = ((clip.max.y * ppp).ceil().min(fh as f32)) as i32;
    for tri in idx.chunks_exact(3) {
        let (a, b, c) = (&verts[tri[0] as usize], &verts[tri[1] as usize], &verts[tri[2] as usize]);
        let (ax, ay) = (a.pos.x * ppp, a.pos.y * ppp);
        let (bx, by) = (b.pos.x * ppp, b.pos.y * ppp);
        let (cx, cy) = (c.pos.x * ppp, c.pos.y * ppp);
        let area = (bx - ax) * (cy - ay) - (by - ay) * (cx - ax);
        if area.abs() < 1e-6 {
            continue;
        }
        let x0 = (ax.min(bx).min(cx).floor() as i32).max(cx0);
        let x1 = (ax.max(bx).max(cx).ceil() as i32).min(cx1);
        let y0 = (ay.min(by).min(cy).floor() as i32).max(cy0);
        let y1 = (ay.max(by).max(cy).ceil() as i32).min(cy1);
        if x0 >= x1 || y0 >= y1 {
            continue;
        }
        let inv_area = 1.0 / area;
        // Solid triangles (same colour and uv on all corners): constant colour, filled as spans.
        let flat = a.color == b.color && b.color == c.color && a.uv == b.uv && b.uv == c.uv;
        if flat {
            let (sr, sg, sb, sa) = shade(tex, a, a.uv.x, a.uv.y);
            // Scanline spans: where the pixel-centre row crosses the triangle's edges. Half-open on both ends, so
            // triangles that share an edge never both cover a pixel (no double blend of translucent fills).
            let pts = [(ax, ay), (bx, by), (cx, cy)];
            for y in y0..y1 {
                let py = y as f32 + 0.5;
                let (mut xl, mut xr) = (f32::MAX, f32::MIN);
                for i in 0..3 {
                    let ((px0, py0), (px1, py1)) = (pts[i], pts[(i + 1) % 3]);
                    if (py0 <= py) != (py1 <= py) {
                        let x = px0 + (py - py0) * (px1 - px0) / (py1 - py0);
                        xl = xl.min(x);
                        xr = xr.max(x);
                    }
                }
                if xl > xr {
                    continue;
                }
                let s = ((xl - 0.5).ceil() as i32).max(x0);
                let e = ((xr - 0.5).ceil() as i32).min(x1);
                if s >= e {
                    continue;
                }
                let span = &mut fb[y as usize * fw + s as usize..y as usize * fw + e as usize];
                if sa >= 255 {
                    span.fill((sr << 16) | (sg << 8) | sb);
                } else {
                    for d in span {
                        *d = over(*d, sr, sg, sb, sa);
                    }
                }
            }
            continue;
        }
        for y in y0..y1 {
            let py = y as f32 + 0.5;
            let row = y as usize * fw;
            for x in x0..x1 {
                let px = x as f32 + 0.5;
                // Barycentric weights (signed by `area`, so winding does not matter).
                let w0 = ((bx - px) * (cy - py) - (by - py) * (cx - px)) * inv_area;
                let w1 = ((cx - px) * (ay - py) - (cy - py) * (ax - px)) * inv_area;
                let w2 = 1.0 - w0 - w1;
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                    continue;
                }
                let u = w0 * a.uv.x + w1 * b.uv.x + w2 * c.uv.x;
                let v = w0 * a.uv.y + w1 * b.uv.y + w2 * c.uv.y;
                let (tr, tg, tb, ta) = sample(tex, u, v);
                let mix = |ca: u8, cb: u8, cc: u8| w0 * ca as f32 + w1 * cb as f32 + w2 * cc as f32;
                let sr = (mix(a.color.r(), b.color.r(), c.color.r()) * tr as f32 / 255.0) as u32;
                let sg = (mix(a.color.g(), b.color.g(), c.color.g()) * tg as f32 / 255.0) as u32;
                let sb = (mix(a.color.b(), b.color.b(), c.color.b()) * tb as f32 / 255.0) as u32;
                let sa = (mix(a.color.a(), b.color.a(), c.color.a()) * ta as f32 / 255.0) as u32;
                let d = &mut fb[row + x as usize];
                *d = if sa >= 255 { (sr << 16) | (sg << 8) | sb } else { over(*d, sr, sg, sb, sa) };
            }
        }
    }
}

/// Texture sample multiplied by the vertex colour of `v`, as (r, g, b, a) premultiplied.
fn shade(tex: Option<&Texture>, v: &Vertex, u: f32, vv: f32) -> (u32, u32, u32, u32) {
    let (tr, tg, tb, ta) = sample(tex, u, vv);
    let m = |c: u8, t: u8| (c as u32 * t as u32 + 127) / 255;
    (m(v.color.r(), tr), m(v.color.g(), tg), m(v.color.b(), tb), m(v.color.a(), ta))
}

#[inline]
fn sample(tex: Option<&Texture>, u: f32, v: f32) -> (u8, u8, u8, u8) {
    let Some(t) = tex else { return (255, 255, 255, 255) };
    if t.w == 0 {
        return (255, 255, 255, 255);
    }
    let x = ((u * t.w as f32) as usize).min(t.w - 1);
    let y = ((v * t.h as f32) as usize).min(t.h - 1);
    let p = t.px[y * t.w + x];
    (p.r(), p.g(), p.b(), p.a())
}

fn main() {
    renderer_probe::report::mark_start();
    let ctx = egui::Context::default();
    page::install_fonts(&ctx);
    let el = EventLoop::new().expect("event loop");
    let mut app = App {
        ctx,
        gfx: None,
        textures: HashMap::new(),
        probe: Probe::new("cpu-softbuffer"),
        prev_cpu_ms: None,
        dumped: false,
        next_redraw: None,
        last_frame: Instant::now(),
    };
    el.run_app(&mut app).expect("run");
}
