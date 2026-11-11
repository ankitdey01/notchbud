// render.rs — island shell only.
//
// The full island silhouette is rendered once; a DComp scale animation
// (targets from state.rs) drives the idle tab -> swell -> island motion
// entirely on the compositor thread. All *content* (greeting, chat,
// inputs) renders in HTML/CSS inside WebView2 — this file owns no text.

use std::cell::RefCell;
use std::mem::ManuallyDrop;

use windows::core::{Interface, Result};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Bitmap1, ID2D1Device, ID2D1DeviceContext, ID2D1Factory,
    ID2D1PathGeometry, ID2D1RenderTarget, ID2D1SolidColorBrush,
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET,
    D2D1_BITMAP_PROPERTIES1, D2D1_DEVICE_CONTEXT_OPTIONS_NONE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_10_0,
    D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice2, IDCompositionAnimation, IDCompositionDevice,
    IDCompositionRectangleClip, IDCompositionScaleTransform, IDCompositionTarget,
    IDCompositionVisual,
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
use windows_numerics::Vector2;

use crate::state::{
    close_bounce_factor, close_progress, close_velocity, scale_idle, scale_swell, spring_progress,
    spring_settle, spring_velocity, App, CLOSE_COLLAPSE_END, CLOSE_DELAY, CLOSE_DIP_FACTOR,
    CLOSE_DIP_T, CLOSE_RECOVER_T, IDLE_RGN_H, IDLE_RGN_HALF_W, ISLAND_BOT_R, ISLAND_FLARE_R,
    ISLAND_H, ISLAND_W, ISLAND_X, OPEN_RESPONSE, OPEN_ZETA, SLIVER_H, SLIVER_W, SLIVER_X,
    STAGE1_DUR, SWELL_RESPONSE, SWELL_ZETA, WINDOW_UNITS_H, WINDOW_UNITS_W,
};

thread_local! {
    static SLOT: RefCell<Option<Renderer>> = const { RefCell::new(None) };
}

pub fn animate(expand: bool) {
    SLOT.with_borrow_mut(|slot| {
        if let Some(rd) = slot {
            if let Err(e) = rd.animate_spring(expand) {
                eprintln!("[notchbud] animate_spring({expand}): {e:#}");
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
            eprintln!("[notchbud] draw: {e:#} — dropping renderer for a rebuild next frame");
            *slot = None;
        }
    });
}

// ---------- colors (Atoll palette) ----------

fn island_bg() -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 1.0,
    } // measured: pure black
}

// ---------- renderer ----------

pub struct Renderer {
    dc: ID2D1DeviceContext,
    rt: ID2D1RenderTarget,
    swap: IDXGISwapChain1,
    // DComp objects must outlive the window binding — dropping the device
    // destroys its targets and the notch goes transparent (seen live).
    dcomp: IDCompositionDevice,
    _clip: IDCompositionRectangleClip,
    ctrl: IDCompositionScaleTransform, // island shell scale (the whole motion)
    island_path: ID2D1PathGeometry,
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
    pixel_w: f32,
    pixel_h: f32,
    dpi_scale: f32,

    // ---- spring state (app-side estimate of compositor values) ----
    anim_live: bool,
    anim_expand: bool, // true = expansion run was the last one
    anim_start_ms: u64,
    anim_settle: f32,    // seconds until the run anim settles
    anim_from: [f32; 2], // scale values where the run anim started

    br_black: ID2D1SolidColorBrush, // island shell — always opaque
}

/// Hermite segment in the 0–1 basis: `p(u) = d + c*u + b*u² + a*u³`
/// with `u = (t - t0) / (t1 - t0)`. Kept for the bounce-tail resampling
/// below, where [`hermite_pos`] / [`hermite_vel`] evaluate in `u`.
fn hermite_unit(t0: f32, t1: f32, v0: f32, v1: f32, m0: f32, m1: f32) -> [f32; 4] {
    let dt = t1 - t0;
    let a = 2.0 * (v0 - v1) + (m0 + m1) * dt;
    let b = 3.0 * (v1 - v0) - (2.0 * m0 + m1) * dt;
    let c = m0 * dt;
    let d = v0;
    [d, c, b, a]
}

/// Hermite segment in seconds, as `AddCubic` expects:
/// `p(s) = d + c*s + b*s² + a*s³` with `s = t - t0` in seconds.
pub fn hermite(t0: f32, t1: f32, v0: f32, v1: f32, m0: f32, m1: f32) -> [f32; 4] {
    let dt = t1 - t0;
    let [d, c_u, b_u, a_u] = hermite_unit(t0, t1, v0, v1, m0, m1);
    if dt == 0.0 || !dt.is_finite() {
        return [d, 0.0, 0.0, 0.0];
    }
    [d, c_u / dt, b_u / (dt * dt), a_u / (dt * dt * dt)]
}

