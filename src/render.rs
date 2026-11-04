// render.rs — the notch: the full island is rendered once; a DComp scale
// animation (targets from state.rs) drives the idle tab -> swell -> island
// motion entirely on the compositor thread. Content fades are app-side
// clocks (content_alpha) repainted by WM_PAINT pumps from main.rs.

use std::cell::RefCell;
use std::mem::ManuallyDrop;

use windows::core::{w, Interface, PCWSTR, Result};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
    D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1, D2D1_DEVICE_CONTEXT_OPTIONS_NONE,
    D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, D2D1_LAYER_PARAMETERS, D2D1_LAYER_OPTIONS_NONE,
    ID2D1Bitmap1, ID2D1Device, ID2D1DeviceContext, ID2D1Factory, ID2D1Geometry,
    ID2D1Layer, ID2D1PathGeometry,
    ID2D1RenderTarget, ID2D1SolidColorBrush,
};
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_0,
    D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, ID3D11Device,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice2, IDCompositionAnimation, IDCompositionDevice,
    IDCompositionRectangleClip, IDCompositionScaleTransform, IDCompositionTarget,
    IDCompositionVisual,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, IDWriteTextLayout, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_TEXT_METRICS,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT_SEMI_BOLD,
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
    scale_idle, scale_swell, spring_progress, spring_settle, spring_velocity, App, Chat,
    close_progress, close_velocity, CLOSE_DELAY, CONTENT_FADE, IDLE_RGN_H, IDLE_RGN_HALF_W,
    ISLAND_BOT_R, ISLAND_FLARE_R, ISLAND_H, ISLAND_W, ISLAND_X, OPEN_RESPONSE, OPEN_ZETA,
    SLIVER_H, SLIVER_W, SLIVER_X, STAGE1_DUR, SWELL_RESPONSE, SWELL_ZETA,
    WINDOW_UNITS_H, WINDOW_UNITS_W,
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

// ---------- renderer ----------

pub struct Renderer {
    dc: ID2D1DeviceContext,
    rt: ID2D1RenderTarget,
    dwrite: IDWriteFactory,
    swap: IDXGISwapChain1,
    // DComp objects must outlive the window binding — dropping the device
    // destroys its targets and the notch goes transparent (seen live).
    dcomp: IDCompositionDevice,
    _clip: IDCompositionRectangleClip,
    ctrl: IDCompositionScaleTransform, // island shell scale (the whole motion)
    island_path: ID2D1PathGeometry,
    layer: ID2D1Layer, // per-frame content clip: text stays inside the shell
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
    pixel_w: f32,
    pixel_h: f32,
    dpi_scale: f32,

    // ---- spring state (app-side estimate of compositor values) ----
    anim_live: bool,
    anim_expand: bool,         // true = expansion run was the last one
    anim_start_ms: u64,
    anim_settle: f32,          // seconds until the run anim settles
    anim_from: [f32; 2],       // scale values where the run anim started

    tf_hi: IDWriteTextFormat, // title: .headline, white
    br_black: ID2D1SolidColorBrush,
    br_text_hi: ID2D1SolidColorBrush,
}

