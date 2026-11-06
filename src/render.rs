// render.rs — the notch: the full island is rendered once; a DComp scale
// animation (targets from state.rs) drives the idle tab -> swell -> island
// motion entirely on the compositor thread. Content fades are app-side
// clocks (content_alpha) repainted by WM_PAINT pumps from main.rs.

use std::cell::RefCell;
use std::mem::ManuallyDrop;

use windows::core::{w, Interface, Result, PCWSTR};
use windows::Win32::Foundation::{BOOL, HWND};
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Bitmap1, ID2D1Device, ID2D1DeviceContext, ID2D1Factory, ID2D1Geometry,
    ID2D1Layer, ID2D1PathGeometry, ID2D1RenderTarget, ID2D1SolidColorBrush,
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET,
    D2D1_BITMAP_PROPERTIES1, D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_DRAW_TEXT_OPTIONS_NONE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_LAYER_OPTIONS_NONE, D2D1_LAYER_PARAMETERS,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_0,
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
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, IDWriteTextLayout,
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_HIT_TEST_METRICS, DWRITE_TEXT_METRICS,
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
    close_bounce_factor, close_progress, close_velocity, scale_idle, scale_swell, spring_progress,
    spring_settle, spring_velocity, App, CLOSE_COLLAPSE_END, CLOSE_DELAY, CLOSE_DIP_FACTOR,
    CLOSE_DIP_T, CLOSE_RECOVER_T, CONTENT_FADE, FIELD_INSET, IDLE_RGN_H, IDLE_RGN_HALF_W, ISLAND_BOT_R,
    ISLAND_FLARE_R, ISLAND_H, ISLAND_W, ISLAND_X, OPEN_RESPONSE, OPEN_ZETA, Screen, SLIVER_H,
    SLIVER_W, SLIVER_X, STAGE1_DUR, SWELL_RESPONSE, SWELL_ZETA, WINDOW_UNITS_H, WINDOW_UNITS_W,
};

