//! The message action buttons of the message view's header bar and of the
//! selection toolbar. At the left, actions that take messages out of the
//! folder, in rising severity (Restore, Move, Archive, Junk, Trash), going
//! into a ⋮ menu from the right as space runs short; at the right, flags
//! that leave them in place (Read, Star). Which buttons show follows the
//! folder.

use adw::prelude::*;
use gtk::gio;

/// The kind of folder the messages are in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Place {
    Ordinary,
    Trash,
    Junk,
    /// Search results from several folders.
    Mixed,
}

pub(super) struct ActionButtons {
    pub start: crate::overflow_box::OverflowBox,
    pub end: gtk::Box,
    trash: gtk::Button,
    /// In Junk: Trash and Delete Permanently.
    trash_menu: gtk::MenuButton,
    delete_permanently: gtk::Button,
    restore: gtk::Button,
    not_junk: gtk::Button,
    archive: gtk::Button,
    junk: gtk::Button,
    read: gtk::Button,
    star: gtk::Button,
    /// Action name, button and its tooltip, for explaining why an action
    /// is unavailable.
    tooltips: Vec<(&'static str, gtk::Widget, &'static str)>,
}

fn button(icon: &str, tooltip: &str, action: &str) -> gtk::Button {
    gtk::Button::builder().icon_name(icon).tooltip_text(tooltip).action_name(action).build()
}

impl ActionButtons {
    pub fn new() -> Self {
        let trash = button("user-trash-symbolic", "Trash", "win.delete");
        let menu = gio::Menu::new();
        menu.append(Some("_Trash"), Some("win.trash"));
        menu.append(Some("_Delete permanently"), Some("win.delete-permanently"));
        let trash_menu = gtk::MenuButton::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Trash")
            .menu_model(&menu)
            .build();
        let delete_permanently = button("salutations-delete-permanently-symbolic", "Delete permanently", "win.delete-permanently");
        delete_permanently.add_css_class("delete-permanently");
        let restore = button("salutations-restore-symbolic", "Restore", "win.restore");
        let not_junk = button("mail-mark-notjunk-symbolic", "Not junk", "win.not-junk");
        let archive = button("salutations-archive-symbolic", "Archive", "win.archive");
        let move_ = button("salutations-folder-move-symbolic", "Move to folder", "win.move");
        let junk = button("mail-mark-junk-symbolic", "Mark as junk", "win.junk");
        let read = button("salutations-mark-read-symbolic", "Mark as read", "win.toggle-read");
        let star = button("salutations-non-starred-symbolic", "Star", "win.toggle-star");
        use crate::overflow_box::{OverflowBox, item};
        let start = OverflowBox::new(vec![
            (restore.clone().upcast(), item("_Restore", "win.restore")),
            (move_.clone().upcast(), item("_Move to folder…", "win.move")),
            (archive.clone().upcast(), item("_Archive", "win.archive")),
            (not_junk.clone().upcast(), item("_Not junk", "win.not-junk")),
            (junk.clone().upcast(), item("Mark as _junk", "win.junk")),
            (trash.clone().upcast(), item("_Trash", "win.delete")),
            (trash_menu.clone().upcast(), menu.clone().upcast()),
            (delete_permanently.clone().upcast(), item("_Delete permanently", "win.delete-permanently")),
        ]);
        let end = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        end.append(&read);
        end.append(&star);
        let tooltips = vec![
            ("trash", trash.clone().upcast(), "Trash"),
            ("delete-permanently", delete_permanently.clone().upcast(), "Delete permanently"),
            ("restore", restore.clone().upcast(), "Restore"),
            ("archive", archive.clone().upcast(), "Archive"),
            ("junk", junk.clone().upcast(), "Mark as junk"),
            ("not-junk", not_junk.clone().upcast(), "Not junk"),
        ];
        let this = Self { start, end, trash, trash_menu, delete_permanently, restore, not_junk, archive, junk, read, star, tooltips };
        this.set_place(Place::Ordinary, true);
        this
    }

    /// Shows the buttons for messages in `place`; `has_junk` is whether the
    /// account has a Junk folder.
    pub fn set_place(&self, place: Place, has_junk: bool) {
        use Place::*;
        // Mixed shows every action; those that do not fit every checked
        // message are disabled, with the reason in the tooltip.
        self.restore.set_visible(matches!(place, Trash | Mixed));
        self.not_junk.set_visible(matches!(place, Junk | Mixed));
        self.trash.set_visible(matches!(place, Ordinary | Mixed));
        // Delete moves to Trash, or deletes for good in an account without
        // one; across folders only the move is offered.
        self.trash.set_action_name(Some(if place == Mixed { "win.trash" } else { "win.delete" }));
        self.trash_menu.set_visible(place == Junk);
        self.delete_permanently.set_visible(matches!(place, Trash | Mixed));
        self.archive.set_visible(place != Trash);
        self.junk.set_visible(place != Junk && has_junk);
        self.start.queue_resize();
    }

    /// `unread` is whether any message is unread, `starred` whether all are
    /// starred.
    pub fn set_flags(&self, unread: bool, starred: bool) {
        let (icon, tip) = if unread {
            ("salutations-mark-read-symbolic", "Mark as read")
        } else {
            ("mail-unread-symbolic", "Mark as unread")
        };
        self.read.set_icon_name(icon);
        self.read.set_tooltip_text(Some(tip));
        let (icon, tip) = if starred { ("starred-symbolic", "Unstar") } else { ("salutations-non-starred-symbolic", "Star") };
        self.star.set_icon_name(icon);
        self.star.set_tooltip_text(Some(tip));
    }

    /// Why the action `name` cannot apply now, shown after its tooltip.
    pub fn set_reason(&self, name: &str, reason: Option<&str>) {
        if let Some((_, w, base)) = self.tooltips.iter().find(|(n, _, _)| *n == name) {
            w.set_tooltip_text(Some(&match reason {
                Some(r) => format!("{base} ({r})"),
                None => base.to_string(),
            }));
        }
    }

    pub fn set_visible(&self, on: bool) {
        self.start.set_visible(on);
        self.end.set_visible(on);
    }
}