// Hermite fit through (t0,v0,m0) -> (t1,v1,m1): returns [D, C, B, A] so that
// v(x) = A·x³ + B·x² + C·x + D for x measured from the segment start.
pub fn hermite(t0: f32, t1: f32, v0: f32, v1: f32, m0: f32, m1: f32) -> [f32; 4] {
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

    // DComp's animation-or-scalar parameters (CenterX, ScaleX, ...) accept
    // an IDCompositionAnimation only in windows-rs — wrap scalars in a flat
    // cubic with zero slopes; the compositor holds the End value forever.
    fn scalar_anim(dcomp: &IDCompositionDevice, v: f32) -> Result<IDCompositionAnimation> {
        unsafe {
            let a = dcomp.CreateAnimation()?;
            a.AddCubic(0.0, v, 0.0, 0.0, 0.0)?;
            a.End(1.0, v)?;
            Ok(a)
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

            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: w.ceil() as u32,
                Height: h.ceil() as u32,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                Flags: 0,
                ..Default::default()
            };

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
            // compositor-side clip: static — the whole motion is a scale of
            // the fully-drawn island about the point (center-x, y=0), which
            // matches Nusa whose overshoot was in the shell size itself.
            // At idle scale the shell IS the small tab; no clip tricks needed.
            let clip: IDCompositionRectangleClip = step("clip", dcomp.CreateRectangleClip())?;
            clip.SetLeft2(0.0)?;
            clip.SetTop2(0.0)?;
            clip.SetRight2(w)?;
            clip.SetBottom2(h)?;
            visual.SetClip(&clip)?;
            let ctrl: IDCompositionScaleTransform = step("ctrl", dcomp.CreateScaleTransform())?;
            let idle = scale_idle();
            step("ctrl_center", ctrl.SetCenterX(&Self::scalar_anim(&dcomp, WINDOW_UNITS_W / 2.0 * scale)?))?;
            step("ctrl_center", ctrl.SetCenterY(&Self::scalar_anim(&dcomp, 0.0)?))?;
            step("ctrl_scale", ctrl.SetScaleX(&Self::scalar_anim(&dcomp, idle[0])?))?;
            step("ctrl_scale", ctrl.SetScaleY(&Self::scalar_anim(&dcomp, idle[1])?))?;
            visual.SetEffect(&ctrl)?;
            let island_path = Self::build_island_path(&factory, h, scale)?;
            let layer = rt.CreateLayer(None)?;
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
            // Atoll home tab: .headline semibold, translated to dyland island units.
            // Apple system font; falls back to the default face when SF Pro
            // isn't installed on this machine.
            let tf_hi = mk_fmt(w!("SF Pro Text"), 17.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;

            let mk_brush = |c: D2D1_COLOR_F| -> Result<ID2D1SolidColorBrush> {
                rt.CreateSolidColorBrush(&c, None)
            };
            let br_black = mk_brush(island_bg())?;
            let br_text_hi = mk_brush(text_hi())?;

            Ok(Self {
                dc,
                rt,
                dwrite,
                swap,
                dcomp,
                _clip: clip,
                ctrl,
                island_path,
                layer,
                _target: target,
                _visual: visual,
                pixel_w: w,
                pixel_h: h,
                dpi_scale: scale,
                anim_live: false,
                anim_expand: false,
                anim_start_ms: 0,
                anim_settle: 0.0,
                anim_from: scale_idle(),
                tf_hi,
                br_black,
                br_text_hi,
            })
        }
    }

    // island silhouette, mirrored from Nusa's captured outline — the
    // top joints are organic pairs of cubic Béziers, the bottom corners
    // are plain circular arcs:
    //  * body 420x140.4 u, sides vertical, bottom edge straight;
    //  * top corners: two cubics blending the top edge into the side.
    //    The flare crest stays exactly on the old flare line (ix±fr), so
    //    the silhouette footprint is unchanged; first handles ride the
    //    top edge (flat, curvature ~0 shoulder), the boundary rises evenly
    //    to a mid point M on the 45° bisector with a diagonal tangent, and
    //    the second handles ride the side (vertical arrival, curvature ~0
    //    blend into it). Corner proportions are canonical fractions of the
    //    corner radius:
    //    S=(±1,0) P1=(±0.83,0) P2=(±0.54,0) M=(0.27,0.27) P3'=(0,0.54)
    //    P4'=(0,0.83) E=(0,1) — the flare keeps its shape at every scale
    //    the DComp shell animates through (idle tab -> swell -> island).
    //  * bottom corners: simple circular arcs of radius ISLAND_BOT_R
    //    (macOS-style pill bottom — no S-profile there).
    unsafe fn build_island_path(factory: &ID2D1Factory, h_window: f32, s: f32) -> Result<ID2D1PathGeometry> {
        let path = factory.CreatePathGeometry()?;
        let sink = path.Open()?;
        let ix = ISLAND_X * s;
        let iw = ISLAND_W * s;
        let ih = (ISLAND_H * s).min(h_window); // keep inside the window
        let fr = ISLAND_FLARE_R * s;
        let rb = ISLAND_BOT_R * s;
        let p = |x: f32, y: f32| D2D_POINT_2F { x, y };
        let bz = |c1: (f32, f32), c2: (f32, f32), e: (f32, f32)| {
            windows::Win32::Graphics::Direct2D::Common::D2D1_BEZIER_SEGMENT {
                point1: p(c1.0, c1.1),
                point2: p(c2.0, c2.1),
                point3: p(e.0, e.1),
            }
        };
        let arc = |e: (f32, f32)| {
            windows::Win32::Graphics::Direct2D::D2D1_ARC_SEGMENT {
                point: p(e.0, e.1),
                size: windows::Win32::Graphics::Direct2D::Common::D2D_SIZE_F {
                    width: rb,
                    height: rb,
                },
                rotationAngle: 0.0,
                sweepDirection: windows::Win32::Graphics::Direct2D::D2D1_SWEEP_DIRECTION_CLOCKWISE,
                arcSize: windows::Win32::Graphics::Direct2D::D2D1_ARC_SIZE_SMALL,
            }
        };
        let cs = 0.27; // canonical midpoint on the 45° bisector
        let ch = 0.17; // canonical shoulder handle (ch ≈ 0.5 − cs)


        sink.BeginFigure(p(ix - fr, 0.0), D2D1_FIGURE_BEGIN_FILLED);
        // straight top edge between the two flare crests
        sink.AddLine(p(ix + iw + fr, 0.0));
        // top-right flare, crest -> side (G2 at both joins):
        //   handles of C1 ride the top edge, handles of C2 ride the side
        sink.AddBezier(&bz(
            (ix + iw + (1.0 - ch) * fr, 0.0),
            (ix + iw + 2.0 * cs * fr, 0.0),
            (ix + iw + cs * fr, cs * fr),
        ));
        sink.AddBezier(&bz(
            (ix + iw, 2.0 * cs * fr),
            (ix + iw, (1.0 - ch) * fr),
            (ix + iw, fr),
        ));
        sink.AddLine(p(ix + iw, ih - rb));
        // bottom-right corner: plain circular arc, side -> bottom edge
        sink.AddArc(&arc((ix + iw - rb, ih)));
        sink.AddLine(p(ix + rb, ih));
        // bottom-left corner: plain circular arc, bottom edge -> side
        sink.AddArc(&arc((ix, ih - rb)));
        // left side into the top-left flare, closing at the crest
        sink.AddLine(p(ix, fr));
        // top-left flare, side -> crest (the TL chain traversed backward):
        // handles ride the side first, then the top edge — same profile
        sink.AddBezier(&bz(
            (ix, (1.0 - ch) * fr),
            (ix, 2.0 * cs * fr),
            (ix - cs * fr, cs * fr),
        ));
        sink.AddBezier(&bz(
            (ix - 2.0 * cs * fr, 0.0),
            (ix + (ch - 1.0) * fr, 0.0),
            (ix - fr, 0.0),
        ));
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        sink.Close()?;
        Ok(path)
    }

    /// Springs sampled into compositor animations, mirroring Nusa's motion:
    /// expand = a 0.19 s swell of the sliver, then the underdamped pop spring
    /// (response 0.35, zeta 0.65 — the measured ~6% overshoot); retract = a
    /// critically damped collapse (response 0.21) with the shell's ~60 ms
    /// reaction delay baked in. Each sample interval gets a Hermite cubic
    /// segment (exact values and slopes at both ends), dispatched as
    /// IDCompositionAnimation segments — the motion still runs on the
    /// compositor thread. Reversing mid-flight starts from the estimated
    /// current position, so a quick hover-out never snaps.
    pub fn animate_spring(&mut self, expand: bool) -> Result<()> {
        // the motion drives the visual SCALE: idle tab -> swell -> full island
        let idle = scale_idle();
        let swell = scale_swell();
        let open = [1.0f32, 1.0f32];

        // ---- estimate where the property currently sits ----
        // The previous run animation (if any) is replayed conceptually from
        // this side: `from` is its value now.
        let now = unsafe { GetTickCount64() };
        // u64 subtract first: casting both ms values to f32 loses precision
        // past ~4.7 h uptime and skews the reversal estimate by whole frames
        let elapsed = (now - self.anim_start_ms) as f32 / 1000.0;
        let from: [f32; 2] = if !self.anim_live {
            idle
        } else if self.anim_expand {
            if elapsed < STAGE1_DUR {
                let p = spring_progress(SWELL_RESPONSE, SWELL_ZETA, elapsed);
                std::array::from_fn(|i| self.anim_from[i] + (swell[i] - self.anim_from[i]) * p)
            } else {
                let p = spring_progress(OPEN_RESPONSE, OPEN_ZETA, elapsed - STAGE1_DUR);
                std::array::from_fn(|i| swell[i] + (open[i] - swell[i]) * p)
            }
        } else {
            // close_progress v: 1 = open (== anim_from), 0 = idle; convert to
            // the canonic from->idle param before interpolating. The whole
            // shrink runs CLOSE_DELAY late (the greeting hides first), so
            // the knot clock is delayed by that much from the run start.
            let p = 1.0 - close_progress(elapsed - CLOSE_DELAY);
            std::array::from_fn(|i| self.anim_from[i] + (idle[i] - self.anim_from[i]) * p)
        };

        // ---- sample the new curve into Hermite segments ----
        // Segments carry their own canonic from/to values, because an expand
        // run changes the base at the swell boundary. Coefficients are
        // normalized x in [0,1]; each track renders as lo + (hi−lo)·x(t).
        struct Seg {
            coefs: [f32; 4],
            t0: f64,
            lo: [f32; 2],
            hi: [f32; 2],
        }
        let dt = 0.016f32;
        let mut segs: Vec<Seg> = Vec::new();
        let settle;
        if expand {
            // stage 1: sliver swell, from -> swell
            let n1 = ((STAGE1_DUR / dt) + 0.999).ceil() as usize;
            for k in 0..n1 {
                let t0 = k as f32 * dt;
                let t1 = ((k + 1) as f32 * dt).min(STAGE1_DUR);
                if t1 - t0 <= 0.0 {
                    continue;
                }
                let x0 = spring_progress(SWELL_RESPONSE, SWELL_ZETA, t0);
                let x1 = spring_progress(SWELL_RESPONSE, SWELL_ZETA, t1);
                let m0 = spring_velocity(SWELL_RESPONSE, SWELL_ZETA, t0);
                let m1 = spring_velocity(SWELL_RESPONSE, SWELL_ZETA, t1);
                segs.push(Seg { coefs: hermite(t0, t1, x0, x1, m0, m1), t0: t0 as f64, lo: from, hi: swell });
            }
            // stage 2: pop, swell -> open. Sample count n2 so the last
            // segment begins before End(settle): settle = STAGE1 + n2*dt.
            let settle2 = spring_settle(OPEN_RESPONSE, OPEN_ZETA);
            let n2 = (settle2 / dt).ceil() as usize;
            for k in 0..n2 {
                let t0 = STAGE1_DUR + k as f32 * dt;
                let t1 = t0 + dt;
                let lt = k as f32 * dt;
                let x0 = spring_progress(OPEN_RESPONSE, OPEN_ZETA, lt);
                let x1 = spring_progress(OPEN_RESPONSE, OPEN_ZETA, lt + dt);
                let m0 = spring_velocity(OPEN_RESPONSE, OPEN_ZETA, lt);
                let m1 = spring_velocity(OPEN_RESPONSE, OPEN_ZETA, lt + dt);
                segs.push(Seg { coefs: hermite(t0, t1, x0, x1, m0, m1), t0: t0 as f64, lo: swell, hi: open });
            }
            settle = STAGE1_DUR + n2 as f32 * dt;
        } else {
            // close: piecewise Hermite straight through Nusa's measured knot
            // table (t in seconds, v = value fraction; 1 = open, 0 = idle)
            // v maps directly onto "from + (idle - from)*v", but the press
            // Hermite convention is x = 0 at the start value -> x = 1 - v
            for w in crate::state::CLOSE_KNOTS.windows(2) {
                let (t0, v0) = w[0];
                let (t1, v1) = w[1];
                if t1 <= crate::state::CLOSE_SETTLE {
                    let m0 = -close_velocity(t0);
                    let m1 = -close_velocity(t1);
                    segs.push(Seg {
                        coefs: hermite(t0, t1, 1.0 - v0, 1.0 - v1, m0, m1),
                        t0: (t0 + CLOSE_DELAY) as f64,
                        lo: from,
                        hi: idle,
                    });
                }
            }
            // flat hold after the last knot: the final cubic must not be
            // extrapolated between its knot span and End (the polynomial
            // runs away outside its fitted range and flashes the shell)
            let last_t = crate::state::CLOSE_KNOTS
                .iter()
                .map(|k| k.0)
                .filter(|&t| t <= crate::state::CLOSE_SETTLE + 1e-6)
                .fold(0.0f32, f32::max);
            segs.push(Seg {
                coefs: [1.0, 0.0, 0.0, 0.0],
                t0: (last_t + CLOSE_DELAY) as f64,
                lo: from,
                hi: idle,
            });
            settle = CLOSE_DELAY + crate::state::CLOSE_SETTLE + 0.06; // End sits past the last segment
        }

        unsafe {
            for i in 0..2 {
                let a = self.dcomp.CreateAnimation()?;
                for seg in &segs {
                    let delta = seg.hi[i] - seg.lo[i];
                    let [d, c, b, q] = seg.coefs;
                    a.AddCubic(seg.t0, seg.lo[i] + delta * d, delta * c, delta * b, delta * q)?;
                }
                a.End(settle as f64, if expand { open[i] } else { idle[i] })?;
                if i == 0 {
                    self.ctrl.SetScaleX(&a)?;
                } else {
                    self.ctrl.SetScaleY(&a)?;
                }
            }
            self.dcomp.Commit()?;
        }

        // book-keeping for the next mid-flight reversal estimate
        self.anim_live = true;
        self.anim_expand = expand;
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
            // the silhouette is fixed-unit scaled — rebuild on resize so the
            // flare/bottom radii stay correct after a DPI change, and keep the
            // scale anchor at (center-x, y=0)
            let factory: ID2D1Factory = self.rt.GetFactory()?;
            self.island_path = Self::build_island_path(&factory, h, scale)?;
            // the clip too: pixel dims follow the DPI change, otherwise the
            // visual stays clipped to the old window size
            self._clip.SetRight2(w)?;
            self._clip.SetBottom2(h)?;
            self.ctrl.SetCenterX(&Self::scalar_anim(&self.dcomp, WINDOW_UNITS_W / 2.0 * scale)?)?;
            self.ctrl.SetCenterY(&Self::scalar_anim(&self.dcomp, 0.0)?)?;
            self.dcomp.Commit()?;
            Ok(())
        }
    }

    // ---------- draw primitives ----------

    unsafe fn layout(&self, s: &str, tf: &IDWriteTextFormat, maxw: f32, maxh: f32) -> Result<IDWriteTextLayout> {
        let utf16: Vec<u16> = s.encode_utf16().collect();
        self.dwrite.CreateTextLayout(&utf16, tf, maxw.max(1.0), maxh.max(1.0))
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

            // content choreography: hidden until the island is fully open
            // (fades in after the pop settles), gone before the visible
            // shrink begins (fades out over CLOSE_DELAY, and the shrink's
            // knots start that late for exactly that window)
            let alpha = self.content_alpha(app.expanded);
            if alpha > 0.0 {
                self.br_text_hi.SetOpacity(alpha);
                self.draw_content(ix, iw, ih, s, &mut app.chat);
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

    // content alpha: 0 while the island is still opening, ramps to 1 after
    // the pop settles; on a retract it drops over CLOSE_DELAY — the shrink
    // curve is shifted by that window so the text hides BEFORE the collapse
    fn content_alpha(&self, expanded: bool) -> f32 {
        if !self.anim_live {
            return 0.0;
        }
        let now = unsafe { GetTickCount64() };
        let t = (now - self.anim_start_ms) as f32 / 1000.0;
        if !self.anim_expand {
            if expanded {
                // stale close state with expanded=true only ever shows up
                // in the launch frame of a fresh expand (draw follows the
                // animate() clock) — nothing should show there yet
                return 0.0;
            }
            // retract: the greeting fades out ahead of the visible shrink
            return (1.0 - t / CLOSE_DELAY).max(0.0);
        }
        if !expanded {
            return 0.0;
        }
        if t < self.anim_settle {
            0.0 // island not fully open yet — inside stays empty
        } else {
            ((t - self.anim_settle) / CONTENT_FADE).min(1.0)
        }
    }

    // Expanded island content: the chat surface, in the greeting slot (same
    // fonts, same alpha fade — the silhouette and choreography are frozen).
    // The block stays bottom-anchored (newest line down) and is clipped to
    // the island silhouette; wheel scrolling offsets it to reveal older
    // lines instead of spilling past the shell.
    unsafe fn draw_content(&self, ix: f32, iw: f32, ih: f32, s: f32, chat: &mut Chat) {
        let text = chat.slot_text();
        if let Ok(l) = self.layout(&text, &self.tf_hi, iw - 24.0 * s, ih) {
            let mut m = DWRITE_TEXT_METRICS::default();
            let (tw, th) = match l.GetMetrics(&mut m) {
                Ok(()) => (m.width, m.height),
                Err(_) => (iw, ih), // fallback keeps it on-island
            };
            let x = if tw < iw * 0.9 {
                // short text (greeting, status lines) stays centered
                ix + (iw - tw) / 2.0
            } else {
                // long chat blocks sit left-aligned with a margin
                ix + 24.0 * s
            };
            // viewport = what fits inside the shell above the bottom margin;
            // scroll 0 pins the newest line to the bottom, scrolling up
            // slides the block down until the oldest line touches the top
            let viewport = (ih - 14.0 * s).max(1.0);
            chat.max_scroll = (th - viewport).max(0.0);
            chat.scroll = chat.scroll.clamp(0.0, chat.max_scroll);
            let y = (ih - th - 10.0 * s) + chat.scroll;
            // clip to the island body — previously tall text drew straight
            // out of the shell's bottom edge onto the desktop
            let mask: ID2D1Geometry = match self.island_path.cast() {
                Ok(g) => g,
                Err(_) => return,
            };
            self.rt.PushLayer(
                &D2D1_LAYER_PARAMETERS {
                    contentBounds: D2D_RECT_F { left: ix, top: 0.0, right: ix + iw, bottom: ih },
                    geometricMask: ManuallyDrop::new(Some(mask)),
                    maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                    maskTransform: Matrix3x2 { M11: 1.0, M12: 0.0, M21: 0.0, M22: 1.0, M31: 0.0, M32: 0.0 },
                    opacity: 1.0,
                    opacityBrush: ManuallyDrop::new(None),
                    layerOptions: D2D1_LAYER_OPTIONS_NONE,
                },
                &self.layer,
            );
            self.rt.DrawTextLayout(
                D2D_POINT_2F { x, y },
                &l,
                &self.br_text_hi,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
            self.rt.PopLayer();
        }
    }
}