/// Programming quantum for the shell scale animation: finer than any
/// common display vsync (60/120/144/240 Hz), so every compositor frame
/// lands inside a fresh segment. Coarser programming holds each value
/// across whole vsyncs then jumps — hold-then-jump reads as low FPS on
/// high-refresh panels even though the springs themselves are correct.
const SAMPLE_DT: f32 = 1.0 / 240.0;

/// Position of a normalized Hermite segment at u in [0, 1].
fn hermite_pos(coefs: &[f32; 4], u: f32) -> f32 {
    coefs[0] + coefs[1] * u + coefs[2] * u * u + coefs[3] * u * u * u
}

/// Velocity (per-second over the fitted span) of that segment at u.
fn hermite_vel(coefs: &[f32; 4], u: f32, span: f32) -> f32 {
    (coefs[1] + 2.0 * coefs[2] * u + 3.0 * coefs[3] * u * u) / span
}

/// One Hermite segment of the shell scale animation. The compositor plays
/// `value(t) = lo + delta*(d + c*s + b*s^2 + a*s^3)` with `s = t - t0` in
/// seconds (the exact `AddCubic` mapping in `animate_spring`'s dispatch).
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct SpringSeg {
    pub coefs: [f32; 4],
    pub t0: f64,
    pub lo: [f32; 2],
    pub hi: [f32; 2],
}

/// Sample the shell scale animation the compositor will play for an
/// expand (open) or retract (close) run starting at `from`. Returns the
/// segment list and the settle time (seconds) the `End` holds.
/// Exposed so tests can pin the close curve smooth — the compositor gives
/// no readback, so sampling what we program is the only headless check
/// that the animation plays as designed.
#[doc(hidden)]
pub fn sample_shell_animation(expand: bool, from: [f32; 2]) -> (Vec<SpringSeg>, f32) {
    let idle = scale_idle();
    let swell = scale_swell();
    let open = [1.0f32, 1.0f32];
    let dt = SAMPLE_DT;
    let mut segs: Vec<SpringSeg> = Vec::new();
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
            segs.push(SpringSeg {
                coefs: hermite(t0, t1, x0, x1, m0, m1),
                t0: t0 as f64,
                lo: from,
                hi: swell,
            });
        }
        // stage 2: pop, swell -> open.
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
            segs.push(SpringSeg {
                coefs: hermite(t0, t1, x0, x1, m0, m1),
                t0: t0 as f64,
                lo: swell,
                hi: open,
            });
        }
        settle = STAGE1_DUR + n2 as f32 * dt;
    } else {
        // close: the measured collapse resampled at frame granularity.
        // Coarse knot-spaced segments play back as hold-then-jump, so the
        // collapse is resampled here at SAMPLE_DT like everything else:
        // identical curve, fresh value every compositor frame.
        let n = ((CLOSE_COLLAPSE_END / dt) + 0.999).ceil() as usize;
        for k in 0..n {
            let t0 = k as f32 * dt;
            let t1 = ((k + 1) as f32 * dt).min(CLOSE_COLLAPSE_END);
            if t1 - t0 <= 0.0 {
                continue;
            }
            let x0 = 1.0 - close_progress(t0);
            let x1 = 1.0 - close_progress(t1);
            let m0 = -close_velocity(t0);
            let m1 = -close_velocity(t1);
            segs.push(SpringSeg {
                coefs: hermite(t0, t1, x0, x1, m0, m1),
                t0: (t0 + CLOSE_DELAY) as f64,
                lo: from,
                hi: idle,
            });
        }
        // bounce tail: idle -> dip -> idle, smooth ease in/out each way,
        // resampled like the collapse so the dip plays back as motion
        // instead of hold-then-jump on fast panels.
        let dip = std::array::from_fn(|i| idle[i] * CLOSE_DIP_FACTOR);
        for (a, b, lo, hi) in [
            (CLOSE_COLLAPSE_END, CLOSE_DIP_T, idle, dip),
            (CLOSE_DIP_T, CLOSE_RECOVER_T, dip, idle),
        ] {
            let base = hermite_unit(a, b, 0.0, 1.0, 0.0, 0.0);
            let span = b - a;
            let mut t0 = a;
            while t0 < b - 1e-9 {
                let t1 = (t0 + dt).min(b);
                let u0 = (t0 - a) / span;
                let u1 = (t1 - a) / span;
                segs.push(SpringSeg {
                    coefs: hermite(
                        t0,
                        t1,
                        hermite_pos(&base, u0),
                        hermite_pos(&base, u1),
                        hermite_vel(&base, u0, span),
                        hermite_vel(&base, u1, span),
                    ),
                    t0: (t0 + CLOSE_DELAY) as f64,
                    lo,
                    hi,
                });
                t0 = t1;
            }
        }
        // flat hold after the recover: the final cubic must not be
        // extrapolated between its knot span and End (the polynomial
        // runs away outside its fitted range and flashes the shell)
        segs.push(SpringSeg {
            coefs: [1.0, 0.0, 0.0, 0.0],
            t0: (CLOSE_RECOVER_T + CLOSE_DELAY) as f64,
            lo: from,
            hi: idle,
        });
        settle = CLOSE_DELAY + CLOSE_RECOVER_T + 0.06; // End sits past the last segment
    }
    (segs, settle)
}

