// shot.rs — full-screen screenshot for the prompt camera button.
//
// GDI grabs the pixels; the image crate downscales + PNG-encodes; base64
// makes the data URL the page previews and Groq reads as an image_url
// part. Payload stays small: downscaled to 768 wide before encoding.
//
// shortcut: primary monitor only; sent screenshots are never archived or
// shown in history — keeps sessions.json small and delta pushes cheap.
// Re-take to resend.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
    SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetDesktopWindow, GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
};

/// Downscaled widths: full (Groq reads detail) and thumb (history +
/// preview display, cheap to push every render).
pub const SHOT_MAX_W: u32 = 768;
pub const SHOT_THUMB_W: u32 = 192;

/// Grab the primary screen as BGRA bytes (top-down). `None` on any
/// failure — the camera button just reports it like any other error.
fn capture_bgra() -> Option<(Vec<u8>, u32, u32)> {
    unsafe {
        let w = GetSystemMetrics(SM_CXSCREEN);
        let h = GetSystemMetrics(SM_CYSCREEN);
        if w <= 0 || h <= 0 {
            return None;
        }
        let desktop = GetDesktopWindow();
        let src_dc = GetDC(Some(desktop));
        if src_dc.is_invalid() {
            return None;
        }
        let dst_dc = CreateCompatibleDC(Some(src_dc));
        if dst_dc.is_invalid() {
            let _ = ReleaseDC(Some(desktop), src_dc);
            return None;
        }
        let bmp = CreateCompatibleBitmap(src_dc, w, h);
        if bmp.is_invalid() {
            let _ = DeleteDC(dst_dc);
            let _ = ReleaseDC(Some(desktop), src_dc);
            return None;
        }
        let old = SelectObject(dst_dc, HGDIOBJ(bmp.0 as *mut _));
        let ok = BitBlt(dst_dc, 0, 0, w, h, Some(src_dc), 0, 0, SRCCOPY).is_ok();
        SelectObject(dst_dc, old);
        let mut out = None;
        if ok {
            let mut bmi = BITMAPINFO::default();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w;
            bmi.bmiHeader.biHeight = -h; // top-down
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB.0;
            let mut px = vec![0u8; (w * h * 4) as usize];
            if GetDIBits(
                dst_dc,
                bmp,
                0,
                h as u32,
                Some(px.as_mut_ptr() as *mut _),
                &mut bmi as *mut _,
                DIB_RGB_COLORS,
            ) != 0
            {
                out = Some((px, w as u32, h as u32));
            }
        }
        let _ = DeleteObject(HGDIOBJ(bmp.0 as *mut _));
        let _ = DeleteDC(dst_dc);
        let _ = ReleaseDC(Some(desktop), src_dc);
        out
    }
}

/// Full pipeline: screen -> (full PNG data URL for Groq, thumb data URL
/// for preview + history). `None` when anything fails. Kept fast on
/// purpose: one full-res resample max, the thumb reuses the downscaled
/// image, and PNG uses fast compression (still lossless).
pub fn capture_shot() -> Option<(String, String)> {
    use image::{
        codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder},
        imageops::{resize, FilterType},
        DynamicImage, ImageBuffer, ImageEncoder as _, Rgba,
    };
    let (mut px, w, h) = capture_bgra()?;
    // GDI 32-bit BI_RGB is B,G,R,unused(0): swizzle to R,G,B,opaque for
    // the image crate (Rgba expects R,G,B,A; unused byte is 0, not 255).
    for px in px.chunks_exact_mut(4) {
        px.swap(0, 2);
        px[3] = 255;
    }
    let rgba = ImageBuffer::<Rgba<u8>, _>::from_raw(w, h, px)?;
    // Downscale once at most; the thumb derives from the result, never
    // from full resolution a second time.
    let full = if w > SHOT_MAX_W {
        DynamicImage::ImageRgba8(resize(
            &rgba,
            SHOT_MAX_W,
            (h * SHOT_MAX_W / w).max(1),
            FilterType::Triangle,
        ))
    } else {
        DynamicImage::ImageRgba8(rgba)
    };
    let tw = SHOT_THUMB_W.min(full.width());
    let thumb = full.resize(
        tw,
        (full.height() * tw / full.width()).max(1),
        FilterType::Triangle,
    );
    let encode = |img: DynamicImage| {
        let mut png = std::io::Cursor::new(Vec::new());
        PngEncoder::new_with_quality(&mut png, CompressionType::Fast, PngFilter::NoFilter)
            .write_image(
                img.as_bytes(),
                img.width(),
                img.height(),
                img.color().into(),
            )
            .ok()?;
        Some(format!(
            "data:image/png;base64,{}",
            B64.encode(png.into_inner())
        ))
    };
    Some((encode(full)?, encode(thumb)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_produces_png_pair_when_a_desktop_exists() {
        // Headless CI has no screen: skip, don't fail.
        let Some((full, thumb)) = capture_shot() else {
            eprintln!("[shot] no desktop — skipping capture test");
            return;
        };
        for url in [&full, &thumb] {
            assert!(url.starts_with("data:image/png;base64,"));
            // PNG magic after the prefix (iVBORw0KGgo = 89 50 4E 47 0D 0A 1A 0A).
            assert!(url["data:image/png;base64,".len()..].starts_with("iVBORw0KGgo"));
        }
        assert!(thumb.len() < full.len(), "thumb must stay the cheap one");
    }
}
