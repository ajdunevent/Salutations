//! A message row with its hover buttons overlaid. While they show, the row
//! under them is hidden and the text running up to them fades out, so a
//! long sender name does not show through the buttons.

use std::cell::{Cell, RefCell};

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};

/// How far before the buttons the fade starts.
const FADE: f32 = 32.0;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct RowHolder {
        /// The row, first among the children; the buttons come after it.
        pub main: RefCell<Option<gtk::Widget>>,
        /// The buttons' width and height while they show.
        pub covered: Cell<Option<(f32, f32)>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RowHolder {
        const NAME: &'static str = "SalutationsRowHolder";
        type Type = super::RowHolder;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_layout_manager_type::<gtk::BinLayout>();
        }
    }

    impl ObjectImpl for RowHolder {
        fn dispose(&self) {
            while let Some(c) = self.obj().first_child() {
                c.unparent();
            }
        }
    }

    impl WidgetImpl for RowHolder {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let (Some((bw, bh)), Some(main)) = (self.covered.get(), self.main.borrow().clone()) else {
                return self.parent_snapshot(snapshot);
            };
            let (w, h) = (obj.width() as f32, obj.height() as f32);
            let start = (w - bw - FADE).max(0.0);
            let opaque = gdk::RGBA::BLACK;
            let clear = gdk::RGBA::TRANSPARENT;
            // The mask: everything below the buttons' band, and that band up
            // to where the fade starts; the fade; nothing under the buttons.
            snapshot.push_mask(gsk::MaskMode::Alpha);
            snapshot.append_color(&opaque, &graphene::Rect::new(0.0, bh, w, (h - bh).max(0.0)));
            snapshot.append_color(&opaque, &graphene::Rect::new(0.0, 0.0, start, bh));
            snapshot.append_linear_gradient(
                &graphene::Rect::new(start, 0.0, FADE, bh),
                &graphene::Point::new(start, 0.0),
                &graphene::Point::new(start + FADE, 0.0),
                &[gsk::ColorStop::new(0.0, opaque), gsk::ColorStop::new(1.0, clear)],
            );
            snapshot.pop();
            obj.snapshot_child(&main, snapshot);
            snapshot.pop();
            let mut c = obj.first_child();
            while let Some(child) = c {
                if child != main {
                    obj.snapshot_child(&child, snapshot);
                }
                c = child.next_sibling();
            }
        }
    }
}

glib::wrapper! {
    pub struct RowHolder(ObjectSubclass<imp::RowHolder>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl RowHolder {
    pub fn new() -> Self {
        glib::Object::new()
    }

    /// Replaces the row.
    pub fn set_child(&self, child: Option<&impl IsA<gtk::Widget>>) {
        if let Some(old) = self.imp().main.take() {
            old.unparent();
        }
        if let Some(c) = child {
            c.insert_after(self, None::<&gtk::Widget>);
            *self.imp().main.borrow_mut() = Some(c.clone().upcast());
        }
    }

    /// Adds a widget over the row: the buttons.
    pub fn add_overlay(&self, w: &impl IsA<gtk::Widget>) {
        w.insert_before(self, None::<&gtk::Widget>);
    }

    /// The buttons' size while they show; None when they are hidden.
    pub fn set_covered(&self, covered: Option<(f32, f32)>) {
        if self.imp().covered.replace(covered) != covered {
            self.queue_draw();
        }
    }
}