impl Renderer {
    fn bitmap_props() -> D2D1_BITMAP_PROPERTIES1 {
        D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 0.0,
            dpiY: 0.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            colorContext: ManuallyDrop::new(None),
        }
    }

    // Wrap a scalar in a flat cubic (zero slopes); the compositor holds
    // the End value forever. Mirrors the original helper exactly.
    fn scalar_anim(dcomp: &IDCompositionDevice, v: f32) -> Result<IDCompositionAnimation> {
        unsafe {
            let a = dcomp.CreateAnimation()?;
            a.AddCubic(0.0, v, 0.0, 0.0, 0.0)?;
            a.End(1.0, v)?;
            Ok(a)
        }
    }

    unsafe fn d3d_device() -> Result<ID3D11Device> {
        let mk =
            |driver: windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE| -> Result<ID3D11Device> {
                let mut device: Option<ID3D11Device> = None;
                let mut ctx: Option<windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext> =
                    None;
                D3D11CreateDevice(
                    None,
                    driver,
                    windows::Win32::Foundation::HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&[
                        D3D_FEATURE_LEVEL_11_0,
                        D3D_FEATURE_LEVEL_10_1,
                        D3D_FEATURE_LEVEL_10_0,
                    ]),
                    windows::Win32::Graphics::Direct3D11::D3D11_SDK_VERSION,
                    Some(&mut device),
                    Some(&mut windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL::default()),
                    Some(&mut ctx),
                )?;
                device.ok_or_else(|| {
                    windows::core::Error::from(windows::Win32::Foundation::E_FAIL)
                })
            };
        mk(D3D_DRIVER_TYPE_HARDWARE).or_else(|_| mk(D3D_DRIVER_TYPE_WARP))
    }

    pub fn create(hwnd: HWND, w: f32, h: f32, scale: f32) -> Result<Self> {
        unsafe fn step<T>(label: &str, r: windows::core::Result<T>) -> windows::core::Result<T> {
            r.map_err(|e| {
                eprintln!("[notchbud] create/{label}: {e:#}");
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
            let dc: ID2D1DeviceContext = step(
                "d2d_dc",
                d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE),
            )?;
            let rt: ID2D1RenderTarget = dc.cast()?;
            rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);

            let adapter = dxgi_device.GetAdapter()?;
            let dxgi_factory: IDXGIFactory2 = adapter.GetParent()?;

            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: w.ceil() as u32,
                Height: h.ceil() as u32,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
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
            let target: IDCompositionTarget =
                step("dcomp_target", dcomp.CreateTargetForHwnd(hwnd, false))?;
            // NB: topmost=false — the DComp shell renders BELOW HWND children
            // so the WebView2 content window paints above it. With true, the
            // composition covers the web page and content appears "behind"
            // the island.
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
            step(
                "ctrl_center",
                ctrl.SetCenterX(&Self::scalar_anim(&dcomp, WINDOW_UNITS_W / 2.0 * scale)?),
            )?;
            step(
                "ctrl_center",
                ctrl.SetCenterY(&Self::scalar_anim(&dcomp, 0.0)?),
            )?;
            step(
                "ctrl_scale",
                ctrl.SetScaleX(&Self::scalar_anim(&dcomp, idle[0])?),
            )?;
            step(
                "ctrl_scale",
                ctrl.SetScaleY(&Self::scalar_anim(&dcomp, idle[1])?),
            )?;
            visual.SetEffect(&ctrl)?;
            let island_path = Self::build_island_path(&factory, h, scale)?;
            step("dcomp_commit", dcomp.Commit())?;

            let mk_brush = |c: D2D1_COLOR_F| -> Result<ID2D1SolidColorBrush> {
                rt.CreateSolidColorBrush(&c, None)
            };
            let br_black = mk_brush(island_bg())?;

            Ok(Self {
                dc,
                rt,
                swap,
                dcomp,
                _clip: clip,
                ctrl,
                island_path,
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
                br_black,
            })
        }
    }

    // ---------- island silhouette (windows 0.62: Vector2 points) ----------
    //
    //  * body 420x140.4 u, sides vertical, bottom edge straight;
    //  * top corners: two cubics blending the top edge into the side.
    //  * bottom corners: simple circular arcs of radius ISLAND_BOT_R.
    unsafe fn build_island_path(
        factory: &ID2D1Factory,
        h_window: f32,
        s: f32,
    ) -> Result<ID2D1PathGeometry> {
        let path = factory.CreatePathGeometry()?;
        let sink = path.Open()?;
        let ix = ISLAND_X * s;
        let iw = ISLAND_W * s;
        let ih = (ISLAND_H * s).min(h_window); // keep inside the window
        let fr = ISLAND_FLARE_R * s;
        let rb = ISLAND_BOT_R * s;
        let p = |x: f32, y: f32| Vector2 { X: x, Y: y };
        let bz = |c1: (f32, f32), c2: (f32, f32), e: (f32, f32)| D2D1_BEZIER_SEGMENT {
            point1: p(c1.0, c1.1),
            point2: p(c2.0, c2.1),
            point3: p(e.0, e.1),
        };
        let arc = |e: (f32, f32)| windows::Win32::Graphics::Direct2D::D2D1_ARC_SEGMENT {
            point: p(e.0, e.1),
            size: D2D_SIZE_F {
                width: rb,
                height: rb,
            },
            rotationAngle: 0.0,
            sweepDirection: windows::Win32::Graphics::Direct2D::D2D1_SWEEP_DIRECTION_CLOCKWISE,
            arcSize: windows::Win32::Graphics::Direct2D::D2D1_ARC_SIZE_SMALL,
        };
        let cs = 0.27; // canonical midpoint on the 45° bisector
        let ch = 0.17; // canonical shoulder handle (ch ≈ 0.5 − cs)

        sink.BeginFigure(p(ix - fr, 0.0), D2D1_FIGURE_BEGIN_FILLED);
        // straight top edge between the two flare crests
        sink.AddLine(p(ix + iw + fr, 0.0));
        // top-right flare, crest -> side
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
        // top-left flare, side -> crest
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
    /// (response 0.35, zeta 0.65 — the measured ~6% overshoot); retract = the
    /// fast collapse, then a bounce dip to CLOSE_DIP_FACTOR of the idle tab
    /// before springing back. Unchanged — the shell motion is frozen.
    pub fn animate_spring(&mut self, expand: bool) -> Result<()> {
        // the motion drives the visual SCALE: idle tab -> swell -> full island
        let idle = scale_idle();
        let swell = scale_swell();
        let open = [1.0f32, 1.0f32];

        // ---- estimate where the property currently sits ----
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
            let tc = elapsed - CLOSE_DELAY;
            if tc <= CLOSE_COLLAPSE_END {
                let p = 1.0 - close_progress(tc);
                std::array::from_fn(|i| self.anim_from[i] + (idle[i] - self.anim_from[i]) * p)
            } else {
                let f = close_bounce_factor(tc);
                std::array::from_fn(|i| idle[i] * f)
            }
        };

        // ---- sample the new curve into Hermite segments ----
        // (built by sample_shell_animation so tests can pin the curve)
        let (segs, settle) = sample_shell_animation(expand, from);

        unsafe {
            for i in 0..2 {
                let a = self.dcomp.CreateAnimation()?;
                for seg in &segs {
                    let delta = seg.hi[i] - seg.lo[i];
                    let [d, c, b, q] = seg.coefs;
                    a.AddCubic(
                        seg.t0,
                        seg.lo[i] + delta * d,
                        delta * c,
                        delta * b,
                        delta * q,
                    )?;
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
            let bitmap: ID2D1Bitmap1 =
                dc.CreateBitmapFromDxgiSurface(&surface, Some(&Self::bitmap_props()))?;
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
            self.ctrl.SetCenterX(&Self::scalar_anim(
                &self.dcomp,
                WINDOW_UNITS_W / 2.0 * scale,
            )?)?;
            self.ctrl
                .SetCenterY(&Self::scalar_anim(&self.dcomp, 0.0)?)?;
            self.dcomp.Commit()?;
            Ok(())
        }
    }

    // ---------- frame ----------

    pub fn draw(&mut self, app: &mut App) -> Result<()> {
        unsafe {
            let s = app.scale;
            let ix = ISLAND_X * s;
            let iw = ISLAND_W * s;
            let ih = ISLAND_H * s;

            // hit surface follows the shell (sliver when retracted)
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

            // shell only — content lives in WebView2 above this surface.
            self.rt
                .FillGeometry(&self.island_path, &self.br_black, None);

            let end = self.rt.EndDraw(None, None);
            if let Err(e) = &end {
                eprintln!("[notchbud] draw/EndDraw: {e:#}");
            }
            end?;
            let present = self.swap.Present(1, DXGI_PRESENT(0));
            if present.is_err() {
                eprintln!("[notchbud] draw/Present: {present:?}");
            }
            present.ok()?;
            Ok(())
        }
    }
}
