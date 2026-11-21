//! Pictures for notifications without GTK: PNG through cairo, JPEG through
//! libturbojpeg, and SVG (BIMI logos) through librsvg, the decoders GTK uses
//! from the same runtime. Everything ends as a square of RGBA pixels.

use std::ffi::{c_int, c_ulong, c_void};

/// Photos are cropped square and scaled to at most this many pixels.
pub const SIZE: usize = 96;

/// Straight (not premultiplied) RGBA pixels, `side` × `side`.
#[derive(Clone)]
pub struct Picture {
    pub side: usize,
    pub rgba: Vec<u8>,
}

#[allow(non_camel_case_types)]
type tjhandle = *mut c_void;
const TJPF_RGBA: c_int = 7;

unsafe extern "C" {
    fn tjInitDecompress() -> tjhandle;
    fn tjDecompressHeader3(
        handle: tjhandle,
        buf: *const u8,
        size: c_ulong,
        width: *mut c_int,
        height: *mut c_int,
        subsamp: *mut c_int,
        colorspace: *mut c_int,
    ) -> c_int;
    fn tjDecompress2(
        handle: tjhandle,
        buf: *const u8,
        size: c_ulong,
        dst: *mut u8,
        width: c_int,
        pitch: c_int,
        height: c_int,
        format: c_int,
        flags: c_int,
    ) -> c_int;
    fn tjDestroy(handle: tjhandle) -> c_int;

    fn rsvg_handle_new_from_data(data: *const u8, len: usize, error: *mut *mut glib::ffi::GError) -> *mut c_void;
    fn rsvg_handle_render_document(
        handle: *mut c_void,
        cr: *mut cairo::ffi::cairo_t,
        viewport: *const Viewport,
        error: *mut *mut glib::ffi::GError,
    ) -> glib::ffi::gboolean;
}

#[repr(C)]
struct Viewport {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

/// Decodes PNG or JPEG `bytes` and scales the middle square to at most
/// SIZE.
pub fn decode(bytes: &[u8]) -> Option<Picture> {
    let (w, h, rgba) = if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        png(bytes)?
    } else if bytes.starts_with(&[0xff, 0xd8]) {
        jpeg(bytes)?
    } else {
        return None;
    };
    square(w, h, &rgba)
}

/// An SVG logo drawn at SIZE, or None when it is not SVG.
pub fn svg(bytes: &[u8]) -> Option<Picture> {
    if !String::from_utf8_lossy(&bytes[..bytes.len().min(512)]).contains("<svg") {
        return None;
    }
    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, SIZE as i32, SIZE as i32).ok()?;
    {
        let cr = cairo::Context::new(&surface).ok()?;
        let mut error = std::ptr::null_mut();
        // A handle made from data has no base location, so nothing outside
        // the document is loaded; librsvg does not animate.
        let handle = unsafe { rsvg_handle_new_from_data(bytes.as_ptr(), bytes.len(), &mut error) };
        if handle.is_null() {
            unsafe { glib::ffi::g_clear_error(&mut error) };
            return None;
        }
        let viewport = Viewport { x: 0.0, y: 0.0, width: SIZE as f64, height: SIZE as f64 };
        let ok = unsafe { rsvg_handle_render_document(handle, cr.to_raw_none(), &viewport, &mut error) };
        unsafe {
            glib::ffi::g_clear_error(&mut error);
            glib::gobject_ffi::g_object_unref(handle as *mut _);
        }
        if ok == 0 {
            return None;
        }
    }
    surface.flush();
    let rgba = unpremultiply(&mut surface)?;
    Some(Picture { side: SIZE, rgba })
}

fn png(bytes: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    let mut surface = cairo::ImageSurface::create_from_png(&mut std::io::Cursor::new(bytes)).ok()?;
    let (w, h) = (surface.width() as usize, surface.height() as usize);
    // Formats other than ARGB32 and RGB24 are drawn onto one.
    if !matches!(surface.format(), cairo::Format::ARgb32 | cairo::Format::Rgb24) {
        let target = cairo::ImageSurface::create(cairo::Format::ARgb32, w as i32, h as i32).ok()?;
        {
            let cr = cairo::Context::new(&target).ok()?;
            cr.set_source_surface(&surface, 0.0, 0.0).ok()?;
            cr.paint().ok()?;
        }
        surface = target;
    }
    let opaque = surface.format() == cairo::Format::Rgb24;
    let mut rgba = unpremultiply(&mut surface)?;
    if opaque {
        rgba.chunks_mut(4).for_each(|p| p[3] = 255);
    }
    Some((w, h, rgba))
}