// ---------- owner slot + clip geometry ----------

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
fn text_hi() -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: 0.949,
        g: 0.949,
        b: 0.969,
        a: 1.0,
    } // #F2F2F7
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
    anim_expand: bool, // true = expansion run was the last one
    anim_start_ms: u64,
    anim_settle: f32,    // seconds until the run anim settles
    anim_from: [f32; 2], // scale values where the run anim started

    tf_hi: IDWriteTextFormat, // title: .headline, white
    tf_greet: IDWriteTextFormat, // greeting: Apple Display cut, larger, white
    br_black: ID2D1SolidColorBrush, // island shell — always opaque
    br_text_hi: ID2D1SolidColorBrush, // content text + pill fill — fades
    br_btn_text: ID2D1SolidColorBrush, // pill label — fades, never touches the shell
    br_select: ID2D1SolidColorBrush, // text-selection highlight — fades with content
    br_field_fill: ID2D1SolidColorBrush, // field fill (subtle gray material) — fades
    br_field_edge: ID2D1SolidColorBrush, // field hairline — fades
    br_text_dim: ID2D1SolidColorBrush, // placeholder text — fades, dimmer
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
        let mk =
            |driver: windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE| -> Result<ID3D11Device> {
                let mut device: Option<ID3D11Device> = None;
                let mut ctx: Option<windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext> =
                    None;
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
                step("dcomp_target", dcomp.CreateTargetForHwnd(hwnd, true))?;
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
            // Atoll home tab: .headline semibold, translated to notchbud island units.
            // Apple system font; falls back to the default face when SF Pro
            // isn't installed on this machine.
            let tf_hi = mk_fmt(w!("SF Pro Text"), 17.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
            // Greeting: the larger Apple cut at a larger size — Apple ships
            // Text for small UI and Display for headlines, so the island
            // follows the same split. Same fallback when SF isn't installed.
            let tf_greet = mk_fmt(w!("SF Pro Display"), 28.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;

            let mk_brush = |c: D2D1_COLOR_F| -> Result<ID2D1SolidColorBrush> {
                rt.CreateSolidColorBrush(&c, None)
            };
            let br_black = mk_brush(island_bg())?;
            let br_text_hi = mk_brush(text_hi())?;
            let br_btn_text = mk_brush(island_bg())?;
            let br_select = mk_brush(text_hi())?;
            // Field + placeholder share the content fade; their base colors
            // are white / text-white and the alpha below keeps them quiet.
            let br_field_fill = mk_brush(text_hi())?;
            let br_field_edge = mk_brush(text_hi())?;
            let br_text_dim = mk_brush(text_hi())?;

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
                tf_greet,
                br_black,
                br_text_hi,
                br_btn_text,
                br_select,
                br_field_fill,
                br_field_edge,
                br_text_dim,
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
        let p = |x: f32, y: f32| D2D_POINT_2F { x, y };
        let bz = |c1: (f32, f32), c2: (f32, f32), e: (f32, f32)| {
            windows::Win32::Graphics::Direct2D::Common::D2D1_BEZIER_SEGMENT {
                point1: p(c1.0, c1.1),
                point2: p(c2.0, c2.1),
                point3: p(e.0, e.1),
            }
        };
        let arc = |e: (f32, f32)| windows::Win32::Graphics::Direct2D::D2D1_ARC_SEGMENT {
            point: p(e.0, e.1),
            size: windows::Win32::Graphics::Direct2D::Common::D2D_SIZE_F {
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
    /// (response 0.35, zeta 0.65 — the measured ~6% overshoot); retract = the
    /// fast collapse, then a bounce dip to CLOSE_DIP_FACTOR of the idle tab
    /// before springing back (the close mirror of the open overshoot: shuts
    /// "too much", then comes back up). The dip is a scale factor on the
    /// idle tab so both axes stay positive. Each sample interval gets a Hermite
    /// cubic segment (exact values and slopes at both ends), dispatched as
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
            // Past the collapse the shell rides the bounce tail around idle
            // (dip to CLOSE_DIP_FACTOR, then back) — the dipped position is
            // near idle regardless of where the close started, so a reversal
            // there restarts from the dipped tab, never snaps.
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
                segs.push(Seg {
                    coefs: hermite(t0, t1, x0, x1, m0, m1),
                    t0: t0 as f64,
                    lo: from,
                    hi: swell,
                });
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
                segs.push(Seg {
                    coefs: hermite(t0, t1, x0, x1, m0, m1),
                    t0: t0 as f64,
                    lo: swell,
                    hi: open,
                });
            }
            settle = STAGE1_DUR + n2 as f32 * dt;
        } else {
            // close: piecewise Hermite straight through Nusa's measured knot
            // table for the collapse (t in seconds, v = value fraction;
            // 1 = open, 0 = idle); v maps directly onto
            // "from + (idle - from)*v", but the press Hermite convention is
            // x = 0 at the start value -> x = 1 - v. Then the bounce tail —
            // the close mirror of the open overshoot: idle dips to
            // CLOSE_DIP_FACTOR (shuts "too much") and springs back to idle.
            // The dip is a scale factor on the idle tab (per-axis, always
            // positive), eased with zero-slope Hermite so the collapse
            // arrives smoothly and the tab settles without flashing.
            for w in crate::state::CLOSE_KNOTS.windows(2) {
                let (t0, v0) = w[0];
                let (t1, v1) = w[1];
                if t1 <= CLOSE_COLLAPSE_END + 1e-6 {
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
            // bounce tail: idle -> dip -> idle, smooth ease in/out each way.
            let dip = std::array::from_fn(|i| idle[i] * CLOSE_DIP_FACTOR);
            segs.push(Seg {
                coefs: hermite(
                    CLOSE_COLLAPSE_END,
                    CLOSE_DIP_T,
                    0.0,
                    1.0,
                    0.0,
                    0.0,
                ),
                t0: (CLOSE_COLLAPSE_END + CLOSE_DELAY) as f64,
                lo: idle,
                hi: dip,
            });
            segs.push(Seg {
                coefs: hermite(CLOSE_DIP_T, CLOSE_RECOVER_T, 0.0, 1.0, 0.0, 0.0),
                t0: (CLOSE_DIP_T + CLOSE_DELAY) as f64,
                lo: dip,
                hi: idle,
            });
            // flat hold after the recover: the final cubic must not be
            // extrapolated between its knot span and End (the polynomial
            // runs away outside its fitted range and flashes the shell)
            segs.push(Seg {
                coefs: [1.0, 0.0, 0.0, 0.0],
                t0: (CLOSE_RECOVER_T + CLOSE_DELAY) as f64,
                lo: from,
                hi: idle,
            });
            settle = CLOSE_DELAY + CLOSE_RECOVER_T + 0.06; // End sits past the last segment
        }

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

    // ---------- draw primitives ----------

    unsafe fn layout(
        &self,
        s: &str,
        tf: &IDWriteTextFormat,
        maxw: f32,
        maxh: f32,
    ) -> Result<IDWriteTextLayout> {
        let utf16: Vec<u16> = s.encode_utf16().collect();
        self.dwrite
            .CreateTextLayout(&utf16, tf, maxw.max(1.0), maxh.max(1.0))
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
            self.rt
                .FillGeometry(&self.island_path, &self.br_black, None);

            // content choreography: hidden until the island is fully open
            // (fades in after the pop settles), gone before the visible
            // shrink begins (fades out over CLOSE_DELAY, and the shrink's
            // knots start that late for exactly that window). The shell
            // itself stays fully opaque — only content brushes fade.
            self.br_black.SetOpacity(1.0);
            let alpha = self.content_alpha(app.expanded);
            if alpha > 0.0 {
                self.br_text_hi.SetOpacity(alpha);
                self.br_btn_text.SetOpacity(alpha);
                self.br_select.SetOpacity(alpha * 0.32);
                // Field material reads quiet: gray fill + hairline, and a
                // dimmer placeholder — all riding the same content fade.
                self.br_field_fill.SetOpacity(alpha * 0.10);
                self.br_field_edge.SetOpacity(alpha * 0.15);
                self.br_text_dim.SetOpacity(alpha * 0.45);
                self.draw_content(ix, iw, ih, s, app);
            }

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

    // Expanded island content, split into managed sections (one Screen owns
    // each frame — no overlaps, no bare fallbacks):
    //   Onboarding — "What should we call you?" + name field. Enter submits;
    //                there is no button.
    //   SignIn     — named but signed out: status + the "Sign in with
    //                ChatGPT" pill, exactly like before. No greeting, no chat.
    //   Home       — signed in, no chat yet: greeting on top + chat field.
    //   Chat       — signed in and chatting: history/stream + pinned field.
    //
    // Two reusable components build every section, in the same font and fade:
    //   Button — the white pill (sign-in); only the label sizes it.
    //   Enter  — the rounded text field (name prompt, chat prompt).
    // The silhouette, fonts, and choreography above are frozen.
    unsafe fn draw_content(&self, ix: f32, iw: f32, ih: f32, s: f32, app: &mut App) {
        const SIGNIN_LABEL: &str = "Sign in with ChatGPT";
        // Fresh click map every frame: stale rects would route clicks to a
        // pill that is no longer on screen.
        app.signin_rect = None;

        // Clip everything to the island body — tall text used to draw
        // straight out of the shell's bottom edge onto the desktop.
        let mask: ID2D1Geometry = match self.island_path.cast() {
            Ok(g) => g,
            Err(_) => return,
        };
        self.rt.PushLayer(
            &D2D1_LAYER_PARAMETERS {
                contentBounds: D2D_RECT_F {
                    left: ix,
                    top: 0.0,
                    right: ix + iw,
                    bottom: ih,
                },
                geometricMask: ManuallyDrop::new(Some(mask)),
                maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                maskTransform: Matrix3x2 {
                    M11: 1.0,
                    M12: 0.0,
                    M21: 0.0,
                    M22: 1.0,
                    M31: 0.0,
                    M32: 0.0,
                },
                opacity: 1.0,
                opacityBrush: ManuallyDrop::new(None),
                layerOptions: D2D1_LAYER_OPTIONS_NONE,
            },
            &self.layer,
        );

        match app.screen() {
            Screen::Onboarding => self.draw_onboarding(ix, iw, ih, s, app),
            Screen::SignIn => self.draw_signin(ix, iw, ih, s, app, SIGNIN_LABEL),
            Screen::Home => self.draw_home(ix, iw, ih, s, app),
            Screen::Chat => self.draw_chat(ix, iw, ih, s, app),
        }
        self.rt.PopLayer();
        // Only the chat viewport scrolls; every other section is fixed.
        if app.screen() != Screen::Chat {
            app.chat.max_scroll = 0.0;
            app.chat.scroll = 0.0;
            // No selectable text off the Chat screen: drop the viewport so
            // clicks and the I-beam never act on a stale rect. The sign-in
            // screen draws no field either, so its geometry goes too (Home
            // and Onboarding republish theirs via draw_field).
            app.chat_viewport = None;
            if app.screen() == Screen::SignIn {
                app.input_field = None;
            }
        }
    }

    // ---------- reusable components ----------

    /// Measure a line for centering. Returns (layout, w, h); None when the
    /// text engine fails (the caller then skips that line, never misplaces).
    unsafe fn measure_line(
        &self,
        text: &str,
        maxw: f32,
        maxh: f32,
    ) -> (Option<IDWriteTextLayout>, f32, f32) {
        self.measure_with(text, &self.tf_hi, maxw, maxh)
    }

    /// Same, with an explicit text format (the greeting uses the larger
    /// Apple Display cut instead of the standard UI format).
    unsafe fn measure_with(
        &self,
        text: &str,
        tf: &IDWriteTextFormat,
        maxw: f32,
        maxh: f32,
    ) -> (Option<IDWriteTextLayout>, f32, f32) {
        if text.is_empty() {
            return (None, 0.0, 0.0);
        }
        match self.layout(text, tf, maxw, maxh) {
            Ok(l) => {
                let mut m = DWRITE_TEXT_METRICS::default();
                match l.GetMetrics(&mut m) {
                    Ok(()) => (Some(l), m.width, m.height),
                    Err(_) => (None, 0.0, 0.0),
                }
            }
            Err(_) => (None, 0.0, 0.0),
        }
    }

    /// Draw a measured line centered in the island (wide blocks dock left
    /// with the standard margin instead of squeezing).
    unsafe fn draw_centered(
        &self,
        layout: &IDWriteTextLayout,
        w: f32,
        ix: f32,
        iw: f32,
        s: f32,
        y: f32,
    ) {
        let x = if w < iw * 0.9 {
            ix + (iw - w) / 2.0
        } else {
            ix + 24.0 * s
        };
        self.rt.DrawTextLayout(
            D2D_POINT_2F { x, y },
            layout,
            &self.br_text_hi,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
        );
    }

    /// Button component: white pill sized around the label + dark label.
    /// Returns the pill rect in window px (for click routing). The sign-in
    /// pill renders through here.
    unsafe fn draw_button(
        &self,
        label: &str,
        center_x: f32,
        top_y: f32,
        iw: f32,
        s: f32,
    ) -> Option<[f32; 4]> {
        let (_, label_tw, label_th) = self.measure_line(label, iw - 24.0 * s, 60.0 * s);
        if label_tw <= 0.0 {
            return None;
        }
        let pad_x = 22.0 * s;
        let pad_y = 10.0 * s;
        let btn_w = (label_tw + 2.0 * pad_x).min(iw - 32.0 * s).max(label_tw);
        let btn_h = label_th + 2.0 * pad_y;
        let bx = center_x - btn_w / 2.0;
        let by = top_y;
        let pill = windows::Win32::Graphics::Direct2D::D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left: bx,
                top: by,
                right: bx + btn_w,
                bottom: by + btn_h,
            },
            radiusX: btn_h / 2.0,
            radiusY: btn_h / 2.0,
        };
        let factory: ID2D1Factory = match self.rt.GetFactory() {
            Ok(f) => f,
            Err(_) => return None,
        };
        if let Ok(geo) = factory.CreateRoundedRectangleGeometry(&pill) {
            let _ = self.rt.FillGeometry(&geo, &self.br_text_hi, None);
        } else {
            return None;
        }
        if let Ok(l) = self.layout(label, &self.tf_hi, iw - 24.0 * s, 60.0 * s) {
            self.rt.DrawTextLayout(
                D2D_POINT_2F {
                    x: bx + (btn_w - label_tw) / 2.0,
                    y: by + pad_y,
                },
                &l,
                &self.br_btn_text,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
        }
        Some([bx, by, bx + btn_w, by + btn_h])
    }

    /// Enter component: pill field with a quiet gray material (fill +
    /// hairline, iMessage-style) + single-line text with a caret. `raw` is
    /// the undecorated field text; `prefix` ("› " or "") and the caret are
    /// layout decoration. `sel` is a highlight in raw UTF-16 offsets (mapped
    /// past the prefix for drawing). Empty fields show the placeholder in
    /// dimmed text. Symmetric side padding; overlong lines reveal their tail
    /// (where the caret is) — the island clip contains the head that slides
    /// out left. Publishes the frame's geometry for mouse hit-testing.
    unsafe fn draw_field(
        &self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        s: f32,
        app: &mut App,
        raw: &str,
        prefix: &'static str,
        placeholder: &'static str,
        sel: Option<(usize, usize)>,
    ) {
        let field = windows::Win32::Graphics::Direct2D::D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left: x,
                top: y,
                right: x + w,
                bottom: y + h,
            },
            radiusX: h / 2.0,
            radiusY: h / 2.0,
        };
        if let Ok(factory) = self.rt.GetFactory() {
            if let Ok(geo) = factory.CreateRoundedRectangleGeometry(&field) {
                // Material first: subtle fill, then a hairline on top.
                let _ = self.rt.FillGeometry(&geo, &self.br_field_fill, None);
                let _ = self.rt.DrawGeometry(&geo, &self.br_field_edge, 1.0 * s, None);
            }
        }
        let shown = crate::state::field_shown(raw, prefix, placeholder);
        // Even padding on both inside edges — no glyph zone anymore.
        let pad_x = 16.0 * s;
        let inner = (w - 2.0 * pad_x).max(1.0);
        if let Ok(l) = self.layout(&shown, &self.tf_hi, 4000.0, h) {
            let mut m = DWRITE_TEXT_METRICS::default();
            let (tw, th) = match l.GetMetrics(&mut m) {
                Ok(()) => (m.width, m.height),
                Err(_) => {
                    app.input_field = None;
                    return;
                }
            };
            // Tail-visible: long lines shift left so the caret stays in view.
            let mut tx = x + pad_x;
            if tw > inner {
                tx -= tw - inner;
            }
            // Tail-visible: long lines shift left so the caret stays in view;
            // the island mask clips the head that hangs past the field edge.
            let ty = y + ((h - th) / 2.0).max(0.0);
            // Highlight behind the text, same wash as the chat viewport.
            if let Some((a, b)) = sel {
                if b > a {
                    let pu = prefix.encode_utf16().count() as u32;
                    let total = shown.encode_utf16().count() as u32;
                    let la = (a as u32 + pu).min(total);
                    let lb = (b as u32 + pu).min(total);
                    if lb > la {
                        let mut need = 0u32;
                        let _ = l.HitTestTextRange(la, lb - la, tx, ty, None, &mut need);
                        if need > 0 {
                            let mut metrics: Vec<DWRITE_HIT_TEST_METRICS> =
                                vec![std::mem::zeroed(); need as usize];
                            let mut got = 0u32;
                            if l
                                .HitTestTextRange(la, lb - la, tx, ty, Some(&mut metrics), &mut got)
                                .is_ok()
                            {
                                for hm in metrics.iter().take(got as usize) {
                                    self.rt.FillRectangle(
                                        &D2D_RECT_F {
                                            left: hm.left,
                                            top: hm.top,
                                            right: hm.left + hm.width,
                                            bottom: hm.top + hm.height,
                                        },
                                        &self.br_select,
                                    );
                                }
                            }
                        }
                    }
                }
            }
            // Placeholder reads dimmer than typed text (secondary label).
            let text_brush = if raw.is_empty() {
                &self.br_text_dim
            } else {
                &self.br_text_hi
            };
            self.rt.DrawTextLayout(
                D2D_POINT_2F { x: tx, y: ty },
                &l,
                text_brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
            app.input_field = Some(crate::state::InputFieldGeom {
                rect: [x, y, x + w, y + h],
                origin: [tx, ty],
                prefix,
                placeholder,
            });
        } else {
            app.input_field = None;
        }
    }

    /// Status lines shared by Home and Chat: transient notice, error, and the
    /// identity-only plan gate. Empty when there is nothing to say.
    fn status_text(app: &App) -> String {
        let mut lines: Vec<String> = Vec::new();
        if let Some(n) = &app.chat.notice {
            if !n.trim().is_empty() {
                lines.push(n.clone());
            }
        }
        if let Some(e) = &app.chat.error {
            if !e.trim().is_empty() {
                lines.push(e.clone());
            }
        }
        if app.chat.connected() && !app.chat.plan_ready() && app.chat.error.is_none() {
            lines.push("ChatGPT plan use isn't enabled — Connect again to authorize it.".into());
        }
        lines.join("\n")
    }

    // ---------- sections ----------

    /// Onboarding: title + name field, vertically centered as one column.
    /// Enter submits; there is no button and clicks do nothing here. A
    /// rejection reason appears under the field only when a submit fails
    /// validation.
    unsafe fn draw_onboarding(&self, ix: f32, iw: f32, ih: f32, s: f32, app: &mut App) {
        const TITLE: &str = "What should we call you?";
        const PLACEHOLDER: &str = "Type your name…";

        let maxw = iw - 48.0 * s;
        let (title_l, title_w, title_h) = self.measure_line(TITLE, maxw, 60.0 * s);
        let title_h = if title_l.is_some() { title_h } else { 30.0 * s };
        let field_h = 34.0 * s;
        let err_h = match app.name_error.clone() {
            Some(e) if !e.is_empty() => {
                let (_, _, h) = self.measure_line(&e, maxw, 60.0 * s);
                if h > 0.0 { 8.0 * s + h } else { 0.0 }
            }
            _ => 0.0,
        };
        // Whole column (title + field + reason) centered in the shell.
        let total = title_h + 10.0 * s + field_h + err_h;
        let mut cy = ((ih - total) / 2.0).max(10.0 * s);
        if let Some(l) = title_l {
            self.draw_centered(&l, title_w, ix, iw, s, cy);
        }
        cy += title_h + 10.0 * s;

        // Name field (Enter component). Column-centered, so side margins
        // are even by construction; cap the width like a form field.
        let field_w = (320.0 * s).min(iw - 2.0 * FIELD_INSET * s);
        let field_x = ix + (iw - field_w) / 2.0;
        let field_y = cy;
        let draft = app.name_draft.clone();
        let sel = app.name_sel.normalized(&draft);
        self.draw_field(
            field_x, field_y, field_w, field_h, s, app, &draft, "", PLACEHOLDER, sel,
        );
        cy = field_y + field_h + 8.0 * s;

        if let Some(err) = app.name_error.clone() {
            if !err.is_empty() {
                let (err_l, err_w, _) = self.measure_line(&err, maxw, 60.0 * s);
                if let Some(l) = err_l {
                    self.draw_centered(&l, err_w, ix, iw, s, cy);
                }
            }
        }
    }

    /// SignIn: named but signed out — status lines above the sign-in pill,
    /// vertically centered as one column, exactly like before. No greeting,
    /// no chat field: they wait for the connection.
    unsafe fn draw_signin(
        &self,
        ix: f32,
        iw: f32,
        ih: f32,
        s: f32,
        app: &mut App,
        signin_label: &str,
    ) {
        // Split the actionable pill off the status text when present.
        let full = app.chat.slot_text();
        let show_button = app.chat.shows_signin_button();
        let (body, has_button) = if show_button {
            if full == signin_label {
                (String::new(), true)
            } else if let Some(prefix) = full.strip_suffix(&format!("\n{signin_label}")) {
                (prefix.to_owned(), true)
            } else {
                (full, false)
            }
        } else {
            (full, false)
        };

        let (body_layout, bw, bh) = if body.is_empty() {
            (None, 0.0, 0.0)
        } else if let Ok(l) = self.layout(&body, &self.tf_hi, iw - 24.0 * s, ih) {
            let mut m = DWRITE_TEXT_METRICS::default();
            let (w, h) = match l.GetMetrics(&mut m) {
                Ok(()) => (m.width, m.height),
                Err(_) => (0.0, 0.0),
            };
            (Some(l), w, h)
        } else {
            (None, 0.0, 0.0)
        };
        if !has_button {
            // Signing in: transient status text only, centered, no pill.
            if let Some(l) = body_layout {
                let bx = if bw < iw * 0.9 {
                    ix + (iw - bw) / 2.0
                } else {
                    ix + 24.0 * s
                };
                let by = ((ih - bh) / 2.0).max(8.0 * s);
                self.rt.DrawTextLayout(
                    D2D_POINT_2F { x: bx, y: by },
                    &l,
                    &self.br_text_hi,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                );
            }
            app.chat.max_scroll = 0.0;
            app.chat.scroll = 0.0;
            return;
        }
        // Pill height for the column budget (draw_button sizes itself the
        // same way around the same label).
        let (_, _, label_th) = self.measure_line(signin_label, iw - 24.0 * s, 60.0 * s);
        let pad_y = 10.0 * s;
        let gap = if body.is_empty() { 0.0 } else { 12.0 * s };
        let btn_h = label_th + 2.0 * pad_y;
        let total = bh + gap + btn_h;
        // Whole column (status + pill) is vertically centered in the shell.
        let mut cy = (ih - total) / 2.0;
        if cy < 8.0 * s {
            cy = 8.0 * s;
        }
        if let Some(l) = body_layout {
            let bx = if bw < iw * 0.9 {
                ix + (iw - bw) / 2.0
            } else {
                ix + 24.0 * s
            };
            self.rt.DrawTextLayout(
                D2D_POINT_2F { x: bx, y: cy },
                &l,
                &self.br_text_hi,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
            cy += bh + gap;
        }
        if let Some(rect) = self.draw_button(signin_label, ix + iw / 2.0, cy, iw, s) {
            app.signin_rect = Some(rect);
        }
        app.chat.max_scroll = 0.0;
        app.chat.scroll = 0.0;
    }

    /// Home: signed in, no chat yet — the time greeting on top plus the
    /// chat field bottom-anchored below it, with transient status (plan
    /// gate, send errors, connecting) in between. No pill: this section
    /// only exists while connected.
    unsafe fn draw_home(&self, ix: f32, iw: f32, ih: f32, s: f32, app: &mut App) {
        const CHAT_PLACEHOLDER: &str = "Ask anything…";
        let maxw = iw - 2.0 * FIELD_INSET * s;

        // Bottom-anchored chat field first, so it stays whole at every scale.
        // Even FIELD_INSET margins on the left, right, and bottom.
        let field_h = 34.0 * s;
        let field_x = ix + FIELD_INSET * s;
        let field_y = ih - field_h - FIELD_INSET * s;

        // Top anchor: the greeting, until the first chat hides it.
        // Larger Apple Display cut — the greeting is the island's headline.
        // Sits a touch below the top edge rather than kissing it.
        let greeting = app.greeting();
        let mut cy = 20.0 * s;
        if !greeting.is_empty() {
            let (t_l, t_w, t_h) = self.measure_with(&greeting, &self.tf_greet, maxw, 60.0 * s);
            if let Some(l) = t_l {
                self.draw_centered(&l, t_w, ix, iw, s, cy);
                cy += t_h + 8.0 * s;
            } else {
                cy += 28.0 * s;
            }
        }

        // Middle status, only what fits above the field.
        let mut status = Self::status_text(app);
        if status.is_empty() && app.chat.signing_in {
            status = "Connecting to ChatGPT…".into();
        }
        if !status.is_empty() {
            let room = (field_y - 6.0 * s - cy).max(0.0);
            let (st_l, st_w, st_h) = self.measure_line(&status, maxw, room.max(1.0));
            if let Some(l) = st_l {
                if st_h <= room + 1.0 {
                    self.draw_centered(&l, st_w, ix, iw, s, cy);
                }
            }
        }

        // Chat field (Enter component).
        let field_w = maxw;
        let raw = app.chat.input.clone();
        let sel = app.chat.input_sel.normalized(&raw);
        self.draw_field(
            field_x,
            field_y,
            field_w,
            field_h,
            s,
            app,
            &raw,
            "› ",
            CHAT_PLACEHOLDER,
            sel,
        );
    }

    /// Chat: signed in and chatting — history/stream viewport on top, chat
    /// field pinned at the bottom. The pill never appears here: losing the
    /// connection routes the frame back to SignIn (which keeps the history).
    /// The viewport text is mouse-selectable: drag highlights it (Ctrl+C
    /// copies the highlight, else the whole transcript).
    unsafe fn draw_chat(&self, ix: f32, iw: f32, ih: f32, s: f32, app: &mut App) {
        use crate::state::Screen;
        debug_assert_eq!(app.screen(), Screen::Chat);
        const CHAT_PLACEHOLDER: &str = "Ask anything…";
        let maxw = iw - 2.0 * FIELD_INSET * s;

        // Bottom-anchored input field first, so the history viewport is
        // whatever space is left above it. Even FIELD_INSET margins on the
        // left, right, and bottom.
        let field_w = maxw;
        let field_h = 32.0 * s;
        let field_x = ix + FIELD_INSET * s;
        let field_y = ih - field_h - FIELD_INSET * s;

        let viewport_top = 10.0 * s;
        let viewport_bottom = field_y - 8.0 * s;
        let viewport = (viewport_bottom - viewport_top).max(1.0);
        // Published for select-drag hit gating + the I-beam cursor; cleared
        // on every non-Chat frame by draw_content.
        app.chat_viewport = Some([ix, viewport_top, ix + iw, viewport_bottom]);

        // Status above history; both bottom-anchored. Wheel scroll reveals
        // older lines. Single source with hit-testing: caret_from_point
        // rebuilds this exact string, width, and origin.
        let full = app.chat.viewport_text();
        if !full.is_empty() {
            if let Ok(l) = self.layout(&full, &self.tf_hi, maxw, 4000.0) {
                let mut m = DWRITE_TEXT_METRICS::default();
                let th = match l.GetMetrics(&mut m) {
                    Ok(()) => m.height,
                    Err(_) => viewport,
                };
                // Chat log always docks left with the standard margin —
                // never centered, however short the turn.
                let x = ix + 24.0 * s;
                app.chat.max_scroll = (th - viewport).max(0.0);
                app.chat.scroll = app.chat.scroll.clamp(0.0, app.chat.max_scroll);
                // Bottom-anchor above the field: newest line kisses the
                // viewport bottom at scroll 0; scrolling slides older lines
                // down into view. The axis-aligned clip confines the block
                // to the viewport so scrolled text can never paint over the
                // pinned field below.
                let y = viewport_bottom - th + app.chat.scroll;
                self.rt.PushAxisAlignedClip(
                    &D2D_RECT_F {
                        left: ix,
                        top: viewport_top,
                        right: ix + iw,
                        bottom: viewport_bottom,
                    },
                    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                );
                // Selection highlight first (white wash), text over it —
                // clipped to the viewport like the text itself.
                if let Some((spos, epos)) = app.chat.normalized_selection() {
                    let mut need = 0u32;
                    let _ = l.HitTestTextRange(
                        spos as u32,
                        (epos - spos) as u32,
                        x,
                        y,
                        None,
                        &mut need,
                    );
                    if need > 0 {
                        let mut metrics: Vec<DWRITE_HIT_TEST_METRICS> =
                            vec![std::mem::zeroed(); need as usize];
                        let mut got = 0u32;
                        if l
                            .HitTestTextRange(
                                spos as u32,
                                (epos - spos) as u32,
                                x,
                                y,
                                Some(&mut metrics),
                                &mut got,
                            )
                            .is_ok()
                        {
                            for hm in metrics.iter().take(got as usize) {
                                self.rt.FillRectangle(
                                    &D2D_RECT_F {
                                        left: hm.left,
                                        top: hm.top,
                                        right: hm.left + hm.width,
                                        bottom: hm.top + hm.height,
                                    },
                                    &self.br_select,
                                );
                            }
                        }
                    }
                }
                self.rt.DrawTextLayout(
                    D2D_POINT_2F { x, y },
                    &l,
                    &self.br_text_hi,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                );
                self.rt.PopAxisAlignedClip();
            }
        } else {
            app.chat.max_scroll = 0.0;
            app.chat.scroll = 0.0;
        }

        let raw = app.chat.input.clone();
        let sel = app.chat.input_sel.normalized(&raw);
        self.draw_field(
            field_x,
            field_y,
            field_w,
            field_h,
            s,
            app,
            &raw,
            "› ",
            CHAT_PLACEHOLDER,
            sel,
        );
    }
}

/// Map a window-px point to a UTF-16 caret offset in the Chat viewport
/// text. Rebuilds the exact layout draw_chat draws (same string, same
/// width, same bottom-anchored origin incl. the scroll clamp) so a
/// select-drag lands on glyphs. None outside the viewport, off the Chat
/// screen, or when there is no text.
pub fn caret_from_point(app: &App, px: f32, py: f32) -> Option<usize> {
    use crate::state::{utf16_len, Screen, ISLAND_H, ISLAND_W, ISLAND_X};
    if app.screen() != Screen::Chat {
        return None;
    }
    let s = app.scale;
    let ix = ISLAND_X * s;
    let iw = ISLAND_W * s;
    let ih = ISLAND_H * s;
    let maxw = iw - 2.0 * FIELD_INSET * s;
    let field_h = 32.0 * s;
    let field_y = ih - field_h - FIELD_INSET * s;
    let viewport_top = 10.0 * s;
    let viewport_bottom = field_y - 8.0 * s;
    if py < viewport_top || py > viewport_bottom || px < ix || px > ix + iw {
        return None;
    }
    let full = app.chat.viewport_text();
    if full.is_empty() {
        return None;
    }
    SLOT.with_borrow(|slot| {
        let rd = slot.as_ref()?;
        unsafe {
            let l = rd.layout(&full, &rd.tf_hi, maxw, 4000.0).ok()?;
            let mut m = DWRITE_TEXT_METRICS::default();
            l.GetMetrics(&mut m).ok()?;
            let viewport = (viewport_bottom - viewport_top).max(1.0);
            let max_scroll = (m.height - viewport).max(0.0);
            let scroll = app.chat.scroll.clamp(0.0, max_scroll);
            let x = ix + 24.0 * s;
            let y = viewport_bottom - m.height + scroll;
            // HitTestPoint takes layout-space coords: the drawn origin off.
            let mut trailing = BOOL::default();
            let mut inside = BOOL::default();
            let mut hm: DWRITE_HIT_TEST_METRICS = std::mem::zeroed();
            l.HitTestPoint(px - x, py - y, &mut trailing, &mut inside, &mut hm)
                .ok()?;
            let pos = hm.textPosition as usize
                + if trailing.0 != 0 {
                    hm.length as usize
                } else {
                    0
                };
            Some(pos.min(utf16_len(&full)))
        }
    })
}

/// Map a window-px point to a UTF-16 caret offset in the visible input
/// field's raw text (name prompt or chat input). Uses the geometry
/// draw_field published this frame plus the same shown string, so a
/// select-drag lands on glyphs even when a long line is tail-shifted.
/// None outside the field, on the SignIn screen, or when the field is
/// empty (the placeholder is not selectable).
pub fn field_caret_from_point(app: &App, px: f32, py: f32) -> Option<usize> {
    use crate::state::{field_shown, utf16_len, Screen};
    let g = app.input_field.as_ref()?;
    if px < g.rect[0] || px >= g.rect[2] || py < g.rect[1] || py >= g.rect[3] {
        return None;
    }
    let raw: &str = match app.screen() {
        Screen::Onboarding => &app.name_draft,
        Screen::Home | Screen::Chat => &app.chat.input,
        Screen::SignIn => return None,
    };
    if raw.is_empty() {
        return None;
    }
    let shown = field_shown(raw, g.prefix, g.placeholder);
    let pu = g.prefix.encode_utf16().count();
    SLOT.with_borrow(|slot| {
        let rd = slot.as_ref()?;
        unsafe {
            let h = (g.rect[3] - g.rect[1]).max(1.0);
            let l = rd.layout(&shown, &rd.tf_hi, 4000.0, h).ok()?;
            let mut trailing = BOOL::default();
            let mut inside = BOOL::default();
            let mut hm: DWRITE_HIT_TEST_METRICS = std::mem::zeroed();
            l.HitTestPoint(
                px - g.origin[0],
                py - g.origin[1],
                &mut trailing,
                &mut inside,
                &mut hm,
            )
            .ok()?;
            let pos = hm.textPosition as usize
                + if trailing.0 != 0 {
                    hm.length as usize
                } else {
                    0
                };
            Some(pos.saturating_sub(pu).min(utf16_len(raw)))
        }
    })
}
