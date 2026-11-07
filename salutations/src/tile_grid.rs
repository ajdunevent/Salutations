//! Tiles laid out in rows, wrapping at the width given. Collapsed, only the
//! first two rows are shown; the owner learns how many rows there are and
//! how many tiles are hidden after each layout, to offer Show All.

use std::cell::{Cell, RefCell};

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

const SPACING: i32 = 6;
const COLLAPSED_ROWS: usize = 2;

type Overflow = Box<dyn Fn(usize, usize)>;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct TileGrid {
        pub tiles: RefCell<Vec<gtk::Widget>>,
        pub expanded: Cell<bool>,
        /// The last tile takes the rest of its row (a text entry after
        /// recipient pills); it starts a new row below this width.
        pub stretch_last: Cell<Option<i32>>,
        /// Rows and hidden tiles last reported.
        pub reported: Cell<(usize, usize)>,
        pub on_overflow: RefCell<Option<Overflow>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TileGrid {
        const NAME: &'static str = "SalutationsTileGrid";
        type Type = super::TileGrid;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for TileGrid {
        fn dispose(&self) {
            for t in self.tiles.take() {
                t.unparent();
            }
        }
    }

    impl TileGrid {
        /// Each tile's row and width at `width`, and the row height.
        pub fn layout(&self, width: i32) -> (Vec<(usize, i32)>, i32) {
            let mut out = Vec::new();
            let (mut row, mut x, mut height) = (0usize, 0i32, 0i32);
            let tiles = self.tiles.borrow();
            let last = tiles.len().saturating_sub(1);
            for (i, t) in tiles.iter().enumerate() {
                let (min, nat, _, _) = t.measure(gtk::Orientation::Horizontal, -1);
                let mut w = if width > 0 { nat.min(width).max(min) } else { nat };
                if let Some(least) = self.stretch_last.get().filter(|_| i == last && width > 0) {
                    let rest = width - x;
                    w = if rest >= least.max(min) { rest } else { width };
                }
                if x > 0 && width > 0 && x + w > width {
                    row += 1;
                    x = 0;
                }
                out.push((row, w));
                x += w + SPACING;
                height = height.max(t.measure(gtk::Orientation::Vertical, w).1);
            }
            (out, height)
        }

        fn limit(&self) -> usize {
            if self.expanded.get() { usize::MAX } else { COLLAPSED_ROWS }
        }
    }

    impl WidgetImpl for TileGrid {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }

        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let tiles = self.tiles.borrow();
            if orientation == gtk::Orientation::Horizontal {
                let min = tiles.iter().map(|t| t.measure(orientation, -1).0).max().unwrap_or(0);
                let nat: i32 = tiles.iter().map(|t| t.measure(orientation, -1).1 + SPACING).sum::<i32>() - SPACING;
                return (min, nat.max(min), -1, -1);
            }
            drop(tiles);
            let (rows, height) = self.layout(for_size);
            let n = rows.last().map_or(0, |(r, _)| r + 1).min(self.limit()) as i32;
            let h = if n == 0 { 0 } else { n * height + (n - 1) * SPACING };
            (h, h, -1, -1)
        }

        fn size_allocate(&self, width: i32, given: i32, _baseline: i32) {
            let (rows, height) = self.layout(width);
            let limit = self.limit();
            // Rows sit in the middle of a taller allocation (a field row).
            let shown = rows.last().map_or(0, |(r, _)| r + 1).min(limit) as i32;
            let top = ((given - (shown * height + (shown - 1).max(0) * SPACING)) / 2).max(0);
            let mut hidden = 0;
            let (mut x, mut current) = (0, 0);
            for (t, (row, w)) in self.tiles.borrow().iter().zip(&rows) {
                if *row != current {
                    current = *row;
                    x = 0;
                }
                let show = *row < limit;
                t.set_child_visible(show);
                if show {
                    let y = top + *row as i32 * (height + SPACING);
                    t.size_allocate(&gtk::Allocation::new(x, y, *w, height), -1);
                } else {
                    hidden += 1;
                }
                x += w + SPACING;
            }
            let total = rows.last().map_or(0, |(r, _)| r + 1);
            if self.reported.replace((total, hidden)) != (total, hidden) {
                // The owner changes other widgets, which cannot happen
                // during this allocation.
                let obj = self.obj().downgrade();
                glib::idle_add_local_once(move || {
                    if let Some(o) = obj.upgrade() {
                        let (total, hidden) = o.imp().reported.get();
                        if let Some(f) = o.imp().on_overflow.borrow().as_ref() {
                            f(total, hidden);
                        }
                    }
                });
            }
        }
    }
}

glib::wrapper! {
    pub struct TileGrid(ObjectSubclass<imp::TileGrid>) @extends gtk::Widget, @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for TileGrid {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl TileGrid {
    pub fn set_tiles(&self, tiles: Vec<gtk::Widget>) {
        // Tiles kept from before stay parented, so a focused one keeps focus.
        for t in self.imp().tiles.take() {
            if !tiles.contains(&t) {
                t.unparent();
            }
        }
        for t in tiles.iter().rev() {
            if t.parent().as_ref() != Some(self.upcast_ref()) {
                t.set_parent(self);
            }
            // Child order sets focus and drawing order.
            t.insert_before(self, self.first_child().as_ref());
        }
        *self.imp().tiles.borrow_mut() = tiles;
        self.queue_resize();
    }

    /// See `stretch_last`; also never collapses.
    pub fn set_stretch_last(&self, least: i32) {
        self.imp().stretch_last.set(Some(least));
        self.imp().expanded.set(true);
    }

    pub fn set_expanded(&self, on: bool) {
        self.imp().expanded.set(on);
        self.queue_resize();
    }

    pub fn is_expanded(&self) -> bool {
        self.imp().expanded.get()
    }

    /// Called with the number of rows and of hidden tiles when they change.
    pub fn connect_overflow(&self, f: impl Fn(usize, usize) + 'static) {
        *self.imp().on_overflow.borrow_mut() = Some(Box::new(f));
    }
}

/// A tile's type icon and its name and size, for a file of `mime` named
/// `name`. Full-color icons: themes rarely have symbolic ones for every type.
pub fn file_label(name: &str, size: Option<&str>, mime: &str) -> gtk::Box {
    use gtk::gio;
    // Saved drafts and some senders label every file octet-stream.
    let kind = match gio::content_type_from_mime_type(mime) {
        Some(k) if !gio::content_type_is_unknown(&k) && mime != "application/octet-stream" => k,
        _ => gio::content_type_guess(Some(name), None).0,
    };
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let icon = gtk::Image::from_gicon(&gio::content_type_get_icon(&kind));
    icon.set_pixel_size(32);
    b.append(&icon);
    let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
    text.set_valign(gtk::Align::Center);
    text.append(
        &gtk::Label::builder()
            .label(name)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .max_width_chars(22)
            .tooltip_text(name)
            .build(),
    );
    if let Some(size) = size {
        text.append(&gtk::Label::builder().label(size).xalign(0.0).css_classes(["size", "dimmed"]).build());
    }
    b.append(&text);
    b
}