/// Cairo's premultiplied native-endian ARGB as straight RGBA.
fn unpremultiply(surface: &mut cairo::ImageSurface) -> Option<Vec<u8>> {
    let (w, h, stride) = (surface.width() as usize, surface.height() as usize, surface.stride() as usize);
    let data = surface.data().ok()?;
    let mut out = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            let i = y * stride + x * 4;
            let px = u32::from_ne_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
            let a = (px >> 24) & 0xff;
            let un = |c: u32| (c * 255 + a / 2).checked_div(a).map_or(0, |v| v.min(255) as u8);
            out.extend_from_slice(&[un((px >> 16) & 0xff), un((px >> 8) & 0xff), un(px & 0xff), a as u8]);
        }
    }
    Some(out)
}

fn jpeg(bytes: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    unsafe {
        let handle = tjInitDecompress();
        if handle.is_null() {
            return None;
        }
        let (mut w, mut h, mut sub, mut cs) = (0, 0, 0, 0);
        let mut out = None;
        if tjDecompressHeader3(handle, bytes.as_ptr(), bytes.len() as c_ulong, &mut w, &mut h, &mut sub, &mut cs) == 0
            && w > 0
            && h > 0
            && (w as usize) * (h as usize) <= 64 << 20
        {
            let mut rgba = vec![0u8; w as usize * h as usize * 4];
            if tjDecompress2(handle, bytes.as_ptr(), bytes.len() as c_ulong, rgba.as_mut_ptr(), w, 0, h, TJPF_RGBA, 0) == 0 {
                out = Some((w as usize, h as usize, rgba));
            }
        }
        tjDestroy(handle);
        out
    }
}

/// The middle square of a `w` × `h` image, averaged down to at most SIZE.
fn square(w: usize, h: usize, pixels: &[u8]) -> Option<Picture> {
    let side = w.min(h);
    if side == 0 {
        return None;
    }
    let stride = w * 4;
    let (x0, y0) = ((w - side) / 2, (h - side) / 2);
    let size = side.min(SIZE);
    let mut out = vec![0u8; size * size * 4];
    for oy in 0..size {
        let (sy0, sy1) = (y0 + oy * side / size, y0 + ((oy + 1) * side / size).max(oy * side / size + 1));
        for ox in 0..size {
            let (sx0, sx1) = (x0 + ox * side / size, x0 + ((ox + 1) * side / size).max(ox * side / size + 1));
            let mut sum = [0u32; 4];
            for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    let i = sy * stride + sx * 4;
                    for c in 0..4 {
                        sum[c] += pixels[i + c] as u32;
                    }
                }
            }
            let n = ((sy1 - sy0) * (sx1 - sx0)) as u32;
            let o = (oy * size + ox) * 4;
            for c in 0..4 {
                out[o + c] = (sum[c] / n) as u8;
            }
        }
    }
    Some(Picture { side: size, rgba: out })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_round_trip() {
        let s = cairo::ImageSurface::create(cairo::Format::ARgb32, 4, 2).unwrap();
        {
            let cr = cairo::Context::new(&s).unwrap();
            cr.set_source_rgb(1.0, 0.0, 0.0);
            cr.paint().unwrap();
        }
        let mut bytes = Vec::new();
        s.write_to_png(&mut bytes).unwrap();
        let p = decode(&bytes).unwrap();
        assert_eq!(p.side, 2);
        assert_eq!(&p.rgba[..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn jpeg_square() {
        let (w, h) = (300u16, 200u16);
        let pixels: Vec<u8> = (0..w as usize * h as usize).flat_map(|_| [0u8, 200, 0]).collect();
        let mut bytes = Vec::new();
        jpeg_encoder::Encoder::new(&mut bytes, 90).encode(&pixels, w, h, jpeg_encoder::ColorType::Rgb).unwrap();
        let p = decode(&bytes).unwrap();
        assert_eq!(p.side, SIZE);
        let px = &p.rgba[..4];
        assert!(px[0] < 20 && px[1] > 180 && px[2] < 20 && px[3] == 255, "{px:?}");
    }

    #[test]
    fn svg_logo() {
        let p = svg(br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><rect width="10" height="10" fill="#00f"/></svg>"##).unwrap();
        assert_eq!(p.side, SIZE);
        assert_eq!(&p.rgba[..4], &[0, 0, 255, 255]);
    }
}
