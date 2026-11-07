//! Shows the first of its children whose natural width fits, as a longer
//! and a shorter version of the same control.

use std::cell::RefCell;

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct FitStack {
        /// Widest first.
        pub children: RefCell<Vec<gtk::Widget>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FitStack {
        const NAME: &'static str = "SalutationsFitStack";
        type Type = super::FitStack;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for FitStack {
        fn dispose(&self) {
            for c in self.children.take() {
                c.unparent();
            }
        }
    }

    impl WidgetImpl for FitStack {
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let children = self.children.borrow();
            let (Some(first), Some(last)) = (children.first(), children.last()) else { return (0, 0, -1, -1) };
            if orientation == gtk::Orientation::Horizontal {
                return (last.measure(orientation, -1).0, first.measure(orientation, -1).1, -1, -1);
            }
            let h = children.iter().map(|c| c.measure(orientation, for_size.max(-1)).1).max().unwrap_or(0);
            (h, h, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            let children = self.children.borrow();
            let pick = children
                .iter()
                .position(|c| c.measure(gtk::Orientation::Horizontal, -1).1 <= width)
                .unwrap_or(children.len().saturating_sub(1));
            for (i, c) in children.iter().enumerate() {
                c.set_child_visible(i == pick);
                if i == pick {
                    c.allocate(width, height, baseline, None);
                }
            }
        }
    }
}

glib::wrapper! {
    pub struct FitStack(ObjectSubclass<imp::FitStack>) @extends gtk::Widget, @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl FitStack {
    /// `children` widest first.
    pub fn new(children: Vec<gtk::Widget>) -> Self {
        let this: Self = glib::Object::new();
        for c in &children {
            c.set_parent(&this);
        }
        *this.imp().children.borrow_mut() = children;
        this
    }
}
