//! Images in a message's text: how one is drawn in the composer, and the
//! dialogs of its menu.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, glib, graphene};
use salutations_core::compose::Size;

mod imp {
    use super::*;
    use gtk::subclass::prelude::*;

    #[derive(Default)]
    pub struct SizedImage {
        pub texture: RefCell<Option<gdk::Texture>>,
        pub size: Cell<(i32, i32)>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SizedImage {
        const NAME: &'static str = "SalutationsSizedImage";
        type Type = super::SizedImage;
        type Interfaces = (gdk::Paintable,);
    }

    impl ObjectImpl for SizedImage {}

    impl PaintableImpl for SizedImage {
        fn intrinsic_width(&self) -> i32 {
            self.size.get().0
        }

        fn intrinsic_height(&self) -> i32 {
            self.size.get().1
        }

        fn snapshot(&self, snapshot: &gdk::Snapshot, width: f64, height: f64) {
            if let Some(t) = self.texture.borrow().as_ref() {
                t.snapshot(snapshot, width, height);
            }
        }
    }
}

glib::wrapper! {
    /// A texture drawn at a size of its own. A picture of the texture itself
    /// asks for the image's full size, which would widen the editor.
    pub struct SizedImage(ObjectSubclass<imp::SizedImage>) @implements gdk::Paintable;
}

impl SizedImage {
    pub fn new(texture: &gdk::Texture, size: (i32, i32)) -> Self {
        let this: Self = glib::Object::new();
        this.set(texture, size);
        this
    }

    pub fn set(&self, texture: &gdk::Texture, size: (i32, i32)) {
        use gtk::subclass::prelude::ObjectSubclassIsExt;
        *self.imp().texture.borrow_mut() = Some(texture.clone());
        self.imp().size.set((size.0.max(1), size.1.max(1)));
        self.invalidate_size();
        self.invalidate_contents();
    }
}

/// Images wider than this start at this width.
pub const START_WIDTH: u32 = 600;

/// The size a new image starts at.
pub fn start_size(pixels: (i32, i32)) -> Size {
    if pixels.0 as u32 > START_WIDTH { Size::Width(START_WIDTH) } else { Size::Natural }
}

/// The size `size` gives an image of `pixels`, before narrowing.
pub fn css_size(size: Size, pixels: (i32, i32)) -> (i32, i32) {
    let (pw, ph) = (pixels.0.max(1) as i64, pixels.1.max(1) as i64);
    match size {
        Size::Natural => (pw as i32, ph as i32),
        Size::Width(w) => (w as i32, (w as i64 * ph / pw).max(1) as i32),
        Size::Stretched(w, h) => (w as i32, h as i32),
    }
}

/// [`css_size`] narrowed to `max` wide, keeping its proportions, as a
/// message is shown.
pub fn shown_size(size: Size, pixels: (i32, i32), max: i32) -> (i32, i32) {
    let (w, h) = css_size(size, pixels);
    if max > 0 && w > max { (max, ((h as i64 * max as i64) / w as i64).max(1) as i32) } else { (w.max(1), h.max(1)) }
}

/// Decodes an image with GTK's loader, turned upright by its EXIF
/// orientation, which GTK's JPEG loader ignores; None when it is not an
/// image GTK reads. Safe off the main thread.
pub fn decode(data: &glib::Bytes) -> Option<gdk::Texture> {
    let texture = gdk::Texture::from_bytes(data).ok()?;
    Some(match orientation(data) {
        1 => texture,
        o => orient(&texture, o),
    })
}

/// A JPEG's EXIF orientation, 1 to 8 (1 is upright); 1 without one.
pub fn orientation(data: &[u8]) -> u8 {
    let be16 = |b: &[u8]| u16::from_be_bytes([b[0], b[1]]);
    if !data.starts_with(&[0xFF, 0xD8]) {
        return 1;
    }
    let mut i = 2;
    while i + 4 <= data.len() && data[i] == 0xFF {
        let marker = data[i + 1];
        // Start of scan: the headers are over.
        if marker == 0xDA || marker == 0xD9 {
            break;
        }
        let len = be16(&data[i + 2..]) as usize;
        let Some(segment) = data.get(i + 4..i + 2 + len) else { break };
        if marker == 0xE1
            && let Some(tiff) = segment.strip_prefix(b"Exif\0\0")
        {
            return tiff_orientation(tiff).filter(|o| (1..=8).contains(o)).unwrap_or(1);
        }
        i += 2 + len;
    }
    1
}

/// The Orientation tag (0x0112) of a TIFF header's first directory.
fn tiff_orientation(tiff: &[u8]) -> Option<u8> {
    let little = match tiff.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |at: usize| -> Option<u16> {
        let b = tiff.get(at..at + 2)?;
        Some(if little { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) })
    };
    let u32_at = |at: usize| -> Option<u32> {
        let b = tiff.get(at..at + 4)?;
        let b = [b[0], b[1], b[2], b[3]];
        Some(if little { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    };
    let ifd = u32_at(4)? as usize;
    let count = u16_at(ifd)? as usize;
    (0..count).find_map(|n| {
        let entry = ifd + 2 + n * 12;
        (u16_at(entry)? == 0x0112).then(|| u16_at(entry + 8).map(|v| v as u8)).flatten()
    })
}

/// Premultiplied RGBA rows of `texture`, tightly packed.
fn pixels(texture: &gdk::Texture) -> Vec<u8> {
    let mut down = gdk::TextureDownloader::new(texture);
    down.set_format(gdk::MemoryFormat::R8g8b8a8Premultiplied);
    let (bytes, stride) = down.download_bytes();
    let row = texture.width() as usize * 4;
    if stride == row {
        return bytes.to_vec();
    }
    (0..texture.height() as usize).flat_map(|y| bytes[y * stride..y * stride + row].iter().copied()).collect()
}

fn texture_from(pixels: Vec<u8>, w: usize, h: usize) -> gdk::Texture {
    gdk::MemoryTexture::new(
        w as i32,
        h as i32,
        gdk::MemoryFormat::R8g8b8a8Premultiplied,
        &glib::Bytes::from_owned(pixels),
        w * 4,
    )
    .upcast()
}

/// How the user turned an image: mirrored left to right first, when
/// `flipped`, then `quarters` quarter turns clockwise.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Turn {
    pub quarters: u8,
    pub flipped: bool,
}

impl Turn {
    /// A quarter turn clockwise more.
    pub fn rotated(self) -> Turn {
        Turn { quarters: (self.quarters + 1) % 4, ..self }
    }

