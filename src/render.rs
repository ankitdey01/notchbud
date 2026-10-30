// render.rs — the notch: full island rendered every frame; a DirectComposition
// clip reveals exactly the idle sliver when retracted and springs open on
// hover. Content (Atoll home tab): album-art card + title/artist column,
// progress track, transport row. Motion: Atoll springs (see state.rs)
// sampled into compositor-side animations.

use std::cell::RefCell;

use windows::core::{w, Interface, PCWSTR, Result};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
    D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1, D2D1_DEVICE_CONTEXT_OPTIONS_NONE,
    D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, D2D1_ROUNDED_RECT,
    ID2D1Bitmap1, ID2D1Device, ID2D1DeviceContext, ID2D1Factory, ID2D1PathGeometry,
    ID2D1RenderTarget, ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_0,
    D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, ID3D11Device,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice2, IDCompositionAnimation, IDCompositionDevice,
    IDCompositionRectangleClip, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, IDWriteTextLayout, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_TEXT_METRICS,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISurface, IDXGISwapChain1, DXGI_PRESENT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::System::SystemInformation::GetTickCount64;

use crate::state::{
    clip_closed, clip_open, spring_progress, spring_settle, spring_velocity, App, IDLE_RGN_H,
    IDLE_RGN_HALF_W, ISLAND_H, ISLAND_W, ISLAND_X, SLIVER_H, SLIVER_W, SLIVER_X,
    SPRING_RESPONSE_CLOSE, SPRING_RESPONSE_OPEN, WINDOW_UNITS_H, WINDOW_UNITS_W,
};

// ---------- owner slot + clip geometry ----------

thread_local! {
    static SLOT: RefCell<Option<Renderer>> = const { RefCell::new(None) };
}

pub fn animate(expand: bool) {
    SLOT.with_borrow_mut(|slot| {
        if let Some(rd) = slot {
            if let Err(e) = rd.animate_spring(expand) {
                eprintln!("[dyland] animate_spring({expand}): {e:#}");
            }
        }
    });
}

pub fn draw_now(app: &mut App) {
    SLOT.with_borrow_mut(|slot| {
        let result = (|| -> Result<()> {
            let (w, h) = app.layout_size();
            match slot {
                Some(rd) => {
                    let needs = (rd.pixel_w - w).abs() > 0.5
                        || (rd.pixel_h - h).abs() > 0.5
                        || (rd.dpi_scale - app.scale).abs() > 0.01;
                    if needs {
                        rd.resize(w, h, app.scale)?;
                    }
                }
                None => {
                    *slot = Some(Renderer::create(app.hwnd, w, h, app.scale)?);
                }
            }
            slot.as_mut().unwrap().draw(app)
        })();
        if let Err(e) = result {
            eprintln!("[dyland] draw: {e:#} — dropping renderer for a rebuild next frame");
            *slot = None;
        }
    });
}

// ---------- colors (Atoll palette) ----------

fn island_bg() -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 1.0 } // measured: pure black
}
fn text_hi() -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: 0.949, g: 0.949, b: 0.969, a: 1.0 } // #F2F2F7
}
fn text_gray() -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: 0.5, g: 0.5, b: 0.5, a: 1.0 } // SwiftUI .gray
}
fn art_bg() -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: 0.11, g: 0.11, b: 0.118, a: 1.0 } // #1C1C1E idle art tile
}
fn track_bg() -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: 1.0, g: 1.0, b: 1.0, a: 0.22 } // slider resting track
}
fn note_color() -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: 1.0, g: 1.0, b: 1.0, a: 0.55 } // idle music note
}

// ---------- renderer ----------

