//! A recipient field: finished addresses become pills, followed by an entry
//! for the one being typed. An address is finished by `,` `;` or Enter, by
//! a space after a bare address, by choosing a contact, or when the field
//! loses focus. Tapping a pill puts its text back into the entry in its
//! place; a right click or long press opens its contact menu. Backspace in
//! an empty entry selects the last pill; a second Backspace removes it.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, glib};
use salutations_core::Address;
use salutations_core::compose;

use crate::tile_grid::TileGrid;

/// A finished entry: an address, or text that does not parse as one.
#[derive(Clone)]
enum Item {
    Good(Address),
    Bad(String),
}

impl Item {
    fn text(&self) -> String {
        match self {
            Item::Good(a) => compose::format_address(a),
            Item::Bad(t) => t.clone(),
        }
    }
}

pub struct RecipientField {
    pub widget: TileGrid,
    pub entry: gtk::Entry,
    items: RefCell<Vec<Item>>,
    /// Where the entry sits among the pills; the end except while a pill
    /// is being edited.
    at: Cell<usize>,
    /// The last pill, selected by a first Backspace.
    armed: Cell<bool>,
    /// Changes made by the field itself do not reach `on_changed` twice.
    quiet: Cell<bool>,
    on_changed: RefCell<Option<Rc<dyn Fn()>>>,
    contact_ctx: RefCell<Option<Rc<crate::contact_menu::Ctx>>>,
}

impl RecipientField {
    pub fn new() -> Rc<Self> {
        let widget = TileGrid::default();
        widget.set_stretch_last(120);
        widget.set_hexpand(true);
        widget.add_css_class("recipients");
        let entry = gtk::Entry::builder().hexpand(true).has_frame(false).build();
        entry.add_css_class("compose-entry");
        let this = Rc::new(Self {
            widget,
            entry,
            items: RefCell::new(Vec::new()),
            at: Cell::new(0),
            contact_ctx: RefCell::new(None),
            armed: Cell::new(false),
            quiet: Cell::new(false),
            on_changed: RefCell::new(None),
        });
        this.connect();
        this.rebuild();
        this
    }