    /// The turned image mirrored left to right: the same as mirroring
    /// first and turning the other way.
    pub fn flipped(self) -> Turn {
        Turn { quarters: (4 - self.quarters) % 4, flipped: !self.flipped }
    }

    pub fn is_none(self) -> bool {
        self == Turn::default()
    }

    /// The EXIF orientation that does the same.
    fn exif(self) -> u8 {
        match (self.flipped, self.quarters % 4) {
            (false, 0) => 1,
            (false, 1) => 6,
            (false, 2) => 3,
            (false, _) => 8,
            (true, 0) => 2,
            (true, 1) => 7,
            (true, 2) => 4,
            (true, _) => 5,
        }
    }
}

/// `texture` turned by `turn`. Safe off the main thread.
pub fn turned(texture: &gdk::Texture, turn: Turn) -> gdk::Texture {
    if turn.is_none() { texture.clone() } else { orient(texture, turn.exif()) }
}

/// `texture` turned as EXIF orientation `o` says.
fn orient(texture: &gdk::Texture, o: u8) -> gdk::Texture {
    let (sw, sh) = (texture.width() as usize, texture.height() as usize);
    let src = pixels(texture);
    let (dw, dh) = if o >= 5 { (sh, sw) } else { (sw, sh) };
    let mut out = vec![0u8; dw * dh * 4];
    for y in 0..dh {
        for x in 0..dw {
            // Where in the stored image the upright pixel (x, y) is.
            let (sx, sy) = match o {
                2 => (sw - 1 - x, y),
                3 => (sw - 1 - x, sh - 1 - y),
                4 => (x, sh - 1 - y),
                5 => (y, x),
                6 => (y, sh - 1 - x),
                7 => (sw - 1 - y, sh - 1 - x),
                8 => (sw - 1 - y, x),
                _ => (x, y),
            };
            let (d, s) = ((y * dw + x) * 4, (sy * sw + sx) * 4);
            out[d..d + 4].copy_from_slice(&src[s..s + 4]);
        }
    }
    texture_from(out, dw, dh)
}

/// Premultiplied RGBA pixels as the type `mime` names: JPEG for JPEG
/// sources, PNG for everything else.
fn encode(pixels: Vec<u8>, w: usize, h: usize, mime: &str) -> (glib::Bytes, String) {
    if mime == "image/jpeg" {
        // Over white: JPEG has no transparency.
        let rgb: Vec<u8> = pixels.as_chunks::<4>().0.iter().flat_map(|p| [0, 1, 2].map(|c| p[c].saturating_add(255 - p[3]))).collect();
        let mut out = Vec::new();
        let encoded = jpeg_encoder::Encoder::new(&mut out, JPEG_QUALITY).encode(&rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb);
        if encoded.is_ok() {
            return (glib::Bytes::from_owned(out), mime.to_string());
        }
    }
    (texture_from(pixels, w, h).save_to_png_bytes(), "image/png".to_string())
}

/// Photo quality that is hard to tell from the original at a fraction of
/// its size.
const JPEG_QUALITY: u8 = 85;

/// `texture` scaled down to `w` by `h`, each output pixel the average of
/// the pixels it covers.
fn shrink(texture: &gdk::Texture, w: usize, h: usize) -> Vec<u8> {
    let (sw, sh) = (texture.width() as usize, texture.height() as usize);
    let src = pixels(texture);
    let span = |i: usize, n: usize, of: usize| {
        let a = i * of / n;
        (a, ((i + 1) * of / n).max(a + 1))
    };
    let xs: Vec<(usize, usize)> = (0..w).map(|x| span(x, w, sw)).collect();
    let mut out = vec![0u8; w * h * 4];
    let mut sum = vec![0u64; w * 4];
    for y in 0..h {
        let (y0, y1) = span(y, h, sh);
        sum.fill(0);
        for sy in y0..y1 {
            let row = &src[sy * sw * 4..(sy + 1) * sw * 4];
            for (x, (x0, x1)) in xs.iter().enumerate() {
                for p in row[x0 * 4..x1 * 4].as_chunks::<4>().0 {
                    for c in 0..4 {
                        sum[x * 4 + c] += p[c] as u64;
                    }
                }
            }
        }
        for (x, (x0, x1)) in xs.iter().enumerate() {
            let n = ((x1 - x0) * (y1 - y0)) as u64;
            for c in 0..4 {
                out[(y * w + x) * 4 + c] = ((sum[x * 4 + c] + n / 2) / n) as u8;
            }
        }
    }
    out
}

/// The pixels to send for an image of `pixels` shown at `size`: twice its
/// shown size, for high-density screens; None for the image's own size.
pub fn sent_pixels(size: Size, pixels: (i32, i32)) -> Option<(u32, u32)> {
    match size {
        Size::Natural => None,
        _ => {
            let (w, h) = css_size(size, pixels);
            Some((w.max(1) as u32 * 2, h.max(1) as u32 * 2))
        }
    }
}

/// The bytes to send for `texture` at `target` pixels, never larger.
/// `data`, of type `mime`, is what `texture` was decoded from, sent as it
/// is when the image is neither shrunk nor `turned`. Safe off the main
/// thread.
pub fn prepare(
    texture: &gdk::Texture,
    data: &glib::Bytes,
    mime: &str,
    target: Option<(u32, u32)>,
    turned: bool,
) -> (glib::Bytes, String) {
    let (tw, th) = (texture.width() as u32, texture.height() as u32);
    let (w, h) = target.map_or((tw, th), |(w, h)| (w.min(tw), h.min(th)));
    if (w, h) == (tw, th) {
        if !turned {
            return (data.clone(), mime.to_string());
        }
        return encode(pixels(texture), w as usize, h as usize, mime);
    }
    encode(shrink(texture, w as usize, h as usize), w as usize, h as usize, mime)
}

/// The part of `texture` inside `rect` (x, y, width, height in pixels),
/// encoded as [`encode`] does for `mime`. Safe off the main thread.
pub fn crop(texture: &gdk::Texture, rect: (i32, i32, i32, i32), mime: &str) -> (glib::Bytes, String) {
    let (x, y, w, h) = (rect.0 as usize, rect.1 as usize, rect.2 as usize, rect.3 as usize);
    let sw = texture.width() as usize;
    let src = pixels(texture);
    let mut out = Vec::with_capacity(w * h * 4);
    for row in y..y + h {
        let start = (row * sw + x) * 4;
        out.extend_from_slice(&src[start..start + w * 4]);
    }
    encode(out, w, h, mime)
}

/// `name` with a `.png` extension in place of its own.
pub fn png_name(name: &str) -> String {
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    format!("{stem}.png")
}

/// Asks for a width and a height in pixels, starting from `current`;
/// None when cancelled. With `ratio` (width over height), changing either
/// changes the other to keep it.
async fn size_dialog(
    parent: &impl IsA<gtk::Widget>,
    current: (i32, i32),
    ratio: Option<f64>,
    heading: &str,
    body: &str,
    action: &str,
) -> Option<(u32, u32)> {
    let row = |title: &str, value: i32| {
        adw::SpinRow::builder()
            .title(title)
            .adjustment(&gtk::Adjustment::new(value as f64, 1.0, 4000.0, 1.0, 10.0, 0.0))
            .build()
    };
    let (width, height) = (row("Width", current.0), row("Height", current.1));
    if let Some(ratio) = ratio.filter(|r| *r > 0.0) {
        // Each row's change sets the other, which would set it back.
        let busy = Rc::new(Cell::new(false));
        let (h, b) = (height.clone(), busy.clone());
        width.connect_value_notify(move |w| {
            if !b.replace(true) {
                h.set_value((w.value() / ratio).round().max(1.0));
                b.set(false);
            }
        });
        let (w, b) = (width.clone(), busy);
        height.connect_value_notify(move |h| {
            if !b.replace(true) {
                w.set_value((h.value() * ratio).round().max(1.0));
                b.set(false);
            }
        });
    }
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    list.append(&width);
    list.append(&height);
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.set_extra_child(Some(&list));
    dialog.add_responses(&[("cancel", "_Cancel"), ("apply", action)]);
    dialog.set_response_appearance("apply", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("apply"));
    dialog.set_close_response("cancel");
    (dialog.choose_future(Some(parent)).await == "apply").then(|| (width.value() as u32, height.value() as u32))
}

/// Asks for a new width and height with the image's proportions; None
/// when cancelled.
pub async fn scale_dialog(parent: &impl IsA<gtk::Widget>, current: (i32, i32), pixels: (i32, i32)) -> Option<(u32, u32)> {
    let ratio = pixels.0.max(1) as f64 / pixels.1.max(1) as f64;
    let body = format!("The image is shown at this size, in pixels. Its full size is {} × {}.", pixels.0, pixels.1);
    size_dialog(parent, current, Some(ratio), "Scale image", &body, "_Scale").await
}

/// Asks for any width and height; None when cancelled.
pub async fn stretch_dialog(parent: &impl IsA<gtk::Widget>, current: (i32, i32)) -> Option<(u32, u32)> {
    size_dialog(parent, current, None, "Stretch image", "The image is shown at exactly this size, in pixels.", "_Stretch").await
}

/// Which edges a drag in the crop dialog moves; all four move the box.
#[derive(Clone, Copy, Default)]
struct Grip {
    left: bool,
    right: bool,
    top: bool,
    bottom: bool,
}

/// Lets the user drag the edges of a box over `texture`; the box in pixels
/// (x, y, width, height), or None when cancelled or left whole.
pub async fn crop_dialog(parent: &impl IsA<gtk::Widget>, texture: &gdk::Texture) -> Option<(i32, i32, i32, i32)> {
    let (pw, ph) = (texture.width() as f64, texture.height() as f64);
    // The box in image pixels: left, top, right, bottom.
    let rect = Rc::new(Cell::new((0.0, 0.0, pw, ph)));
    let picture = gtk::Picture::for_paintable(texture);
    picture.set_content_fit(gtk::ContentFit::Contain);
    picture.set_can_shrink(true);
    let area = gtk::DrawingArea::new();
    // The picture sits inside a margin so the box's handles at its edges
    // are drawn whole.
    const INSET: i32 = 16;
    picture.set_margin_top(INSET);
    picture.set_margin_bottom(INSET);
    picture.set_margin_start(INSET);
    picture.set_margin_end(INSET);
    let overlay = gtk::Overlay::builder().child(&picture).build();
    overlay.add_overlay(&area);
    // Where the picture draws the image in the area: its offset and scale,
    // as ContentFit::Contain places it.
    let fit = move |aw: f64, ah: f64| {
        let inset = INSET as f64;
        let (aw, ah) = (aw - 2.0 * inset, ah - 2.0 * inset);
        let s = (aw / pw).min(ah / ph);
        (inset + (aw - pw * s) / 2.0, inset + (ah - ph * s) / 2.0, s)
    };
    let r = rect.clone();
    area.set_draw_func(move |_, cr, aw, ah| {
        let (ox, oy, s) = fit(aw as f64, ah as f64);
        let (l, t, rr, b) = r.get();
        let (x0, y0, x1, y1) = (ox + l * s, oy + t * s, ox + rr * s, oy + b * s);
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.55);
        cr.set_fill_rule(gtk::cairo::FillRule::EvenOdd);
        cr.rectangle(ox, oy, pw * s, ph * s);
        cr.rectangle(x0, y0, x1 - x0, y1 - y0);
        let _ = cr.fill();
        // White over a dark edge, visible on light and dark pictures.
        for (width, shade) in [(4.0, 0.0), (2.0, 1.0)] {
            cr.set_source_rgb(shade, shade, shade);
            cr.set_line_width(width);
            cr.rectangle(x0, y0, x1 - x0, y1 - y0);
            let _ = cr.stroke();
        }
        for (cx, cy) in [(x0, y0), (x1, y0), (x0, y1), (x1, y1)] {
            cr.set_source_rgb(0.0, 0.0, 0.0);
            cr.rectangle(cx - 7.0, cy - 7.0, 14.0, 14.0);
            let _ = cr.fill();
            cr.set_source_rgb(1.0, 1.0, 1.0);
            cr.rectangle(cx - 6.0, cy - 6.0, 12.0, 12.0);
            let _ = cr.fill();
        }
    });
    let drag = gtk::GestureDrag::new();
    let grip = Rc::new(Cell::new(None::<(Grip, (f64, f64, f64, f64))>));
    let (r, g, a) = (rect.clone(), grip.clone(), area.clone());
    drag.connect_drag_begin(move |_, x, y| {
        let (ox, oy, s) = fit(a.width() as f64, a.height() as f64);
        let (l, t, rr, b) = r.get();
        let (x0, y0, x1, y1) = (ox + l * s, oy + t * s, ox + rr * s, oy + b * s);
        // Touch needs a wider reach than a pointer; 16 px serves both.
        let near = 16.0;
        let within_y = y > y0 - near && y < y1 + near;
        let within_x = x > x0 - near && x < x1 + near;
        let mut grip_now = Grip {
            left: within_y && (x - x0).abs() < near,
            right: within_y && (x - x1).abs() < near,
            top: within_x && (y - y0).abs() < near,
            bottom: within_x && (y - y1).abs() < near,
        };
        // A small box: the nearer of two opposite edges wins.
        if grip_now.left && grip_now.right {
            grip_now.left = (x - x0).abs() <= (x - x1).abs();
            grip_now.right = !grip_now.left;
        }
        if grip_now.top && grip_now.bottom {
            grip_now.top = (y - y0).abs() <= (y - y1).abs();
            grip_now.bottom = !grip_now.top;
        }
        let any = grip_now.left || grip_now.right || grip_now.top || grip_now.bottom;
        if !any && x > x0 && x < x1 && y > y0 && y < y1 {
            grip_now = Grip { left: true, right: true, top: true, bottom: true };
        } else if !any {
            g.set(None);
            return;
        }
        g.set(Some((grip_now, r.get())));
    });
    let (r, g, a) = (rect.clone(), grip.clone(), area.clone());
    drag.connect_drag_update(move |_, dx, dy| {
        let Some((grip, (l, t, rr, b))) = g.get() else { return };
        let (_, _, s) = fit(a.width() as f64, a.height() as f64);
        let (dx, dy) = (dx / s, dy / s);
        let min = 8.0_f64.min(pw).min(ph);
        let moving = grip.left && grip.right && grip.top && grip.bottom;
        let next = if moving {
            let dx = dx.clamp(-l, pw - rr);
            let dy = dy.clamp(-t, ph - b);
            (l + dx, t + dy, rr + dx, b + dy)
        } else {
            (
                if grip.left { (l + dx).clamp(0.0, rr - min) } else { l },
                if grip.top { (t + dy).clamp(0.0, b - min) } else { t },
                if grip.right { (rr + dx).clamp(l + min, pw) } else { rr },
                if grip.bottom { (b + dy).clamp(t + min, ph) } else { b },
            )
        };
        r.set(next);
        a.queue_draw();
    });
    area.add_controller(drag);