pub struct Renderer {
    dc: ID2D1DeviceContext,
    rt: ID2D1RenderTarget,
    dwrite: IDWriteFactory,
    swap: IDXGISwapChain1,
    // DComp objects must outlive the window binding — dropping the device
    // destroys its targets and the notch goes transparent (seen live).
    dcomp: IDCompositionDevice,
    clip: IDCompositionRectangleClip,
    island_path: ID2D1PathGeometry,
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
    pixel_w: f32,
    pixel_h: f32,
    dpi_scale: f32,

    // ---- Atoll spring state (app-side estimate of compositor values) ----
    anim_live: bool,
    anim_dir: bool,            // true = expansion
    anim_start_ms: u64,
    anim_settle: f32,          // seconds until the run anim settles
    anim_from: [f32; 3],       // edge values where the run anim started

    tf_hi: IDWriteTextFormat,   // title: .headline, white
    tf_sub: IDWriteTextFormat,  // artist: .headline medium, gray
    tf_note: IDWriteTextFormat, // idle album-art note
    tf_icon: IDWriteTextFormat, // Segoe MDL2 transport glyphs
    br_black: ID2D1SolidColorBrush,
    br_text_hi: ID2D1SolidColorBrush,
    br_gray: ID2D1SolidColorBrush,
    br_art_bg: ID2D1SolidColorBrush,
    br_track: ID2D1SolidColorBrush,
    br_note: ID2D1SolidColorBrush,
}

// Hermite fit through (t0,v0,m0) -> (t1,v1,m1): returns [D, C, B, A] so that
// v(x) = A·x³ + B·x² + C·x + D for x measured from the segment start.
fn hermite(t0: f32, t1: f32, v0: f32, v1: f32, m0: f32, m1: f32) -> [f32; 4] {
    let d = t1 - t0;
    let a = (2.0 * v0 + m0 * d - 2.0 * v1 + m1 * d) / (d * d * d);
    let b = (-3.0 * v0 - 2.0 * m0 * d + 3.0 * v1 - m1 * d) / (d * d);
    [v0, m0, b, a]
}

