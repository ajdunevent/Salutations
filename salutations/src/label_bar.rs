//! A bar of buttons that show their labels only while they fit, given in
//! order of importance: when space runs short the last label goes first.
//! Each button keeps its icon and tooltip either way.

use std::cell::{Cell, RefCell};

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct LabelBar {
        pub child: RefCell<Option<gtk::Widget>>,
        /// Most important first. A group is shown or hidden together, for
        /// one button drawn by several widgets.
        pub labels: RefCell<Vec<Vec<gtk::Label>>>,
        /// The child's widths with no labels (minimum) and with all of
        /// them (natural), from the last allocation; 0 before the first.
        pub bare: Cell<i32>,
        pub full: Cell<i32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LabelBar {
        const NAME: &'static str = "SalutationsLabelBar";
        type Type = super::LabelBar;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for LabelBar {
        fn dispose(&self) {
            if let Some(c) = self.child.take() {
                c.unparent();
            }
        }
    }

    impl LabelBar {
        fn show(&self, n: usize) {
            for (i, group) in self.labels.borrow().iter().enumerate() {
                for l in group {
                    l.set_visible(i < n);
                }
            }
        }

        fn shown(&self) -> usize {
            self.labels.borrow().iter().take_while(|g| g.iter().any(|l| l.is_visible())).count()
        }
    }

    impl WidgetImpl for LabelBar {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::ConstantSize
        }

        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let Some(c) = self.child.borrow().clone() else { return (0, 0, -1, -1) };
            // Labels change only in size_allocate: measuring here must not
            // toggle them, so the widths come from the last allocation.
            let (min, nat, _, _) = c.measure(orientation, -1);
            if orientation == gtk::Orientation::Vertical {
                return (min, nat, -1, -1);
            }
            let min = if self.shown() == 0 || self.bare.get() == 0 { min } else { self.bare.get() };
            (min, nat.max(self.full.get()).max(min), -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            let Some(c) = self.child.borrow().clone() else { return };
            let n = self.labels.borrow().len();
            let width_with = |k: usize| {
                self.show(k);
                c.measure(gtk::Orientation::Horizontal, -1)
            };
            let (bare, _, _, _) = width_with(0);
            let (_, full, _, _) = width_with(n);
            let mut k = n;
            if full > width {
                k = (0..n).rev().find(|&k| width_with(k).1 <= width).unwrap_or(0);
            }
            self.show(k);
            let changed = bare != self.bare.get() || full != self.full.get();
            self.bare.set(bare);
            self.full.set(full);
            c.allocate(width, height, baseline, None);
            if changed {
                // The first measurement happened before these were known.
                let obj = self.obj().downgrade();
                glib::idle_add_local_once(move || {
                    if let Some(o) = obj.upgrade() {
                        o.queue_resize();
                    }
                });
            }
        }
    }
}

glib::wrapper! {
    pub struct LabelBar(ObjectSubclass<imp::LabelBar>) @extends gtk::Widget, @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl LabelBar {
    /// `labels` are inside `child`'s buttons, most important first.
    pub fn new(child: &impl IsA<gtk::Widget>, labels: Vec<Vec<gtk::Label>>) -> Self {
        let this: Self = glib::Object::new();
        child.set_parent(&this);
        for l in labels.iter().flatten() {
            l.set_visible(false);
        }
        *this.imp().child.borrow_mut() = Some(child.clone().upcast());
        *this.imp().labels.borrow_mut() = labels;
        this
    }
}

/// A button's content: an icon, then a label the bar may hide. A menu
/// button's content also ends in a drop-down arrow.
pub fn content(icon: &str, text: &str, arrow: bool) -> (gtk::Box, gtk::Label) {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    b.set_halign(gtk::Align::Center);
    b.append(&gtk::Image::from_icon_name(icon));
    let label = gtk::Label::with_mnemonic(text);
    b.append(&label);
    if arrow {
        b.append(&gtk::Image::from_icon_name("pan-down-symbolic"));
    }
    (b, label)
}