    let cancel = gtk::Button::with_mnemonic("_Cancel");
    let crop = gtk::Button::with_mnemonic("C_rop");
    crop.add_css_class("suggested-action");
    let header = adw::HeaderBar::builder().show_start_title_buttons(false).show_end_title_buttons(false).build();
    header.pack_start(&cancel);
    header.pack_end(&crop);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&overlay));
    let dialog = adw::Dialog::builder().title("Crop image").content_width(640).content_height(560).child(&view).build();
    let (tx, rx) = futures_channel::oneshot::channel::<bool>();
    let tx = Rc::new(RefCell::new(Some(tx)));
    let finish = {
        let (tx, dialog) = (tx.clone(), dialog.clone());
        move |ok: bool| {
            if let Some(tx) = tx.borrow_mut().take() {
                let _ = tx.send(ok);
            }
            dialog.close();
        }
    };
    let f = finish.clone();
    cancel.connect_clicked(move |_| f(false));
    crop.connect_clicked(move |_| finish(true));
    dialog.connect_closed(move |_| {
        if let Some(tx) = tx.borrow_mut().take() {
            let _ = tx.send(false);
        }
    });
    dialog.present(Some(parent));
    if !rx.await.unwrap_or(false) {
        return None;
    }
    let (l, t, rr, b) = rect.get();
    let (x, y) = (l.round() as i32, t.round() as i32);
    let (w, h) = ((rr.round() as i32 - x).max(1), (b.round() as i32 - y).max(1));
    let whole = x == 0 && y == 0 && w == pw as i32 && h == ph as i32;
    (!whole).then_some((x, y, w, h))
}

