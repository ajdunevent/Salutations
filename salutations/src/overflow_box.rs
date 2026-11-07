//! A row of buttons that moves the last ones into a ⋮ menu when space
//! runs short, rightmost first. Hidden buttons take no part; each button
//! brings the menu item that stands in for it.

use std::cell::RefCell;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gio, glib};

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct OverflowBox {
        /// In order, with the menu items that stand in for each.
        pub items: RefCell<Vec<(gtk::Widget, gio::MenuModel)>>,
        pub more: RefCell<Option<gtk::MenuButton>>,
        /// The items in the menu at the last allocation, by index.
        pub overflowed: RefCell<Vec<usize>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for OverflowBox {
        const NAME: &'static str = "SalutationsOverflowBox";
        type Type = super::OverflowBox;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for OverflowBox {
        fn dispose(&self) {
            for (w, _) in self.items.take() {
                w.unparent();
            }
            if let Some(m) = self.more.take() {
                m.unparent();
            }
        }
    }

    impl OverflowBox {
        fn visible(&self) -> Vec<(usize, gtk::Widget)> {
            self.items.borrow().iter().enumerate().filter(|(_, (w, _))| w.is_visible()).map(|(i, (w, _))| (i, w.clone())).collect()
        }

        fn more(&self) -> gtk::MenuButton {
            self.more.borrow().clone().expect("built")
        }
    }

    impl WidgetImpl for OverflowBox {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let visible = self.visible();
            let more = self.more().measure(orientation, -1);
            if orientation == gtk::Orientation::Vertical {
                let h = visible.iter().map(|(_, w)| w.measure(orientation, -1).1).max().unwrap_or(0).max(more.1);
                return (h, h, -1, -1);
            }
            if visible.is_empty() {
                return (0, 0, -1, -1);
            }
            let nat: i32 = visible.iter().map(|(_, w)| w.measure(orientation, -1).1).sum();
            (more.1.min(nat), nat, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            let visible = self.visible();
            let more = self.more();
            let widths: Vec<i32> = visible.iter().map(|(_, w)| w.measure(gtk::Orientation::Horizontal, -1).1).collect();
            let total: i32 = widths.iter().sum();
            // Everything fits, or as many as fit beside the ⋮ button.
            let room = if total <= width { width } else { width - more.measure(gtk::Orientation::Horizontal, -1).1 };
            let mut x = 0;
            let mut overflowed = Vec::new();
            for ((i, w), wd) in visible.iter().zip(&widths) {
                if overflowed.is_empty() && x + wd <= room {
                    w.set_child_visible(true);
                    w.size_allocate(&gtk::Allocation::new(x, 0, *wd, height), -1);
                    x += wd;
                } else {
                    w.set_child_visible(false);
                    overflowed.push(*i);
                }
            }
            more.set_child_visible(!overflowed.is_empty());
            if !overflowed.is_empty() {
                let mw = more.measure(gtk::Orientation::Horizontal, -1).1;
                more.size_allocate(&gtk::Allocation::new(x, 0, mw, height), -1);
            }
            if *self.overflowed.borrow() != overflowed {
                let menu = gio::Menu::new();
                for i in &overflowed {
                    if let Some((_, m)) = self.items.borrow().get(*i) {
                        menu.append_section(None, m);
                    }
                }
                more.set_menu_model(Some(&menu));
                *self.overflowed.borrow_mut() = overflowed;
            }
        }
    }
}

glib::wrapper! {
    pub struct OverflowBox(ObjectSubclass<imp::OverflowBox>) @extends gtk::Widget, @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl OverflowBox {
    /// `items` in order, each with the menu items that stand in for it.
    pub fn new(items: Vec<(gtk::Widget, gio::MenuModel)>) -> Self {
        let this: Self = glib::Object::new();
        for (w, _) in &items {
            w.set_parent(&this);
        }
        let more = gtk::MenuButton::builder().icon_name("view-more-symbolic").tooltip_text("More actions").build();
        more.set_parent(&this);
        more.set_child_visible(false);
        *this.imp().more.borrow_mut() = Some(more);
        *this.imp().items.borrow_mut() = items;
        this
    }
}

/// A menu of one item.
pub fn item(label: &str, action: &str) -> gio::MenuModel {
    let m = gio::Menu::new();
    m.append(Some(label), Some(action));
    m.upcast()
}