impl Renderer {
    // interop bitmap: premultiplied BGRA off the swapchain surface.
    // TARGET|CANNOT_DRAW — TARGET alone is E_INVALIDARG (seen live).
    fn bitmap_props() -> D2D1_BITMAP_PROPERTIES1 {
        D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 0.0,
            dpiY: 0.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            colorContext: std::mem::ManuallyDrop::new(None),
        }
    }

    unsafe fn d3d_device() -> Result<ID3D11Device> {
        let mk = |driver: windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE| -> Result<ID3D11Device> {
            let mut device: Option<ID3D11Device> = None;
            let mut ctx: Option<windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                None,
                driver,
                None,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[
                    D3D_FEATURE_LEVEL_11_0,
                    D3D_FEATURE_LEVEL_10_1,
                    D3D_FEATURE_LEVEL_10_0,
                ]),
                windows::Win32::Graphics::Direct3D11::D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut D3D_FEATURE_LEVEL::default()),
                Some(&mut ctx),
            )?;
            device.ok_or_else(|| windows::core::Error::from_hresult(windows::core::HRESULT(-1)))
        };
        mk(D3D_DRIVER_TYPE_HARDWARE).or_else(|_| mk(D3D_DRIVER_TYPE_WARP))
    }

    pub fn create(hwnd: HWND, w: f32, h: f32, scale: f32) -> Result<Self> {
        unsafe fn step<T>(label: &str, r: windows::core::Result<T>) -> windows::core::Result<T> {
            r.map_err(|e| {
                eprintln!("[dyland] create/{label}: {e:#}");
                e
            })
        }
        unsafe {
            let d3d = Self::d3d_device()?;
            let dxgi_device: IDXGIDevice = d3d.cast()?;
            let factory: ID2D1Factory =
                D2D1CreateFactory::<ID2D1Factory>(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let factory1: windows::Win32::Graphics::Direct2D::ID2D1Factory1 = factory.cast()?;
            let d2d_device: ID2D1Device = step("d2d_device", factory1.CreateDevice(&dxgi_device))?;
            let dc: ID2D1DeviceContext =
                step("d2d_dc", d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE))?;
            let rt: ID2D1RenderTarget = dc.cast()?;
            rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);

            let adapter = dxgi_device.GetAdapter()?;
            let dxgi_factory: IDXGIFactory2 = adapter.GetParent()?;

            let mut desc = DXGI_SWAP_CHAIN_DESC1::default();
            desc.Width = w.ceil() as u32;
            desc.Height = h.ceil() as u32;
            desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
            desc.SampleDesc = DXGI_SAMPLE_DESC { Count: 1, Quality: 0 };
            desc.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
            desc.BufferCount = 2;
            desc.Scaling = DXGI_SCALING_STRETCH;
            desc.SwapEffect = DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL;
            desc.AlphaMode = DXGI_ALPHA_MODE_PREMULTIPLIED;
            desc.Flags = 0;

            let swap: IDXGISwapChain1 = step(
                "create_swapchain",
                dxgi_factory.CreateSwapChainForComposition(&dxgi_device, &desc, None),
            )?;

            let surface: IDXGISurface = step("get_buffer", swap.GetBuffer::<IDXGISurface>(0))?;
            let bitmap: ID2D1Bitmap1 = step(
                "bitmap_from_surface",
                dc.CreateBitmapFromDxgiSurface(&surface, Some(&Self::bitmap_props())),
            )?;
            dc.SetTarget(&bitmap);

            let dcomp: IDCompositionDevice = step(
                "dcomp_device",
                DCompositionCreateDevice2::<&IDXGIDevice, IDCompositionDevice>(&dxgi_device),
            )?;
            let target: IDCompositionTarget = step("dcomp_target", dcomp.CreateTargetForHwnd(hwnd, true))?;
            let visual: IDCompositionVisual = dcomp.CreateVisual()?;
            visual.SetContent(&swap)?;
            target.SetRoot(&visual)?;
            // compositor-side clip: idle shows only the island's top-center
            // sliver; hover animates the clip open (runs on the compositor
            // thread — this is what makes the motion buttery)
            let clip: IDCompositionRectangleClip = step("clip", dcomp.CreateRectangleClip())?;
            let [cl, cr, cb] = clip_closed(scale);
            clip.SetLeft2(cl)?;
            clip.SetTop2(0.0)?;
            clip.SetRight2(cr)?;
            clip.SetBottom2(cb)?;
            visual.SetClip(&clip)?;
            let island_path = Self::build_island_path(&factory, w, h)?;
            step("dcomp_commit", dcomp.Commit())?;

            let dwrite: IDWriteFactory =
                DWriteCreateFactory::<IDWriteFactory>(DWRITE_FACTORY_TYPE_SHARED)?;
            let mk_fmt = |name: PCWSTR,
                          size: f32,
                          weight: windows::Win32::Graphics::DirectWrite::DWRITE_FONT_WEIGHT| {
                use windows::Win32::Graphics::DirectWrite::IDWriteFontCollection;
                dwrite.CreateTextFormat(
                    name,
                    None::<&IDWriteFontCollection>,
                    weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size * scale,
                    w!("en-us"),
                )
            };
            // Atoll home tab: title .headline (semibold) white, artist .headline
            // medium gray; sizes translated to dyland island units.
            let tf_hi = mk_fmt(w!("Segoe UI"), 13.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
            let tf_sub = mk_fmt(w!("Segoe UI"), 11.0, DWRITE_FONT_WEIGHT_NORMAL)?;
            let tf_note = mk_fmt(w!("Segoe UI"), 24.0, DWRITE_FONT_WEIGHT_NORMAL)?;
            let tf_icon = mk_fmt(w!("Segoe MDL2 Assets"), 18.0, DWRITE_FONT_WEIGHT_NORMAL)?;

            let mk_brush = |c: D2D1_COLOR_F| -> Result<ID2D1SolidColorBrush> {
                rt.CreateSolidColorBrush(&c, None)
            };
            let br_black = mk_brush(island_bg())?;
            let br_text_hi = mk_brush(text_hi())?;
            let br_gray = mk_brush(text_gray())?;
            let br_art_bg = mk_brush(art_bg())?;
            let br_track = mk_brush(track_bg())?;
            let br_note = mk_brush(note_color())?;

            Ok(Self {
                dc,
                rt,
                dwrite,
                swap,
                dcomp,
                clip,
                island_path,
                _target: target,
                _visual: visual,
                pixel_w: w,
                pixel_h: h,
                dpi_scale: scale,
                anim_live: false,
                anim_dir: false,
                anim_start_ms: 0,
                anim_settle: 0.0,
                anim_from: [cl, cr, cb],
                tf_hi,
                tf_sub,
                tf_note,
                tf_icon,
                br_black,
                br_text_hi,
                br_gray,
                br_art_bg,
                br_track,
                br_note,
            })
        }
    }

    // island silhouette: top edge flush with the screen edge and both top
    // corners square; sides straight and vertical; only the two bottom
    // corners are heavily rounded (symmetric), joining a straight bottom
    // edge. Bottom radius ≈ 9.6% of the panel width, capped at 20% of the
    // panel height — the hanging-notch shape.
    unsafe fn build_island_path(factory: &ID2D1Factory, w: f32, h: f32) -> Result<ID2D1PathGeometry> {
        let path = factory.CreatePathGeometry()?;
        let sink = path.Open()?;
        // bottom radius: 9.6% of panel width, capped at 20% of panel height
        let rb = (0.096 * w).min(0.20 * h).min(w / 2.0).min(h / 2.0);
        sink.BeginFigure(
            D2D_POINT_2F { x: 0.0, y: 0.0 }, // square top-left corner
            D2D1_FIGURE_BEGIN_FILLED,
        );
        sink.AddLine(D2D_POINT_2F { x: w, y: 0.0 }); // straight top edge (squares the top-right corner)
        sink.AddLine(D2D_POINT_2F { x: w, y: h - rb });
        sink.AddArc(&windows::Win32::Graphics::Direct2D::D2D1_ARC_SEGMENT {
            point: D2D_POINT_2F { x: w - rb, y: h },
            size: D2D_SIZE_F { width: rb, height: rb },
            rotationAngle: 0.0,
            sweepDirection: windows::Win32::Graphics::Direct2D::D2D1_SWEEP_DIRECTION_CLOCKWISE,
            arcSize: windows::Win32::Graphics::Direct2D::D2D1_ARC_SIZE_SMALL,
        });
        sink.AddLine(D2D_POINT_2F { x: rb, y: h });
        sink.AddArc(&windows::Win32::Graphics::Direct2D::D2D1_ARC_SEGMENT {
            point: D2D_POINT_2F { x: 0.0, y: h - rb },
            size: D2D_SIZE_F { width: rb, height: rb },
            rotationAngle: 0.0,
            sweepDirection: windows::Win32::Graphics::Direct2D::D2D1_SWEEP_DIRECTION_CLOCKWISE,
            arcSize: windows::Win32::Graphics::Direct2D::D2D1_ARC_SIZE_SMALL,
        });
        // figure closes along the vertical left side back to (0,0) — square top-left corner
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        sink.Close()?;
        Ok(path)
    }

    /// Atoll springs, baked into compositor animations.
    ///
    /// The critically damped spring x(t)dx = 1 - (1+ωt)e^{-ωt} is sampled every
    /// dt and each sample interval gets a Hermite cubic segment (exact values
    /// and slopes at both ends), then the whole polyline is dispatched as
    /// IDCompositionAnimation segments — the motion still runs on the
    /// compositor thread, exactly as fast/slow as SwiftUI's spring.
    /// Reversing mid-flight starts from the estimated current position, so a
    /// quick hover-out never snaps.
    pub fn animate_spring(&mut self, expand: bool) -> Result<()> {
        let scale = self.dpi_scale;
        let closed = clip_closed(scale);
        let open = clip_open(scale);

        // ---- estimate where the property currently sits ----
        // The previous run animation (if any) is replayed conceptually from
        // this side: p is its normalized progress now.
        let now = unsafe { GetTickCount64() };
        let elapsed = (now as f32 - self.anim_start_ms as f32) / 1000.0;
        let p = if self.anim_live && elapsed < self.anim_settle {
            let resp = if self.anim_dir { SPRING_RESPONSE_OPEN } else { SPRING_RESPONSE_CLOSE };
            spring_progress(resp, elapsed)
        } else {
            1.0 // settled at the previous target
        };
        let prev_final = if self.anim_dir { open } else { closed };
        let from: [f32; 3] = std::array::from_fn(|i| {
            self.anim_from[i] + (prev_final[i] - self.anim_from[i]) * p
        });

        // ---- sample the new spring into Hermite segments ----
        let resp = if expand { SPRING_RESPONSE_OPEN } else { SPRING_RESPONSE_CLOSE };
        let settle = spring_settle(resp);
        let dt = 0.016f32;
        let n = ((settle / dt).ceil().max(1.0)) as usize;
        let target = if expand { open } else { closed };

        // normalized coefficient table: x(t) = A·t³ + B·t² + C·t + D over each
        // segment, with x in [0,1]; each edge is D + (target − from)·x.
        let mut segs: Vec<([f32; 4], f64)> = Vec::with_capacity(n);
        for k in 0..n {
            let t0 = k as f32 * dt;
            let t1 = (k + 1) as f32 * dt;
            let x0 = spring_progress(resp, t0);
            let x1 = spring_progress(resp, t1);
            let m0 = spring_velocity(resp, t0);
            let m1 = spring_velocity(resp, t1);
            segs.push((hermite(t0, t1, x0, x1, m0, m1), t0 as f64));
        }

        unsafe {
            for i in 0..3 {
                let from_e = from[i];
                let target_e = target[i];
                let delta = target_e - from_e;
                let a = self.dcomp.CreateAnimation()?;
                for ([d, c, b, q], t0) in &segs {
                    a.AddCubic(*t0, from_e + delta * d, delta * c, delta * b, delta * q)?;
                }
                a.End(settle as f64, target_e)?;
                match i {
                    0 => self.clip.SetLeft(&a)?,
                    1 => self.clip.SetRight(&a)?,
                    _ => self.clip.SetBottom(&a)?,
                }
            }
            self.dcomp.Commit()?;
        }

        // book-keeping for the next mid-flight reversal estimate
        self.anim_live = true;
        self.anim_dir = expand;
        self.anim_start_ms = now;
        self.anim_settle = settle;
        self.anim_from = from;
        Ok(())
    }

    pub fn resize(&mut self, w: f32, h: f32, scale: f32) -> Result<()> {
        unsafe {
            let dc: &ID2D1DeviceContext = &self.dc;
            self.dpi_scale = scale;
            self.pixel_w = w;
            self.pixel_h = h;
            self.swap.ResizeBuffers(
                2,
                w.ceil() as u32,
                h.ceil() as u32,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_SWAP_CHAIN_FLAG(0),
            )?;
            let surface: IDXGISurface = self.swap.GetBuffer(0)?;
            let bitmap: ID2D1Bitmap1 = dc.CreateBitmapFromDxgiSurface(&surface, Some(&Self::bitmap_props()))?;
            dc.SetTarget(&bitmap);
            // the silhouette's bottom radius is proportional to the panel
            // size — rebuild the path so it stays correct after a resize
            let factory: ID2D1Factory = self.rt.GetFactory()?;
            self.island_path = Self::build_island_path(&factory, w, h)?;
            Ok(())
        }
    }

    // ---------- draw primitives ----------

    unsafe fn layout(&self, s: &str, tf: &IDWriteTextFormat, maxw: f32, maxh: f32) -> Result<IDWriteTextLayout> {
        let utf16: Vec<u16> = s.encode_utf16().collect();
        self.dwrite.CreateTextLayout(&utf16, tf, maxw.max(1.0), maxh.max(1.0))
    }

    fn text(&self, s: &str, tf: &IDWriteTextFormat, x: f32, y: f32, brush: &ID2D1SolidColorBrush, maxw: f32, maxh: f32) {
        unsafe {
            if let Ok(l) = self.layout(s, tf, maxw, maxh) {
                self.rt.DrawTextLayout(D2D_POINT_2F { x, y }, &l, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
            }
        }
    }

    // draw a glyph centered on (cx, cy) — used for transport icons and the
    // idle album-art note
    fn glyph(&self, code: u16, tf: &IDWriteTextFormat, cx: f32, cy: f32, brush: &ID2D1SolidColorBrush) {
        unsafe {
            let s: String = std::iter::once(code as u32).filter_map(char::from_u32).collect();
            let utf16: Vec<u16> = s.encode_utf16().collect();
            if let Ok(l) = self.dwrite.CreateTextLayout(&utf16, tf, 100.0, 100.0) {
                let mut m = DWRITE_TEXT_METRICS::default();
                if l.GetMetrics(&mut m).is_ok() {
                    self.rt.DrawTextLayout(
                        D2D_POINT_2F { x: cx - m.width / 2.0, y: cy - m.height / 2.0 },
                        &l,
                        brush,
                        D2D1_DRAW_TEXT_OPTIONS_NONE,
                    );
                }
            }
        }
    }

    // ---------- frame ----------

    pub fn draw(&mut self, app: &mut App) -> Result<()> {
        unsafe {
            let s = app.scale;
            let ix = ISLAND_X * s;
            let iw = ISLAND_W * s;
            let ih = ISLAND_H * s;

            // hit surface follows the clip TARGET (sliver when retracted)
            app.bar_area = if app.expanded {
                [ix, 0.0, ix + iw, ih]
            } else {
                let sx = SLIVER_X * s;
                [sx, 0.0, sx + SLIVER_W * s, SLIVER_H * s]
            };
            app.rgn_area = if app.expanded {
                [0.0, 0.0, WINDOW_UNITS_W * s, (WINDOW_UNITS_H - 2.0) * s]
            } else {
                let cx = WINDOW_UNITS_W / 2.0 * s; // center in px
                let hw = IDLE_RGN_HALF_W * s;
                [cx - hw, 0.0, cx + hw, IDLE_RGN_H * s]
            };

            self.rt.BeginDraw();
            self.rt.Clear(None);

            // full island always rendered; the DComp clip reveals/retracts it
            self.rt.FillGeometry(&self.island_path, &self.br_black, None);

            if app.expanded {
                self.draw_home(ix, iw, ih, s);
            }

            let end = self.rt.EndDraw(None, None);
            if let Err(e) = &end {
                eprintln!("[dyland] draw/EndDraw: {e:#}");
            }
            end?;
            let present = self.swap.Present(1, DXGI_PRESENT(0));
            if present.is_err() {
                eprintln!("[dyland] draw/Present: {present:?}");
            }
            present.ok()?;
            Ok(())
        }
    }

    // Expanded island content: greeting only.
    unsafe fn draw_home(&self, ix: f32, iw: f32, ih: f32, s: f32) {
        let width = 180.0 * s;
        let height = 34.0 * s;
        self.text(
            "Hello Ankit",
            &self.tf_hi,
            ix + (iw - width) / 2.0,
            (ih - height) / 2.0,
            &self.br_text_hi,
            width,
            height,
        );
    }
}