/// The widget shown at an image's place in the editor: the picture with a
/// menu button over its corner. The button, a right click and a long press
/// call `on_menu` with the widget the menu points at and, for a press, the
/// point in it.
pub fn widget(paintable: &SizedImage, on_menu: impl Fn(&gtk::Widget, Option<(f64, f64)>) + 'static) -> gtk::Overlay {
    let picture = gtk::Picture::for_paintable(paintable);
    picture.set_can_shrink(false);
    let button = gtk::Button::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("Image options")
        .halign(gtk::Align::End)
        .valign(gtk::Align::Start)
        .margin_top(6)
        .margin_end(6)
        .focus_on_click(false)
        .build();
    // Readable over any picture, light or dark.
    button.add_css_class("osd");
    button.add_css_class("circular");
    let on_menu = Rc::new(on_menu);
    // The text view claims every press inside it, which cancels the
    // button's own click; this claims the press first.
    let click = gtk::GestureClick::new();
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    click.connect_pressed(|g, _, _, _| {
        g.set_state(gtk::EventSequenceState::Claimed);
    });
    let (b, f) = (button.downgrade(), on_menu.clone());
    click.connect_released(move |_, _, _, _| {
        if let Some(b) = b.upgrade() {
            f(b.upcast_ref(), None);
        }
    });
    button.add_controller(click);
    let f = on_menu.clone();
    button.connect_clicked(move |b| f(b.upcast_ref(), None));
    let overlay = gtk::Overlay::builder().child(&picture).build();
    overlay.add_overlay(&button);
    overlay.set_cursor_from_name(Some("default"));
    // Claimed in the capture phase for the same reason as the button's.
    let secondary = gtk::GestureClick::builder().button(gtk::gdk::BUTTON_SECONDARY).build();
    secondary.set_propagation_phase(gtk::PropagationPhase::Capture);
    let (o, f) = (overlay.downgrade(), on_menu.clone());
    secondary.connect_pressed(move |g, _, x, y| {
        g.set_state(gtk::EventSequenceState::Claimed);
        if let Some(o) = o.upgrade() {
            f(o.upcast_ref(), Some((x, y)));
        }
    });
    overlay.add_controller(secondary);
    // Touch only: a mouse has the right click.
    let long = gtk::GestureLongPress::builder().touch_only(true).build();
    long.set_propagation_phase(gtk::PropagationPhase::Capture);
    let o = overlay.downgrade();
    long.connect_pressed(move |g, x, y| {
        g.set_state(gtk::EventSequenceState::Claimed);
        if let Some(o) = o.upgrade() {
            on_menu(o.upcast_ref(), Some((x, y)));
        }
    });
    overlay.add_controller(long);
    overlay
}