    fn connect(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.entry.connect_changed(move |e| {
            let Some(this) = weak.upgrade() else { return };
            if this.quiet.get() {
                return;
            }
            this.disarm();
            let text = e.text().to_string();
            if text.contains([',', ';']) {
                // The finished parts become pills; what follows the last
                // separator stays in the entry.
                let (done, rest) = text.rsplit_once([',', ';']).expect("contains a separator");
                let (done, rest) = (done.to_string(), rest.trim_start().to_string());
                glib::idle_add_local_once(move || {
                    this.commit_text(&done.replace(';', ","));
                    this.set_entry(&rest);
                    this.changed();
                });
                return;
            }
            if text.ends_with(' ') && is_bare_address(text.trim()) {
                glib::idle_add_local_once(move || {
                    let t = this.entry.text().to_string();
                    this.commit_text(&t);
                    this.set_entry("");
                    this.changed();
                });
                return;
            }
            this.changed();
        });
        let weak = Rc::downgrade(self);
        self.entry.connect_activate(move |_| {
            if let Some(this) = weak.upgrade() {
                this.commit();
            }
        });
        let focus = gtk::EventControllerFocus::new();
        let weak = Rc::downgrade(self);
        focus.connect_leave(move |_| {
            let Some(this) = weak.upgrade() else { return };
            this.disarm();
            // After a completion click, which arrives after focus leaves.
            glib::timeout_add_local_once(std::time::Duration::from_millis(200), move || {
                if !this.entry.has_focus() {
                    this.commit();
                }
            });
        });
        self.entry.add_controller(focus);
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, _| {
            let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
            if key != gdk::Key::BackSpace || !this.entry.text().is_empty() || this.at.get() == 0 {
                this.disarm();
                return glib::Propagation::Proceed;
            }
            if this.armed.get() {
                let i = this.at.get() - 1;
                this.items.borrow_mut().remove(i);
                this.at.set(i);
                this.armed.set(false);
                this.rebuild();
                this.changed();
            } else {
                this.armed.set(true);
                this.rebuild();
            }
            glib::Propagation::Stop
        });
        self.entry.add_controller(keys);
    }

    fn disarm(self: &Rc<Self>) {
        if self.armed.replace(false) {
            self.rebuild();
        }
    }

    fn set_entry(&self, text: &str) {
        self.quiet.set(true);
        self.entry.set_text(text);
        self.entry.set_position(-1);
        self.quiet.set(false);
    }

    /// Turns the entry's text into pills.
    pub fn commit(self: &Rc<Self>) {
        let text = self.entry.text().to_string();
        if text.trim().is_empty() {
            if self.at.get() != self.items.borrow().len() {
                self.at.set(self.items.borrow().len());
                self.rebuild();
            }
            return;
        }
        self.commit_text(&text);
        self.set_entry("");
        self.changed();
    }

    /// Adds pills for `text` (comma-separated) where the entry is, and
    /// puts the entry back at the end.
    fn commit_text(self: &Rc<Self>, text: &str) {
        let new: Vec<Item> = split(text)
            .into_iter()
            .map(|t| match compose::parse_addresses(&t) {
                Ok(list) if list.len() == 1 => Item::Good(list.into_iter().next().expect("one")),
                _ => Item::Bad(t),
            })
            .collect();
        {
            let mut items = self.items.borrow_mut();
            let at = self.at.get().min(items.len());
            for (k, item) in new.into_iter().enumerate() {
                items.insert(at + k, item);
            }
            self.at.set(items.len());
        }
        self.rebuild();
    }

    /// A contact chosen from the completion list.
    pub fn accept(self: &Rc<Self>, text: &str) {
        self.set_entry("");
        self.commit_text(text);
        self.changed();
        self.entry.grab_focus();
    }

    fn changed(&self) {
        let cb = self.on_changed.borrow().clone();
        if let Some(cb) = cb {
            cb();
        }
    }

    pub fn connect_changed(&self, f: impl Fn() + 'static) {
        *self.on_changed.borrow_mut() = Some(Rc::new(f));
    }

    /// The pills and the text being typed, as a comma-separated field.
    pub fn text(&self) -> String {
        let mut parts: Vec<String> = self.items.borrow().iter().map(Item::text).collect();
        let typed = self.entry.text().trim().to_string();
        if !typed.is_empty() {
            parts.insert(self.at.get().min(parts.len()), typed);
        }
        parts.join(", ")
    }

    pub fn set_text(self: &Rc<Self>, text: &str) {
        self.items.borrow_mut().clear();
        self.at.set(0);
        self.set_entry("");
        self.commit_text(text);
    }

    /// Tapping a pill opens this contact menu.
    pub fn set_contact_ctx(&self, ctx: Rc<crate::contact_menu::Ctx>) {
        *self.contact_ctx.borrow_mut() = Some(ctx);
    }

    pub fn grab_focus(&self) {
        self.entry.grab_focus();
    }

    fn rebuild(self: &Rc<Self>) {
        let items = self.items.borrow().clone();
        let at = self.at.get().min(items.len());
        let mut children: Vec<gtk::Widget> = Vec::new();
        for (i, item) in items.iter().enumerate() {
            if i == at {
                children.push(self.entry.clone().upcast());
            }
            children.push(self.pill(i, item, self.armed.get() && i + 1 == at));
        }
        if at == items.len() {
            children.push(self.entry.clone().upcast());
        }
        let focused = self.entry.has_focus();
        self.widget.set_tiles(children);
        if focused {
            self.entry.grab_focus_without_selecting();
        }
    }

    fn pill(self: &Rc<Self>, index: usize, item: &Item, selected: bool) -> gtk::Widget {
        let pill = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        pill.add_css_class("recipient-pill");
        pill.set_valign(gtk::Align::Center);
        if selected {
            pill.add_css_class("selected");
        }
        let (label, tooltip) = match item {
            Item::Good(a) => (a.name.clone().unwrap_or_else(|| a.email.clone()), compose::format_address(a)),
            Item::Bad(t) => {
                pill.add_css_class("error");
                (t.clone(), format!("Not a valid address: {t}"))
            }
        };
        let l = gtk::Label::builder()
            .label(&label)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(28)
            .tooltip_text(&tooltip)
            .build();
        pill.append(&l);
        let remove = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Remove")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();
        pill.append(&remove);
        // Pills hold their index; any change rebuilds them all.
        let weak = Rc::downgrade(self);
        remove.connect_clicked(move |_| {
            let Some(this) = weak.upgrade() else { return };
            glib::idle_add_local_once(move || {
                let mut items = this.items.borrow_mut();
                if index < items.len() {
                    items.remove(index);
                    if this.at.get() > index {
                        this.at.set(this.at.get() - 1);
                    }
                }
                drop(items);
                this.rebuild();
                this.changed();
            });
        });
        let click = gtk::GestureClick::new();
        let weak = Rc::downgrade(self);
        click.connect_released(move |_, _, _, _| {
            let Some(this) = weak.upgrade() else { return };
            glib::idle_add_local_once(move || this.edit(index));
        });
        if let Item::Good(a) = item {
            let menu_click = gtk::GestureClick::builder().button(gdk::BUTTON_SECONDARY).build();
            let long = gtk::GestureLongPress::builder().touch_only(true).build();
            for gesture in [menu_click.upcast_ref::<gtk::Gesture>().clone(), long.upcast_ref::<gtk::Gesture>().clone()] {
                let (weak, a, anchor) = (Rc::downgrade(self), a.clone(), pill.downgrade());
                let fire = move |g: &gtk::Gesture, x: f64, y: f64| {
                    let (Some(this), Some(anchor)) = (weak.upgrade(), anchor.upgrade()) else { return };
                    let Some(ctx) = this.contact_ctx.borrow().clone() else { return };
                    g.set_state(gtk::EventSequenceState::Claimed);
                    // The gesture is on the label; the menu points from the pill.
                    let (x, y) = g
                        .widget()
                        .and_then(|w| w.compute_point(&anchor, &gtk::graphene::Point::new(x as f32, y as f32)))
                        .map_or((x, y), |p| (p.x() as f64, p.y() as f64));
                    let rect = gdk::Rectangle::new(x as i32, y as i32, 1, 1);
                    crate::contact_menu::popup(&ctx, &anchor, Some(rect), a.name.clone(), a.email.clone(), false);
                };
                if let Some(c) = gesture.downcast_ref::<gtk::GestureClick>() {
                    c.connect_pressed(move |g, _, x, y| fire(g.upcast_ref(), x, y));
                } else if let Some(l) = gesture.downcast_ref::<gtk::GestureLongPress>() {
                    l.connect_pressed(move |g, x, y| fire(g.upcast_ref(), x, y));
                }
                l.add_controller(gesture);
            }
        }
        l.add_controller(click);
        pill.upcast()
    }

    /// Puts pill `index` back into the entry, in its place.
    fn edit(self: &Rc<Self>, index: usize) {
        self.commit();
        let item = {
            let mut items = self.items.borrow_mut();
            if index >= items.len() {
                return;
            }
            items.remove(index)
        };
        self.at.set(index);
        self.set_entry(&item.text());
        self.rebuild();
        self.entry.grab_focus_without_selecting();
        self.entry.set_position(-1);
    }
}

/// `text` split at commas outside quotes and angle brackets.
fn split(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut cur, mut quoted, mut angle) = (String::new(), false, false);
    for c in text.chars() {
        match c {
            '"' => quoted = !quoted,
            '<' if !quoted => angle = true,
            '>' if !quoted => angle = false,
            ',' | ';' if !quoted && !angle => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    out.push(cur);
    out.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// An address alone, without a name: a space after it finishes it, while
/// a space after "Bob" continues a name.
fn is_bare_address(text: &str) -> bool {
    !text.contains(' ') && text.contains('@') && compose::parse_addresses(text).is_ok_and(|l| l.len() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitting() {
        assert_eq!(split("a@b.c, \"Doe, Jane\" <j@d.e>;x@y.z"), vec!["a@b.c", "\"Doe, Jane\" <j@d.e>", "x@y.z"]);
        assert_eq!(split(" , "), Vec::<String>::new());
    }
}