/// Where a point given in `from`'s coordinates falls in `view`'s text, when
/// it falls inside the view.
pub fn iter_at_point(from: &impl IsA<gtk::Widget>, view: &impl IsA<gtk::TextView>, x: f64, y: f64) -> Option<gtk::TextIter> {
    let v: &gtk::TextView = view.as_ref();
    let p = from.compute_point(v, &graphene::Point::new(x as f32, y as f32))?;
    let (px, py) = (p.x() as f64, p.y() as f64);
    if px < 0.0 || py < 0.0 || px > v.width() as f64 || py > v.height() as f64 {
        return None;
    }
    let (bx, by) = v.window_to_buffer_coords(gtk::TextWindowType::Widget, px as i32, py as i32);
    v.iter_at_location(bx, by)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(start_size((800, 400)), Size::Width(600));
        assert_eq!(start_size((300, 200)), Size::Natural);
        assert_eq!(css_size(Size::Width(400), (800, 600)), (400, 300));
        assert_eq!(shown_size(Size::Natural, (800, 600), 400), (400, 300));
        assert_eq!(shown_size(Size::Stretched(500, 100), (10, 10), 250), (250, 50));
        assert_eq!(shown_size(Size::Width(200), (800, 600), 400), (200, 150));
        assert_eq!(png_name("cat.jpeg"), "cat.png");
        assert_eq!(sent_pixels(Size::Width(300), (1200, 800)), Some((600, 400)));
        assert_eq!(sent_pixels(Size::Natural, (1200, 800)), None);
        let t = Turn::default();
        assert_eq!(t.rotated().rotated().rotated().rotated(), t);
        assert_eq!(t.flipped().flipped(), t);
        // Mirroring after a quarter turn equals mirroring first, then three
        // quarter turns: EXIF 5, the transpose.
        assert_eq!(t.rotated().flipped().exif(), 5);
        assert_eq!(t.flipped().rotated().exif(), 7);
        assert_eq!(png_name("cat"), "cat.png");
    }

    #[test]
    fn exif_orientation() {
        // An APP1 segment holding a little-endian TIFF with one entry:
        // Orientation (0x0112), SHORT, 1 value, 6.
        let mut tiff = b"II*\0\x08\0\0\0\x01\0".to_vec();
        tiff.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0]);
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend_from_slice(&tiff);
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1];
        jpeg.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(&app1);
        jpeg.extend_from_slice(&[0xFF, 0xDA, 0, 2]);
        assert_eq!(orientation(&jpeg), 6);
        assert_eq!(orientation(&[0xFF, 0xD8, 0xFF, 0xDA, 0, 2]), 1);
        assert_eq!(orientation(b"\x89PNG"), 1);
    }
}
