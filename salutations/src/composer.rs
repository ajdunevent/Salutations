//! The message editor, shown in the message view's place. It holds the
//! fields and reports what the user does; the window saves, sends and
//! closes it.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use salutations_core::Address;
use salutations_core::camel::prelude::*;
use salutations_core::compose::{self, Attachment, Format, Inline, Outgoing, Size};
use sourceview5::prelude::*;

use crate::inline_image::{self, SizedImage};
use crate::recipients::RecipientField;

/// The placeholder GTK gives a child anchor in the buffer's text.
const ANCHOR: char = '\u{FFFC}';

/// An image embedded in the text.
struct Image {
    cid: String,
    name: String,
    /// The image as sent: the source, shrunk to its size when that makes
    /// it smaller.
    mime_type: String,
    data: glib::Bytes,
    /// The image the size applies to, as inserted or cropped: its bytes and
    /// type, and its picture upright.
    source: (glib::Bytes, String),
    /// The source's picture upright, and as turned by the user.
    upright: gdk::Texture,
    turn: inline_image::Turn,
    texture: gdk::Texture,
    size: Size,
    /// The image as inserted, for Reset: name, MIME type, data, texture.
    original: (String, String, glib::Bytes, gdk::Texture),
    /// Bumped by every change; a shrink finishing for an older one is
    /// dropped.
    version: u64,
}

/// Where an image stands in the text, and the paintable drawn there.
struct Place {
    anchor: gtk::TextChildAnchor,
    cid: String,
    paintable: SizedImage,
}

/// What the composer was opened for; the title and the Sent flags follow it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    New,
    Reply,
    Forward,
    Draft,
}

type Handler = Rc<dyn Fn()>;

/// How a message is protected; the protection menu's choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protection {
    None,
    Sign,
    SignEncrypt,
}

impl Protection {
    fn key(self) -> &'static str {
        match self {
            Protection::None => "none",
            Protection::Sign => "sign",
            Protection::SignEncrypt => "encrypt",
        }
    }

    fn from_key(k: &str) -> Self {
        match k {
            "sign" => Protection::Sign,
            "encrypt" => Protection::SignEncrypt,
            _ => Protection::None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Protection::None => "Not signed",
            Protection::Sign => "Signed",
            Protection::SignEncrypt => "Encrypted",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Protection::None => "channel-insecure-symbolic",
            Protection::Sign => "application-certificate-symbolic",
            Protection::SignEncrypt => "channel-secure-symbolic",
        }
    }
}

/// The account's OpenPGP settings as a new message starts with them.
#[derive(Debug, Clone, Copy)]
pub struct CryptoSetup {
    /// The account has a signing key; without one the menu is hidden.
    pub available: bool,
    pub protection: Protection,
    pub autocrypt: bool,
    pub attach: salutations_core::AttachKey,
}

/// One From choice: an address of an account.
#[derive(Debug, Clone)]
pub struct FromChoice {
    /// The account's UID.
    pub account: String,
    pub account_name: String,
    pub address: Address,
}

pub struct Composer {
    pub widget: adw::ToolbarView,
    title: adw::WindowTitle,
    from: gtk::DropDown,
    from_row: gtk::Box,
    froms: RefCell<Vec<FromChoice>>,
    /// The account the message is from, as last loaded or chosen.
    account: RefCell<String>,
    on_account: RefCell<Option<Rc<dyn Fn(String)>>>,
    to: Rc<RecipientField>,
    cc: Rc<RecipientField>,
    bcc: Rc<RecipientField>,
    cc_rows: gtk::Revealer,
    cc_button: gtk::Button,
    subject: gtk::Entry,
    buffer: sourceview5::Buffer,
    view: sourceview5::View,
    format: gio::SimpleAction,
    format_button: gtk::MenuButton,
    signature: gtk::ToggleButton,
    important: gtk::ToggleButton,
    /// The attachment tiles, with Show All under them.
    attachments_box: gtk::Box,
    tiles: crate::tile_grid::TileGrid,
    show_all: gtk::Button,
    attachments: RefCell<Vec<Attachment>>,
    /// A message forwarded as an attachment.
    forwarded: RefCell<Option<salutations_core::camel::MimeMessage>>,
    status: adw::Banner,
    send_button: gtk::Button,
    /// Kept so a saved draft and the sent message share one Message-ID.
    message_id: RefCell<String>,
    in_reply_to: RefCell<Option<String>>,
    forwarded_id: RefCell<Option<String>>,
    references: RefCell<Vec<String>>,
    /// The signature text inserted while the toggle is on, to take it out
    /// again when it is turned off.
    signature_text: RefCell<String>,
    signature_above: Cell<bool>,
    /// Around the inserted signature while the toggle is on. An edit
    /// inside makes it ordinary text and turns the toggle off.
    signature_marks: RefCell<Option<(gtk::TextMark, gtk::TextMark)>>,
    /// Set while the composer itself changes the buffer or the toggle.
    signature_internal: Cell<bool>,
    /// Changes the user made since the last save.
    dirty: Cell<bool>,
    /// Field changes made by the composer itself do not count as edits.
    loading: Cell<bool>,
    on_send: RefCell<Option<Handler>>,
    on_discard: RefCell<Option<Handler>>,
    on_changed: RefCell<Option<Handler>>,
    contacts: RefCell<Option<salutations_core::contacts::Contacts>>,
    protection: gio::SimpleAction,
    protection_button: gtk::MenuButton,
    protection_icon: gtk::Image,
    protection_label: gtk::Label,
    setup: Cell<Option<CryptoSetup>>,
    /// The account's public key, attached when its settings say so.
    key: RefCell<Option<Attachment>>,
    /// The user removed the key's tile; it is not added again.
    key_removed: Cell<bool>,
    image_button: gtk::Button,
    /// Images in the text, by Content-ID. They stay after their place is
    /// deleted; the message carries only those the text still shows.
    images: RefCell<Vec<Image>>,
    places: RefCell<Vec<Place>>,
    /// How wide an image in the editor may be.
    image_width: Cell<i32>,
    /// Deletions of image places under way. GTK's undo history records an
    /// image's place as a plain character, and not its insertion at all,
    /// so changes that insert or delete one are kept out of it.
    image_deletes: Cell<u32>,
    /// Images being made ready to send.
    preparing: Cell<u32>,
    spelling: RefCell<libspelling::TextBufferAdapter>,
}

/// `labels` keeps the field labels as wide as the widest, so the fields
/// line up without a fixed width.
fn field_row(label: &str, entry: &impl IsA<gtk::Widget>, labels: &gtk::SizeGroup) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("compose-field");
    let l = gtk::Label::new(Some(label));
    l.add_css_class("dimmed");
    l.set_xalign(0.0);
    // Level with the first line when pills wrap onto more.
    l.set_valign(gtk::Align::Start);
    labels.add_widget(&l);
    row.append(&l);
    row.append(entry);
    row
}

fn entry() -> gtk::Entry {
    let e = gtk::Entry::builder().hexpand(true).has_frame(false).build();
    e.add_css_class("compose-entry");
    e
}

impl Composer {
    pub fn new() -> Rc<Self> {
        let header = adw::HeaderBar::new();
        let title = adw::WindowTitle::new("New message", "");
        header.set_title_widget(Some(&title));
        let discard = gtk::Button::with_mnemonic("_Discard");
        discard.add_css_class("destructive-action");
        discard.set_tooltip_text(Some("Delete this message"));
        let send_button = gtk::Button::with_mnemonic("_Send");
        send_button.add_css_class("suggested-action");
        send_button.set_tooltip_text(Some("Send (Ctrl+Enter)"));

        let labels = gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal);
        let from = gtk::DropDown::from_strings(&[]);
        from.set_hexpand(true);
        // The button's label ellipsizes so a long address does not set
        // the window's minimum width; the list shows addresses whole.
        let factory = |ellipsize: bool| {
            let f = gtk::SignalListItemFactory::new();
            f.connect_setup(move |_, item| {
                let l = gtk::Label::builder().xalign(0.0).build();
                if ellipsize {
                    l.set_ellipsize(gtk::pango::EllipsizeMode::End);
                }
                item.downcast_ref::<gtk::ListItem>().expect("list item").set_child(Some(&l));
            });
            // An entry is "address" or, with several accounts,
            // "address\naccount"; the list shows the account under the
            // address, the button only the address.
            f.connect_bind(move |_, item| {
                let item = item.downcast_ref::<gtk::ListItem>().expect("list item");
                let text = item.item().and_downcast::<gtk::StringObject>().map(|s| s.string()).unwrap_or_default();
                let Some(l) = item.child().and_downcast::<gtk::Label>() else { return };
                match text.split_once('\n') {
                    Some((address, account)) if !ellipsize => l.set_markup(&format!(
                        "{}\n<small><span alpha=\"60%\">{}</span></small>",
                        glib::markup_escape_text(address),
                        glib::markup_escape_text(account)
                    )),
                    Some((address, _)) => l.set_text(address),
                    None => l.set_text(&text),
                }
            });
            f
        };
        from.set_factory(Some(&factory(true)));
        from.set_list_factory(Some(&factory(false)));
        from.add_css_class("flat");
        let from_row = field_row("From", &from, &labels);
        from_row.set_visible(false);
        let to = RecipientField::new();
        let to_row = field_row("To", &to.widget, &labels);
        let cc_button = gtk::Button::with_label("Cc, Bcc");
        cc_button.set_valign(gtk::Align::Center);
        to_row.append(&cc_button);
        let cc = RecipientField::new();
        let bcc = RecipientField::new();
        let cc_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        cc_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        cc_box.append(&field_row("Cc", &cc.widget, &labels));
        cc_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        cc_box.append(&field_row("Bcc", &bcc.widget, &labels));
        let cc_rows = gtk::Revealer::builder().child(&cc_box).reveal_child(false).build();
        let subject = entry();
        let subject_row = field_row("Subject", &subject, &labels);

        let buffer = sourceview5::Buffer::new(None);
        buffer.set_highlight_syntax(true);
        // GtkSourceView follows the light or dark style only through its scheme.
        let scheme = |b: &sourceview5::Buffer, dark: bool| {
            let name = if dark { "Adwaita-dark" } else { "Adwaita" };
            b.set_style_scheme(sourceview5::StyleSchemeManager::default().scheme(name).as_ref());
        };
        let style = adw::StyleManager::default();
        scheme(&buffer, style.is_dark());
        {
            let b = buffer.downgrade();
            style.connect_dark_notify(move |m| {
                if let Some(b) = b.upgrade() {
                    scheme(&b, m.is_dark());
                }
            });
        }
        let view = sourceview5::View::with_buffer(&buffer);
        view.set_wrap_mode(gtk::WrapMode::WordChar);
        view.set_vexpand(true);
        view.set_top_margin(12);
        view.set_bottom_margin(12);
        view.set_left_margin(12);
        view.set_right_margin(12);
        view.add_css_class("compose-body");
        // Spell checking in the system language, with suggestions in the
        // editor's context menu.
        let spelling = spell_checker(&buffer, &view);

        let tiles = crate::tile_grid::TileGrid::default();
        let show_all = gtk::Button::builder().halign(gtk::Align::Start).css_classes(["flat"]).visible(false).build();
        let attachments_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        attachments_box.set_margin_start(12);
        attachments_box.set_margin_end(12);
        attachments_box.set_margin_top(6);
        attachments_box.append(&tiles);
        attachments_box.append(&show_all);
        attachments_box.set_visible(false);

        use crate::label_bar::content;
        let format = gio::SimpleAction::new_stateful(
            "format",
            Some(glib::VariantTy::STRING),
            &Format::Markdown.key().to_variant(),
        );
        let format_menu = gio::Menu::new();
        for f in [Format::Markdown, Format::Plain] {
            let item = gio::MenuItem::new(Some(f.label()), None);
            item.set_action_and_target_value(Some("composer.format"), Some(&f.key().to_variant()));
            format_menu.append_item(&item);
        }
        let (c, format_label) = content("format-text-rich-symbolic", "Format", true);
        let format_button = gtk::MenuButton::builder().child(&c).menu_model(&format_menu).build();
        let (c, signature_label) = content("salutations-signature-symbolic", "Signature", false);
        let signature = gtk::ToggleButton::builder()
            .child(&c)
            .tooltip_text("Include the account's signature")
            .visible(false)
            .build();
        let (c, important_label) = content("salutations-important-symbolic", "Important", false);
        let important = gtk::ToggleButton::builder().child(&c).tooltip_text("Mark as high importance").build();
        let protection = gio::SimpleAction::new_stateful(
            "protection",
            Some(glib::VariantTy::STRING),
            &Protection::None.key().to_variant(),
        );
        let actions = gio::SimpleActionGroup::new();
        actions.add_action(&format);
        actions.add_action(&protection);
        let protection_menu = gio::Menu::new();
        for (label, p) in [("None", Protection::None), ("Sign only", Protection::Sign), ("Sign & encrypt", Protection::SignEncrypt)] {
            let item = gio::MenuItem::new(Some(label), None);
            item.set_action_and_target_value(Some("composer.protection"), Some(&p.key().to_variant()));
            protection_menu.append_item(&item);
        }
        let (c, protection_label) = content(Protection::None.icon(), Protection::None.label(), true);
        let protection_icon = c.first_child().and_downcast::<gtk::Image>().expect("icon first");
        let protection_button = gtk::MenuButton::builder()
            .child(&c)
            .menu_model(&protection_menu)
            .direction(gtk::ArrowType::Up)
            .visible(false)
            .build();
        let (c, attach_label) = content("mail-attachment-symbolic", "Attach", false);
        let attach = gtk::Button::builder().child(&c).tooltip_text("Attach files").build();
        let (c, image_label) = content("insert-image-symbolic", "Embed", false);
        let image_button = gtk::Button::builder().child(&c).tooltip_text("Embed images in the text").build();
        let tools = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        tools.add_css_class("toolbar");
        tools.add_css_class("compose-tools");
        tools.append(&format_button);
        tools.append(&signature);
        tools.append(&important);
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        tools.append(&spacer);
        tools.append(&image_button);
        tools.append(&attach);
        let tools = crate::label_bar::LabelBar::new(
            &tools,
            [attach_label, image_label, format_label, important_label, signature_label]
                .into_iter()
                .map(|l| vec![l])
                .collect(),
        );

        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        column.append(&from_row);
        column.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        column.append(&to_row);
        column.append(&cc_rows);
        column.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        column.append(&subject_row);
        column.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        column.append(&attachments_box);
        column.append(&tools);
        column.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        column.append(&view);
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&column)
            .build();
        let status = adw::Banner::new("");
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&status);
        content.append(&scroller);

        let ends = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        ends.append(&protection_button);
        ends.append(&send_button);
        let ends = crate::label_bar::LabelBar::new(&ends, vec![vec![protection_label.clone()]]);
        let bottom = gtk::CenterBox::new();
        bottom.add_css_class("toolbar");
        bottom.set_start_widget(Some(&discard));
        bottom.set_end_widget(Some(&ends));

        let widget = adw::ToolbarView::new();
        widget.insert_action_group("composer", Some(&actions));
        widget.add_top_bar(&header);
        widget.add_bottom_bar(&bottom);
        widget.set_content(Some(&content));

        let this = Rc::new(Self {
            widget,
            title,
            from,
            from_row,
            froms: RefCell::new(Vec::new()),
            account: RefCell::new(String::new()),
            on_account: RefCell::new(None),
            to,
            cc,
            bcc,
            cc_rows,
            cc_button,
            subject,
            buffer,
            view,
            format,
            format_button,
            signature,
            important,
            attachments_box,
            tiles,
            show_all,
            attachments: RefCell::new(Vec::new()),
            forwarded: RefCell::new(None),
            status,
            send_button,
            message_id: RefCell::new(String::new()),
            in_reply_to: RefCell::new(None),
            forwarded_id: RefCell::new(None),
            references: RefCell::new(Vec::new()),
            signature_text: RefCell::new(String::new()),
            signature_above: Cell::new(false),
            signature_marks: RefCell::new(None),
            signature_internal: Cell::new(false),
            dirty: Cell::new(false),
            loading: Cell::new(false),
            on_send: RefCell::new(None),
            on_discard: RefCell::new(None),
            on_changed: RefCell::new(None),
            contacts: RefCell::new(None),
            protection,
            protection_button,
            protection_icon,
            protection_label,
            setup: Cell::new(None),
            key: RefCell::new(None),
            key_removed: Cell::new(false),
            image_button,
            images: RefCell::new(Vec::new()),
            places: RefCell::new(Vec::new()),
            image_width: Cell::new(0),
            image_deletes: Cell::new(0),
            preparing: Cell::new(0),
            spelling: RefCell::new(spelling),
        });
        this.connect_crypto();
        this.connect(&discard, &attach);
        for f in [this.to.clone(), this.cc.clone(), this.bcc.clone()] {
            this.complete(&f);
        }
        this.update_format();
        this
    }

    fn connect(self: &Rc<Self>, discard: &gtk::Button, attach: &gtk::Button) {
        let weak = Rc::downgrade(self);
        self.send_button.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.emit(&this.on_send);
            }
        });
        let weak = Rc::downgrade(self);
        discard.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.emit(&this.on_discard);
            }
        });
        let weak = Rc::downgrade(self);
        attach.connect_clicked(move |b| {
            if let Some(this) = weak.upgrade() {
                this.choose_files(b);
            }
        });
        let weak = Rc::downgrade(self);
        self.image_button.connect_clicked(move |b| {
            if let Some(this) = weak.upgrade() {
                this.choose_images(b);
            }
        });
        let weak = Rc::downgrade(self);
        self.buffer.connect_mark_set(move |b, _, mark| {
            if *mark == b.get_insert()
                && let Some(this) = weak.upgrade()
            {
                this.update_cursor();
            }
        });
        let weak = Rc::downgrade(self);
        self.buffer.connect_changed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.update_cursor();
            }
        });
        let weak = Rc::downgrade(self);
        self.buffer.connect_delete_range(move |b, start, end| {
            if let Some(this) = weak.upgrade()
                && start.slice(end).contains(ANCHOR)
            {
                b.begin_irreversible_action();
                this.image_deletes.set(this.image_deletes.get() + 1);
            }
        });
        let weak = Rc::downgrade(self);
        self.buffer.connect_local("delete-range", true, move |args| {
            let this = weak.upgrade()?;
            if this.image_deletes.get() > 0 {
                this.image_deletes.set(this.image_deletes.get() - 1);
                args[0].get::<gtk::TextBuffer>().ok()?.end_irreversible_action();
            }
            None
        });
        if let Some(scroller) = self.view.ancestor(gtk::ScrolledWindow::static_type()).and_downcast::<gtk::ScrolledWindow>() {
            let weak = Rc::downgrade(self);
            scroller.hadjustment().connect_page_size_notify(move |adj| {
                let Some(this) = weak.upgrade() else { return };
                // The editor's side margins, and room so a full-width
                // image does not touch them.
                let width = adj.page_size() as i32 - 24 - 8;
                if width > 0 && width != this.image_width.get() {
                    this.image_width.set(width);
                    this.refresh_images();
                }
            });
        }
        let weak = Rc::downgrade(self);
        self.cc_button.connect_clicked(move |b| {
            if let Some(this) = weak.upgrade() {
                this.cc_rows.set_reveal_child(true);
                b.set_visible(false);
                this.cc.grab_focus();
            }
        });
        let weak = Rc::downgrade(self);
        self.subject.connect_changed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.edited();
            }
        });
        for f in [&self.to, &self.cc, &self.bcc] {
            let weak = Rc::downgrade(self);
            f.connect_changed(move || {
                if let Some(this) = weak.upgrade() {
                    this.edited();
                }
            });
        }
        let weak = Rc::downgrade(self);
        self.buffer.connect_changed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.edited();
            }
        });
        let weak = Rc::downgrade(self);
        self.from.connect_selected_notify(move |_| {
            let Some(this) = weak.upgrade() else { return };
            if this.loading.get() {
                return;
            }
            let chosen = this.froms.borrow().get(this.from.selected() as usize).map(|c| c.account.clone());
            if let Some(uid) = chosen.filter(|u| *u != *this.account.borrow()) {
                *this.account.borrow_mut() = uid.clone();
                let handler = this.on_account.borrow().clone();
                if let Some(f) = handler {
                    f(uid);
                }
            }
            this.edited();
        });
        let weak = Rc::downgrade(self);
        self.format.connect_change_state(move |a, v| {
            let Some(this) = weak.upgrade() else { return };
            if let Some(v) = v {
                a.set_state(v);
            }
            this.update_format();
            this.edited();
        });
        let weak = Rc::downgrade(self);
        self.important.connect_toggled(move |_| {
            if let Some(this) = weak.upgrade() {
                this.edited();
            }
        });
        let weak = Rc::downgrade(self);
        self.show_all.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.tiles.set_expanded(!this.tiles.is_expanded());
            }
        });
        let weak = Rc::downgrade(self);
        self.tiles.connect_overflow(move |rows, hidden| {
            let Some(this) = weak.upgrade() else { return };
            this.show_all.set_visible(rows > 2);
            this.show_all.set_label(&if hidden > 0 { format!("Show all ({} more)", hidden) } else { "Show fewer".into() });
        });
        let weak = Rc::downgrade(self);
        self.signature.connect_toggled(move |b| {
            if let Some(this) = weak.upgrade()
                && !this.loading.get()
                && !this.signature_internal.get()
            {
                this.apply_signature(b.is_active());
                this.edited();
            }
        });
        let weak = Rc::downgrade(self);
        self.buffer.connect_insert_text(move |_, at, _| {
            if let Some(this) = weak.upgrade() {
                let o = at.offset();
                this.signature_edited(o, o, false);
            }
        });
        let weak = Rc::downgrade(self);
        self.buffer.connect_delete_range(move |_, start, end| {
            if let Some(this) = weak.upgrade() {
                this.signature_edited(start.offset(), end.offset(), true);
            }
        });
        // Ctrl+Enter sends from any field.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, mods| {
            let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
            if mods.contains(gdk::ModifierType::CONTROL_MASK) && matches!(key, gdk::Key::Return | gdk::Key::KP_Enter) {
                this.emit(&this.on_send);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.widget.add_controller(keys);
        // Files dropped anywhere on the composer are attached, or images
        // embedded where they were dropped. Capture phase, so the fields
        // and the editor do not take them as text.
        let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        drop.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        drop.connect_drop(move |_, value, x, y| {
            let (Some(this), Ok(list)) = (weak.upgrade(), value.get::<gdk::FileList>()) else { return false };
            // A link dragged from a browser also arrives as a file list.
            let files: Vec<gio::File> = list.files().into_iter().filter(|f| f.path().is_some()).collect();
            if files.is_empty() {
                return false;
            }
            this.files_dropped(files, x, y);
            true
        });
        self.widget.add_controller(drop);
    }

    fn emit(&self, slot: &RefCell<Option<Handler>>) {
        let cb = slot.borrow().clone();
        if let Some(cb) = cb {
            cb();
        }
    }

    fn edited(&self) {
        if self.loading.get() {
            return;
        }
        self.dirty.set(true);
        self.emit(&self.on_changed);
    }

    fn connect_crypto(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.protection.connect_change_state(move |a, v| {
            let Some(this) = weak.upgrade() else { return };
            if let Some(v) = v {
                a.set_state(v);
            }
            this.update_protection();
            this.update_key_tile();
            this.edited();
        });
    }

    pub fn protection(&self) -> Protection {
        if !self.setup.get().is_some_and(|s| s.available) {
            return Protection::None;
        }
        self.protection.state().and_then(|v| v.get::<String>()).map_or(Protection::None, |k| Protection::from_key(&k))
    }

    fn update_protection(&self) {
        let p = self.protection();
        self.protection_icon.set_icon_name(Some(p.icon()));
        self.protection_label.set_text(p.label());
        self.protection_button.set_tooltip_text(Some(p.label()));
    }

    /// Shows the account's protection menu in its starting state.
    pub fn set_crypto(&self, setup: CryptoSetup) {
        self.setup.set(Some(setup));
        self.protection_button.set_visible(setup.available);
        self.key.borrow_mut().take();
        self.key_removed.set(false);
        self.protection.set_state(&setup.protection.key().to_variant());
        self.update_protection();
    }

    /// The account's key, exported once the composer is open; attached
    /// when the account's settings call for it.
    pub fn set_key(self: &Rc<Self>, key: Attachment) {
        *self.key.borrow_mut() = Some(key);
        self.update_key_tile();
    }

    fn is_key(&self, a: &Attachment) -> bool {
        self.key.borrow().as_ref().is_some_and(|k| k.name == a.name && k.mime_type == a.mime_type)
    }

    /// Adds or removes the key's tile as the settings and the protection
    /// choice say, until the user removes it.
    fn update_key_tile(self: &Rc<Self>) {
        use salutations_core::AttachKey as A;
        let (Some(setup), Some(key)) = (self.setup.get(), self.key.borrow().clone()) else { return };
        if self.key_removed.get() {
            return;
        }
        let want = setup.available
            && setup.autocrypt
            && match setup.attach {
                A::Always => true,
                A::WhenProtected => self.protection() != Protection::None,
                A::Never => false,
            };
        // A reopened draft has it already.
        let at = self.attachments.borrow().iter().position(|a| self.is_key(a));
        match (want, at) {
            (true, None) => self.attachments.borrow_mut().push(key),
            (false, Some(i)) => {
                self.attachments.borrow_mut().remove(i);
            }
            _ => return,
        }
        self.refresh_attachments();
    }

    /// The address books recipients are completed from.
    pub fn set_contacts(&self, contacts: salutations_core::contacts::Contacts) {
        *self.contacts.borrow_mut() = Some(contacts);
    }

    pub fn contacts(&self) -> Option<salutations_core::contacts::Contacts> {
        self.contacts.borrow().clone()
    }

    /// Tapping a recipient pill opens this contact menu.
    pub fn set_contact_ctx(&self, ctx: Rc<crate::contact_menu::Ctx>) {
        for f in [&self.to, &self.cc, &self.bcc] {
            f.set_contact_ctx(ctx.clone());
        }
    }

    /// Offers address book matches for the address being typed in `entry`
    /// (the text after its last comma) in a list under it.
    fn complete(self: &Rc<Self>, field: &Rc<RecipientField>) {
        let entry = &field.entry;
        let list = gtk::ListBox::new();
        list.add_css_class("navigation-sidebar");
        let popover = gtk::Popover::builder()
            .child(&list)
            .autohide(false)
            .has_arrow(false)
            .position(gtk::PositionType::Bottom)
            .halign(gtk::Align::Start)
            .build();
        popover.set_parent(entry);
        let generation = Rc::new(Cell::new(0u64));
        let accept = {
            let (field, popover) = (Rc::downgrade(field), popover.downgrade());
            Rc::new(move |text: &str| {
                let (Some(field), Some(popover)) = (field.upgrade(), popover.upgrade()) else { return };
                popover.popdown();
                field.accept(text);
            })
        };
        let a = accept.clone();
        list.connect_row_activated(move |_, row| {
            if let Some(text) = row.child().and_downcast::<gtk::Label>().map(|l| l.label()) {
                a(&text);
            }
        });
        let weak = Rc::downgrade(self);
        let (l, p, g) = (list.clone(), popover.clone(), generation.clone());
        entry.connect_changed(move |e| {
            let Some(this) = weak.upgrade() else { return };
            let text = e.text().to_string();
            let token = text.trim().to_string();
            let n = g.get() + 1;
            g.set(n);
            let contacts = this.contacts.borrow().clone();
            if this.loading.get() || token.chars().count() < 2 || token.contains('<') || contacts.is_none() {
                p.popdown();
                return;
            }
            let (l, p, g) = (l.clone(), p.clone(), g.clone());
            glib::spawn_future_local(async move {
                let contacts = contacts.expect("checked");
                let found = gio::spawn_blocking(move || contacts.search_sync(&token, 6)).await.unwrap_or_default();
                if g.get() != n {
                    return;
                }
                l.remove_all();
                if found.is_empty() {
                    p.popdown();
                    return;
                }
                for m in &found {
                    let a = Address { name: (!m.name.is_empty()).then(|| m.name.clone()), email: m.email.clone() };
                    let label = gtk::Label::builder().label(compose::format_address(&a)).xalign(0.0).build();
                    l.append(&label);
                }
                l.select_row(l.row_at_index(0).as_ref());
                p.popup();
            });
        });
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let (l, p) = (list.clone(), popover.clone());
        keys.connect_key_pressed(move |_, key, _, _| {
            if !p.is_visible() {
                return glib::Propagation::Proceed;
            }
            let index = l.selected_row().map_or(-1, |r| r.index());
            match key {
                gdk::Key::Down | gdk::Key::Up => {
                    let next = if key == gdk::Key::Down { index + 1 } else { index - 1 };
                    if let Some(row) = l.row_at_index(next.max(0)) {
                        l.select_row(Some(&row));
                    }
                    glib::Propagation::Stop
                }
                gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::Tab => {
                    match l.selected_row().and_then(|r| r.child()).and_downcast::<gtk::Label>() {
                        Some(label) => {
                            accept(&label.label());
                            glib::Propagation::Stop
                        }
                        None => glib::Propagation::Proceed,
                    }
                }
                gdk::Key::Escape => {
                    p.popdown();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        entry.add_controller(keys);
        let p = popover.clone();
        let focus = gtk::EventControllerFocus::new();
        focus.connect_leave(move |_| {
            // A click on the list arrives after focus leaves; closing waits
            // for it.
            let p = p.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || p.popdown());
        });
        entry.add_controller(focus);
    }

    pub fn connect_send(&self, f: impl Fn() + 'static) {
        *self.on_send.borrow_mut() = Some(Rc::new(f));
    }

    pub fn connect_discard(&self, f: impl Fn() + 'static) {
        *self.on_discard.borrow_mut() = Some(Rc::new(f));
    }

    /// The UID of the account the message is from.
    pub fn account(&self) -> String {
        self.account.borrow().clone()
    }

    /// Another account chosen in From, with its UID.
    pub fn connect_account_changed(&self, f: impl Fn(String) + 'static) {
        *self.on_account.borrow_mut() = Some(Rc::new(f));
    }

    /// Any edit by the user.
    pub fn connect_changed(&self, f: impl Fn() + 'static) {
        *self.on_changed.borrow_mut() = Some(Rc::new(f));
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty.get()
    }

    pub fn mark_saved(&self) {
        self.dirty.set(false);
    }

    /// A save failed: the next one must still happen.
    pub fn mark_dirty(&self) {
        self.dirty.set(true);
    }

    pub fn format(&self) -> Format {
        self.format.state().and_then(|v| v.get::<String>()).and_then(|k| Format::from_key(&k)).unwrap_or_default()
    }

    fn update_format(self: &Rc<Self>) {
        self.format_button.set_tooltip_text(Some(&format!("Format: {}", self.format().label())));
        self.update_language();
        let markdown = self.format() == Format::Markdown;
        self.image_button.set_visible(markdown);
        if !markdown && !self.loading.get() {
            self.images_to_attachments();
        }
    }

    fn update_language(&self) {
        let lang = (self.format() == Format::Markdown)
            .then(|| sourceview5::LanguageManager::default().language("markdown"))
            .flatten();
        self.buffer.set_language(lang.as_ref());
    }

    /// The buffer's text with [`ANCHOR`] at each image, so offsets match
    /// the buffer's.
    fn slice(&self) -> String {
        let (start, end) = self.buffer.bounds();
        self.buffer.slice(&start, &end, true).to_string()
    }

    /// The text as sent: each image as its image token.
    fn text(&self) -> String {
        let slice = self.slice();
        if !slice.contains(ANCHOR) {
            return slice;
        }
        let mut out = String::with_capacity(slice.len());
        for (offset, c) in slice.chars().enumerate() {
            if c != ANCHOR {
                out.push(c);
                continue;
            }
            let anchor = self.buffer.iter_at_offset(offset as i32).child_anchor();
            if let Some(token) = anchor.and_then(|a| self.token(&a)) {
                out.push_str(&token);
            }
        }
        out
    }

    fn token(&self, anchor: &gtk::TextChildAnchor) -> Option<String> {
        let places = self.places.borrow();
        let cid = &places.iter().find(|p| p.anchor == *anchor)?.cid;
        let images = self.images.borrow();
        let image = images.iter().find(|i| i.cid == *cid)?;
        Some(compose::image_token(&image.name, cid, image.size))
    }

    /// Replaces the text, showing image tokens for known images as the
    /// images. Not undoable.
    fn set_body(self: &Rc<Self>, text: &str) {
        let b = &self.buffer;
        b.begin_irreversible_action();
        b.set_text("");
        self.places.borrow_mut().clear();
        let mut end = b.end_iter();
        let mut last = 0;
        if self.format() == Format::Markdown {
            for t in compose::image_tokens(text) {
                let known = match self.images.borrow_mut().iter_mut().find(|i| i.cid == t.cid) {
                    Some(image) => {
                        image.size = t.size;
                        true
                    }
                    None => false,
                };
                if !known {
                    continue;
                }
                b.insert(&mut end, &text[last..t.range.start]);
                self.insert_place(&mut end, &t.cid);
                last = t.range.end;
            }
        }
        b.insert(&mut end, &text[last..]);
        b.end_irreversible_action();
    }

    /// Fills the composer. `froms` holds every account's addresses, the
    /// message's `account` among them; `from` is selected when it is one
    /// of that account's. `signature` is the account's, inserted when
    /// `include_signature` is on.
    #[allow(clippy::too_many_arguments)]
    pub fn load(
        self: &Rc<Self>,
        kind: Kind,
        out: &Outgoing,
        froms: Vec<FromChoice>,
        account: &str,
        signature: &str,
        include_signature: bool,
        signature_above: bool,
        account_name: &str,
    ) {
        self.loading.set(true);
        self.title.set_title(match kind {
            Kind::New | Kind::Draft => "New message",
            Kind::Reply => "Reply",
            Kind::Forward => "Forward",
        });
        self.title.set_subtitle(account_name);
        let several = froms.iter().any(|c| c.account != account);
        let names: Vec<String> = froms
            .iter()
            .map(|c| {
                let a = compose::format_address(&c.address);
                if several { format!("{a}\n{}", c.account_name) } else { a }
            })
            .collect();
        let model = gtk::StringList::new(&names.iter().map(String::as_str).collect::<Vec<_>>());
        self.from.set_model(Some(&model));
        let pick = froms
            .iter()
            .position(|c| c.account == account && c.address.email.eq_ignore_ascii_case(&out.from.email))
            .or_else(|| froms.iter().position(|c| c.account == account))
            .unwrap_or(0);
        self.from.set_selected(pick as u32);
        self.from_row.set_visible(froms.len() > 1);
        *self.froms.borrow_mut() = froms;
        *self.account.borrow_mut() = account.to_string();
        self.to.set_text(&out.to);
        self.cc.set_text(&out.cc);
        self.bcc.set_text(&out.bcc);
        let show_cc = !out.cc.is_empty() || !out.bcc.is_empty();
        self.cc_rows.set_reveal_child(show_cc);
        self.cc_button.set_visible(!show_cc);
        self.subject.set_text(&out.subject);
        self.format.set_state(&out.format.key().to_variant());
        self.update_format();
        self.important.set_active(out.important);
        *self.signature_text.borrow_mut() = signature.trim_end().to_string();
        self.signature_above.set(signature_above);
        self.signature.set_visible(!signature.trim().is_empty());
        let body = if include_signature && !signature.trim().is_empty() {
            compose::with_signature(&out.body, signature, signature_above)
        } else {
            out.body.clone()
        };
        self.signature.set_active(include_signature && !signature.trim().is_empty());
        *self.images.borrow_mut() = out
            .inline
            .iter()
            .filter_map(|i| {
                let texture = inline_image::decode(&i.data)?;
                Some(Image {
                    cid: i.cid.clone(),
                    name: i.name.clone(),
                    mime_type: i.mime_type.clone(),
                    data: i.data.clone(),
                    source: (i.data.clone(), i.mime_type.clone()),
                    upright: texture.clone(),
                    turn: Default::default(),
                    texture: texture.clone(),
                    size: Size::Natural,
                    original: (i.name.clone(), i.mime_type.clone(), i.data.clone(), texture),
                    version: 0,
                })
            })
            .collect();
        self.signature_internal.set(true);
        self.set_body(&body);
        self.signature_internal.set(false);
        self.mark_signature(self.signature.is_active());
        self.buffer.place_cursor(&self.buffer.start_iter());
        *self.attachments.borrow_mut() = out.attachments.clone();
        *self.forwarded.borrow_mut() = out.forwarded.clone();
        self.refresh_attachments();
        *self.message_id.borrow_mut() = out.message_id.clone();
        *self.in_reply_to.borrow_mut() = out.in_reply_to.clone();
        *self.forwarded_id.borrow_mut() = out.forwarded_id.clone();
        *self.references.borrow_mut() = out.references.clone();
        self.status.set_revealed(false);
        self.dirty.set(false);
        self.loading.set(false);
    }

    /// Puts the cursor where the user starts typing: To when it is empty,
    /// otherwise the start of the body.
    pub fn focus(&self) {
        if self.to.text().is_empty() {
            self.to.grab_focus();
        } else {
            self.view.grab_focus();
        }
    }

    pub fn outgoing(&self) -> Outgoing {
        let froms = self.froms.borrow();
        let from = froms.get(self.from.selected() as usize).map(|c| c.address.clone()).unwrap_or_default();
        Outgoing {
            from,
            to: self.to.text(),
            cc: self.cc.text(),
            bcc: self.bcc.text(),
            subject: self.subject.text().to_string(),
            body: self.text(),
            format: self.format(),
            message_id: self.message_id.borrow().clone(),
            in_reply_to: self.in_reply_to.borrow().clone(),
            forwarded_id: self.forwarded_id.borrow().clone(),
            references: self.references.borrow().clone(),
            attachments: self.attachments.borrow().clone(),
            inline: self
                .images
                .borrow()
                .iter()
                .map(|i| Inline { cid: i.cid.clone(), name: i.name.clone(), mime_type: i.mime_type.clone(), data: i.data.clone() })
                .collect(),
            forwarded: self.forwarded.borrow().clone(),
            autocrypt: None,
            sign: self.protection() != Protection::None,
            encrypt: self.protection() == Protection::SignEncrypt,
            important: self.important.is_active(),
        }
    }

    /// Shows a problem above the fields, or hides it with None.
    pub fn set_status(&self, message: Option<&str>) {
        match message {
            Some(m) => {
                self.status.set_title(m);
                self.status.set_revealed(true);
            }
            None => self.status.set_revealed(false),
        }
    }

    pub fn set_sending(&self, sending: bool) {
        self.send_button.set_sensitive(!sending);
        self.send_button.set_label(if sending { "Sending…" } else { "Send" });
    }

    /// After another account is chosen in From: its name, its signature
    /// in place of the old one while the signature is on, and its keys.
    /// The protection choice stays when the account can sign.
    pub fn switch_account(self: &Rc<Self>, account_name: &str, signature: &str, signature_above: bool, mut setup: CryptoSetup) {
        self.title.set_subtitle(account_name);
        let was_on = self.signature.is_visible() && self.signature.is_active();
        if was_on {
            self.apply_signature(false);
        }
        *self.signature_text.borrow_mut() = signature.trim_end().to_string();
        self.signature_above.set(signature_above);
        let has = !signature.trim().is_empty();
        self.signature.set_visible(has);
        self.signature_internal.set(true);
        self.signature.set_active(was_on && has);
        self.signature_internal.set(false);
        if was_on && has {
            self.apply_signature(true);
        } else {
            self.mark_signature(false);
        }
        setup.protection = if setup.available { self.chosen_protection() } else { Protection::None };
        // The old account's key leaves with it.
        let old_key = self.key.borrow().clone();
        if let Some(k) = old_key {
            self.attachments.borrow_mut().retain(|a| !(a.name == k.name && a.mime_type == k.mime_type));
            self.refresh_attachments();
        }
        self.set_crypto(setup);
    }

    /// The protection menu's state, whether or not the account can sign.
    fn chosen_protection(&self) -> Protection {
        self.protection.state().and_then(|v| v.get::<String>()).map_or(Protection::None, |k| Protection::from_key(&k))
    }

    /// Adds or removes the signature where `load` would have put it.
    fn apply_signature(self: &Rc<Self>, on: bool) {
        let sig = self.signature_text.borrow().clone();
        if sig.is_empty() {
            return;
        }
        let text = self.text();
        let new = if on {
            compose::with_signature(&text, &sig, self.signature_above.get())
        } else {
            let Some(at) = text.rfind(&sig) else { return };
            let before = text[..at].trim_end_matches('\n');
            let after = text[at + sig.len()..].trim_start_matches('\n');
            if after.is_empty() { format!("{before}\n") } else { format!("{before}\n\n{after}") }
        };
        self.signature_internal.set(true);
        self.set_body(&new);
        self.signature_internal.set(false);
        self.mark_signature(on);
    }

    /// Marks where the inserted signature is: the first copy when it goes
    /// above the quote, the last when below.
    fn mark_signature(&self, on: bool) {
        if let Some((a, b)) = self.signature_marks.take() {
            self.buffer.delete_mark(&a);
            self.buffer.delete_mark(&b);
        }
        let sig = self.signature_text.borrow().clone();
        if !on || sig.is_empty() {
            return;
        }
        let text = self.slice();
        let found = if self.signature_above.get() { text.find(&sig) } else { text.rfind(&sig) };
        let Some(at) = found else { return };
        let start = text[..at].chars().count() as i32;
        let end = start + sig.chars().count() as i32;
        let a = self.buffer.create_mark(None, &self.buffer.iter_at_offset(start), true);
        let b = self.buffer.create_mark(None, &self.buffer.iter_at_offset(end), false);
        *self.signature_marks.borrow_mut() = Some((a, b));
    }

    /// An insertion at `from` or a deletion of `from..to`. Typing at either
    /// edge of the signature is outside it.
    fn signature_edited(&self, from: i32, to: i32, deleting: bool) {
        if self.signature_internal.get() || self.loading.get() {
            return;
        }
        let range = self.signature_marks.borrow().as_ref().map(|(a, b)| {
            (self.buffer.iter_at_mark(a).offset(), self.buffer.iter_at_mark(b).offset())
        });
        let Some((start, end)) = range else { return };
        let inside = if deleting { from < end && to > start } else { from > start && from < end };
        if !inside {
            return;
        }
        // Not now: the buffer is mid-change and its marks must stay valid.
        let (buffer, marks) = (self.buffer.clone(), self.signature_marks.take());
        if let Some((a, b)) = marks {
            glib::idle_add_local_once(move || {
                buffer.delete_mark(&a);
                buffer.delete_mark(&b);
            });
        }
        self.signature_internal.set(true);
        self.signature.set_active(false);
        self.signature_internal.set(false);
    }

    /// GTK draws the cursor as tall as its line, which beside an image is
    /// the image's height; on such a line it is hidden.
    fn update_cursor(&self) {
        let mut start = self.buffer.iter_at_mark(&self.buffer.get_insert());
        start.set_line_offset(0);
        let mut end = start;
        if !end.ends_line() {
            end.forward_to_line_end();
        }
        self.view.set_cursor_visible(!start.slice(&end).contains(ANCHOR));
    }

    /// The width images in the editor are narrowed to.
    fn max_image_width(&self) -> i32 {
        match self.image_width.get() {
            w if w > 0 => w,
            _ => inline_image::START_WIDTH as i32,
        }
    }

    /// Puts the image `cid` at `iter`, which ends up after it.
    fn insert_place(self: &Rc<Self>, iter: &mut gtk::TextIter, cid: &str) {
        let found = self.images.borrow().iter().find(|i| i.cid == cid).map(|i| (i.texture.clone(), i.size));
        let Some((texture, size)) = found else { return };
        let shown = inline_image::shown_size(size, (texture.width(), texture.height()), self.max_image_width());
        let paintable = SizedImage::new(&texture, shown);
        self.buffer.begin_irreversible_action();
        let anchor = self.buffer.create_child_anchor(iter);
        self.buffer.end_irreversible_action();
        self.restart_spelling();
        let actions = self.image_actions(&anchor, cid);
        let weak = Rc::downgrade(self);
        // The menu hangs from the composer: a popover inside the text view
        // moves focus into it, which scrolls the editor and closes it.
        let widget = inline_image::widget(&paintable, move |from, point| {
            let Some(this) = weak.upgrade() else { return };
            let menu = gtk::PopoverMenu::from_model(Some(&image_menu()));
            menu.insert_action_group("image", Some(&actions));
            menu.set_parent(&this.widget);
            let target = match point {
                Some((x, y)) => from
                    .compute_point(&this.widget, &gtk::graphene::Point::new(x as f32, y as f32))
                    .map(|p| gdk::Rectangle::new(p.x() as i32, p.y() as i32, 1, 1)),
                None => from
                    .compute_bounds(&this.widget)
                    .map(|b| gdk::Rectangle::new(b.x() as i32, b.y() as i32, b.width() as i32, b.height() as i32)),
            };
            menu.set_pointing_to(target.as_ref());
            let view = this.view.downgrade();
            menu.connect_closed(move |m| {
                let (m, view) = (m.clone(), view.clone());
                glib::idle_add_local_once(move || {
                    m.unparent();
                    // Focus would otherwise stay with the closed menu.
                    if let Some(v) = view.upgrade() {
                        v.grab_focus();
                    }
                });
            });
            menu.popup();
        });
        self.view.add_child_at_anchor(&widget, &anchor);
        self.places.borrow_mut().push(Place { anchor, cid: cid.to_string(), paintable });
    }

    /// libspelling follows the buffer's length through GTK's commit
    /// notifications, which an inserted image does not send; typing past an
    /// image then failed its assertion (`offset <= region->length`) and
    /// aborted. A new checker starts from the buffer's real length.
    fn restart_spelling(&self) {
        let old = self.spelling.replace(spell_checker(&self.buffer, &self.view));
        let underline = old.tag();
        // SAFETY: nothing uses the old checker after this; disposing it
        // removes its commit notification before the next edit reaches it.
        unsafe { old.run_dispose() };
        if let Some(tag) = underline {
            self.buffer.tag_table().remove(&tag);
        }
    }

    /// Redraws every image in the editor at its size.
    fn refresh_images(&self) {
        let max = self.max_image_width();
        self.places.borrow_mut().retain(|p| !p.anchor.is_deleted());
        let images = self.images.borrow();
        for p in self.places.borrow().iter() {
            if let Some(i) = images.iter().find(|i| i.cid == p.cid) {
                p.paintable.set(&i.texture, inline_image::shown_size(i.size, (i.texture.width(), i.texture.height()), max));
            }
        }
    }

    fn change_image(self: &Rc<Self>, cid: &str, f: impl FnOnce(&mut Image)) {
        if let Some(i) = self.images.borrow_mut().iter_mut().find(|i| i.cid == cid) {
            f(i);
        }
        self.prepare_image(cid);
        self.refresh_images();
        self.edited();
    }

    /// Makes the bytes sent for image `cid` from its source at its size,
    /// off the main thread.
    fn prepare_image(self: &Rc<Self>, cid: &str) {
        let job = {
            let mut images = self.images.borrow_mut();
            let Some(i) = images.iter_mut().find(|i| i.cid == cid) else { return };
            i.version += 1;
            let target = inline_image::sent_pixels(i.size, (i.texture.width(), i.texture.height()));
            (i.version, i.texture.clone(), i.source.clone(), target, !i.turn.is_none())
        };
        let (version, texture, (data, mime), target, turned) = job;
        self.preparing.set(self.preparing.get() + 1);
        let (this, cid) = (self.clone(), cid.to_string());
        glib::spawn_future_local(async move {
            let out = gio::spawn_blocking(move || inline_image::prepare(&texture, &data, &mime, target, turned)).await;
            this.preparing.set(this.preparing.get() - 1);
            if let Ok((data, mime)) = out
                && let Some(i) = this.images.borrow_mut().iter_mut().find(|i| i.cid == cid && i.version == version)
            {
                (i.data, i.mime_type) = (data, mime);
            }
        });
    }

    /// Whether images are still being made ready to send.
    pub fn images_pending(&self) -> bool {
        self.preparing.get() > 0
    }

    /// Waits for [`Self::images_pending`] to end.
    pub async fn images_ready(&self) {
        while self.images_pending() {
            glib::timeout_future(std::time::Duration::from_millis(50)).await;
        }
    }

    /// The actions of one image's menu.
    fn image_actions(self: &Rc<Self>, anchor: &gtk::TextChildAnchor, cid: &str) -> gio::SimpleActionGroup {
        let group = gio::SimpleActionGroup::new();
        let action = |name: &str, param: Option<&glib::VariantTy>| {
            let a = gio::SimpleAction::new(name, param);
            group.add_action(&a);
            a
        };
        let (weak, id) = (Rc::downgrade(self), cid.to_string());
        action("crop", None).connect_activate(move |_, _| {
            let (Some(this), id) = (weak.upgrade(), id.clone()) else { return };
            glib::spawn_future_local(async move { this.crop_image(&id).await });
        });
        let (weak, id) = (Rc::downgrade(self), cid.to_string());
        action("scale", None).connect_activate(move |_, _| {
            let (Some(this), id) = (weak.upgrade(), id.clone()) else { return };
            glib::spawn_future_local(async move {
                let found = this.images.borrow().iter().find(|i| i.cid == id).map(|i| {
                    let pixels = (i.texture.width(), i.texture.height());
                    (inline_image::css_size(i.size, pixels), pixels)
                });
                let Some((current, pixels)) = found else { return };
                if let Some((w, _)) = inline_image::scale_dialog(&this.widget, current, pixels).await {
                    this.change_image(&id, |i| i.size = Size::Width(w));
                }
            });
        });
        let (weak, id) = (Rc::downgrade(self), cid.to_string());
        action("stretch", None).connect_activate(move |_, _| {
            let (Some(this), id) = (weak.upgrade(), id.clone()) else { return };
            glib::spawn_future_local(async move {
                let current = this
                    .images
                    .borrow()
                    .iter()
                    .find(|i| i.cid == id)
                    .map(|i| inline_image::css_size(i.size, (i.texture.width(), i.texture.height())));
                let Some(current) = current else { return };
                if let Some((w, h)) = inline_image::stretch_dialog(&this.widget, current).await {
                    this.change_image(&id, |i| i.size = Size::Stretched(w, h));
                }
            });
        });
        let (weak, id) = (Rc::downgrade(self), cid.to_string());
        action("reset", None).connect_activate(move |_, _| {
            let Some(this) = weak.upgrade() else { return };
            this.change_image(&id, |i| {
                let (name, mime, data, texture) = i.original.clone();
                i.size = inline_image::start_size((texture.width(), texture.height()));
                i.source = (data.clone(), mime.clone());
                (i.upright, i.turn) = (texture.clone(), Default::default());
                (i.name, i.mime_type, i.data, i.texture) = (name, mime, data, texture);
            });
        });
        for (name, rotate) in [("rotate", true), ("flip", false)] {
            let (weak, id) = (Rc::downgrade(self), cid.to_string());
            action(name, None).connect_activate(move |_, _| {
                let (Some(this), id) = (weak.upgrade(), id.clone()) else { return };
                glib::spawn_future_local(async move { this.turn_image(&id, rotate).await });
            });
        }
        let (weak, anchor) = (Rc::downgrade(self), anchor.clone());
        action("remove", None).connect_activate(move |_, _| {
            let (weak, anchor) = (weak.clone(), anchor.clone());
            // Not from inside the menu: removing the place destroys it.
            glib::idle_add_local_once(move || {
                let Some(this) = weak.upgrade() else { return };
                if anchor.is_deleted() {
                    return;
                }
                let mut start = this.buffer.iter_at_child_anchor(&anchor);
                let mut end = start;
                end.forward_char();
                this.buffer.delete(&mut start, &mut end);
                this.buffer.place_cursor(&start);
            });
        });
        group
    }

    async fn crop_image(self: &Rc<Self>, cid: &str) {
        let found = self.images.borrow().iter().find(|i| i.cid == cid).map(|i| (i.texture.clone(), i.name.clone(), i.source.1.clone()));
        let Some((texture, name, mime)) = found else { return };
        let Some(rect) = inline_image::crop_dialog(&self.widget, &texture).await else { return };
        let t = texture.clone();
        let Ok((data, cropped_mime)) = gio::spawn_blocking(move || inline_image::crop(&t, rect, &mime)).await else { return };
        let Some(cropped) = inline_image::decode(&data) else { return };
        self.change_image(cid, |i| {
            // A stretched image keeps its width; the height follows the crop.
            if let Size::Stretched(w, _) = i.size {
                i.size = Size::Width(w);
            }
            if cropped_mime != i.source.1 {
                i.name = inline_image::png_name(&name);
            }
            // The crop is of the turned picture, which becomes the source.
            i.source = (data.clone(), cropped_mime.clone());
            (i.upright, i.turn) = (cropped.clone(), Default::default());
            (i.mime_type, i.data, i.texture) = (cropped_mime, data, cropped);
        });
    }

    /// Turns image `cid` a quarter turn clockwise, or mirrors it left to
    /// right.
    async fn turn_image(self: &Rc<Self>, cid: &str, rotate: bool) {
        // The turn is recorded at once, so quick repeats each count.
        let found = self.images.borrow_mut().iter_mut().find(|i| i.cid == cid).map(|i| {
            if rotate {
                // The shown size turns with the picture: 600 × 400 becomes
                // 400 × 600. The picture's size before this turn comes from
                // the turn, since its texture may not have caught up.
                let (w, h) = (i.upright.width(), i.upright.height());
                let before = if i.turn.quarters % 2 == 1 { (h, w) } else { (w, h) };
                i.size = match i.size {
                    Size::Width(_) => Size::Width(inline_image::css_size(i.size, before).1.max(1) as u32),
                    Size::Stretched(w, h) => Size::Stretched(h, w),
                    Size::Natural => Size::Natural,
                };
            }
            i.turn = if rotate { i.turn.rotated() } else { i.turn.flipped() };
            (i.upright.clone(), i.turn)
        });
        let Some((upright, turn)) = found else { return };
        let Ok(texture) = gio::spawn_blocking(move || inline_image::turned(&upright, turn)).await else { return };
        let current = self.images.borrow().iter().any(|i| i.cid == cid && i.turn == turn);
        if current {
            self.change_image(cid, |i| i.texture = texture);
        }
    }

    fn choose_images(self: &Rc<Self>, button: &impl IsA<gtk::Widget>) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Images"));
        filter.add_mime_type("image/*");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder().title("Insert images").modal(true).filters(&filters).build();
        let parent = button.root().and_downcast::<gtk::Window>();
        let weak = Rc::downgrade(self);
        dialog.open_multiple(parent.as_ref(), None::<&gio::Cancellable>, move |result| {
            let (Some(this), Ok(model)) = (weak.upgrade(), result) else { return };
            let files: Vec<gio::File> = (0..model.n_items()).filter_map(|i| model.item(i).and_downcast()).collect();
            let at = this.buffer.create_mark(None, &this.buffer.iter_at_mark(&this.buffer.get_insert()), false);
            glib::spawn_future_local(async move { this.embed_files(files, at).await });
        });
    }

    /// Files dropped at (`x`, `y`) on the composer. In a Markdown message,
    /// images can go into the text there; everything else is attached.
    fn files_dropped(self: &Rc<Self>, files: Vec<gio::File>, x: f64, y: f64) {
        if self.format() != Format::Markdown {
            self.attach_files(files);
            return;
        }
        let is_image = |f: &gio::File| {
            let name = f.basename().map(|b| b.to_string_lossy().into_owned()).unwrap_or_default();
            let (ct, _) = gio::content_type_guess(Some(&name), None);
            gio::content_type_get_mime_type(&ct).is_some_and(|m| m.starts_with("image/"))
        };
        let (images, others): (Vec<gio::File>, Vec<gio::File>) = files.into_iter().partition(is_image);
        if !others.is_empty() {
            self.attach_files(others);
        }
        if images.is_empty() {
            return;
        }
        let iter = inline_image::iter_at_point(&self.widget, &self.view, x, y)
            .unwrap_or_else(|| self.buffer.iter_at_mark(&self.buffer.get_insert()));
        let at = self.buffer.create_mark(None, &iter, false);
        let this = self.clone();
        glib::spawn_future_local(async move {
            let body = if images.len() == 1 {
                "An embedded image shows in the text where it was dropped.".to_string()
            } else {
                format!("Embedded images show in the text where they were dropped ({} images).", images.len())
            };
            let dialog = adw::AlertDialog::new(Some("Embed here or attach?"), Some(&body));
            dialog.add_responses(&[("cancel", "_Cancel"), ("attach", "_Attach"), ("embed", "_Embed")]);
            dialog.set_response_appearance("embed", adw::ResponseAppearance::Suggested);
            dialog.set_default_response(Some("embed"));
            dialog.set_close_response("cancel");
            match dialog.choose_future(Some(&this.widget)).await.as_str() {
                "embed" => this.embed_files(images, at).await,
                "attach" => {
                    this.buffer.delete_mark(&at);
                    this.attach_files(images);
                }
                _ => this.buffer.delete_mark(&at),
            }
        });
    }

    /// Reads `files` and puts them into the text at `at`, each on a line of
    /// its own, in order; deletes the mark after. A file GTK cannot show is
    /// attached instead.
    async fn embed_files(self: &Rc<Self>, files: Vec<gio::File>, at: gtk::TextMark) {
        let from = self.froms.borrow().get(self.from.selected() as usize).map(|c| c.address.email.clone()).unwrap_or_default();
        for file in files {
            let name = file.basename().map(|b| b.to_string_lossy().into_owned()).unwrap_or_else(|| "image".into());
            let data = match file.load_contents_future().await {
                Ok((data, _)) => glib::Bytes::from_owned(data),
                Err(e) => {
                    self.set_status(Some(&format!("Cannot insert {name}: {}", e.message())));
                    continue;
                }
            };
            let d = data.clone();
            let texture = gio::spawn_blocking(move || inline_image::decode(&d)).await.ok().flatten();
            let (ct, _) = gio::content_type_guess(Some(&name), &data[..]);
            let mime = gio::content_type_get_mime_type(&ct).map(|m| m.to_string()).unwrap_or_else(|| "application/octet-stream".into());
            let Some(texture) = texture else {
                self.set_status(Some(&format!("{name} cannot be shown in the text, so it was attached")));
                self.attachments.borrow_mut().push(Attachment { name, mime_type: mime, data });
                self.refresh_attachments();
                self.edited();
                continue;
            };
            let cid = compose::new_content_id(&from);
            self.images.borrow_mut().push(Image {
                cid: cid.clone(),
                name: name.clone(),
                mime_type: mime.clone(),
                data: data.clone(),
                source: (data.clone(), mime.clone()),
                upright: texture.clone(),
                turn: Default::default(),
                texture: texture.clone(),
                size: inline_image::start_size((texture.width(), texture.height())),
                original: (name, mime, data, texture),
                version: 0,
            });
            self.prepare_image(&cid);
            if at.is_deleted() {
                return;
            }
            let mut iter = self.buffer.iter_at_mark(&at);
            if !iter.starts_line() {
                self.buffer.insert(&mut iter, "\n");
            }
            self.insert_place(&mut iter, &cid);
            // The cursor goes to the line after the image, where typing
            // continues the text.
            if !iter.ends_line() || iter.is_end() {
                self.buffer.insert(&mut iter, "\n");
            } else {
                iter.forward_line();
            }
            self.buffer.place_cursor(&iter);
        }
        self.buffer.delete_mark(&at);
        self.view.grab_focus();
    }

    /// Plain text cannot show images: the ones in the text become
    /// attachments.
    fn images_to_attachments(self: &Rc<Self>) {
        let places: Vec<(gtk::TextChildAnchor, String)> = self
            .places
            .borrow()
            .iter()
            .filter(|p| !p.anchor.is_deleted())
            .map(|p| (p.anchor.clone(), p.cid.clone()))
            .collect();
        if places.is_empty() {
            return;
        }
        let several = places.len() > 1;
        let mut moved: Vec<String> = Vec::new();
        for (anchor, cid) in places {
            let mut start = self.buffer.iter_at_child_anchor(&anchor);
            let mut end = start;
            end.forward_char();
            self.buffer.delete(&mut start, &mut end);
            if moved.contains(&cid) {
                continue;
            }
            if let Some(i) = self.images.borrow().iter().find(|i| i.cid == cid) {
                self.attachments.borrow_mut().push(Attachment { name: i.name.clone(), mime_type: i.mime_type.clone(), data: i.data.clone() });
            }
            moved.push(cid);
        }
        self.places.borrow_mut().clear();
        self.refresh_attachments();
        self.set_status(Some(if several {
            "Plain text cannot show images, so the images were attached"
        } else {
            "Plain text cannot show images, so the image was attached"
        }));
    }

    fn choose_files(self: &Rc<Self>, button: &impl IsA<gtk::Widget>) {
        let dialog = gtk::FileDialog::builder().title("Attach files").modal(true).build();
        let parent = button.root().and_downcast::<gtk::Window>();
        let weak = Rc::downgrade(self);
        dialog.open_multiple(parent.as_ref(), None::<&gio::Cancellable>, move |result| {
            let (Some(this), Ok(model)) = (weak.upgrade(), result) else { return };
            let files: Vec<gio::File> = (0..model.n_items()).filter_map(|i| model.item(i).and_downcast()).collect();
            this.attach_files(files);
        });
    }

    fn attach_files(self: &Rc<Self>, files: Vec<gio::File>) {
        for file in files {
            let weak = Rc::downgrade(self);
            glib::spawn_future_local(async move {
                let name = file.basename().map(|b| b.to_string_lossy().into_owned()).unwrap_or_else(|| "attachment".into());
                let data = match file.load_contents_future().await {
                    Ok((data, _)) => glib::Bytes::from_owned(data),
                    Err(e) => {
                        if let Some(this) = weak.upgrade() {
                            this.set_status(Some(&format!("Cannot attach {name}: {}", e.message())));
                        }
                        return;
                    }
                };
                let (mime, _) = gio::content_type_guess(Some(&name), &data[..]);
                let mime = gio::content_type_get_mime_type(&mime).map(|m| m.to_string()).unwrap_or_else(|| "application/octet-stream".into());
                let Some(this) = weak.upgrade() else { return };
                this.attachments.borrow_mut().push(Attachment { name, mime_type: mime, data });
                this.refresh_attachments();
                this.edited();
            });
        }
    }

    fn refresh_attachments(self: &Rc<Self>) {
        let forwarded = self.forwarded.borrow().as_ref().map(|m| {
            let s = m.subject().map(|s| s.to_string()).unwrap_or_default();
            let name = format!("{}.eml", if s.trim().is_empty() { "Forwarded message" } else { s.trim() });
            (name, None, "message/rfc822".to_string(), None)
        });
        let files: Vec<(String, Option<String>, String, Option<usize>)> = self
            .attachments
            .borrow()
            .iter()
            .enumerate()
            .map(|(i, a)| (a.name.clone(), Some(glib::format_size(a.data.len() as u64).to_string()), a.mime_type.clone(), Some(i)))
            .collect();
        let mut tiles = Vec::new();
        for (name, size, mime, index) in forwarded.into_iter().chain(files) {
            let tile = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            tile.add_css_class("attachment-tile");
            tile.append(&crate::tile_grid::file_label(&name, size.as_deref(), &mime));
            let remove = gtk::Button::from_icon_name("window-close-symbolic");
            remove.add_css_class("flat");
            remove.add_css_class("circular");
            remove.set_valign(gtk::Align::Center);
            remove.set_tooltip_text(Some("Remove"));
            let weak = Rc::downgrade(self);
            remove.connect_clicked(move |_| {
                let Some(this) = weak.upgrade() else { return };
                match index {
                    Some(i) => {
                        let mut a = this.attachments.borrow_mut();
                        if i < a.len() {
                            let gone = a.remove(i);
                            if this.is_key(&gone) {
                                this.key_removed.set(true);
                            }
                        }
                    }
                    None => *this.forwarded.borrow_mut() = None,
                }
                // Not from inside the button's handler: it is removed.
                glib::idle_add_local_once(move || {
                    this.refresh_attachments();
                    this.edited();
                });
            });
            tile.append(&remove);
            tiles.push(tile.upcast());
        }
        let any = !tiles.is_empty();
        self.tiles.set_tiles(tiles);
        self.attachments_box.set_visible(any);
    }
}

/// The menu over each image in the editor. One section: with Reset and
/// Remove in a second, GTK measured the menu short and it scrolled.
fn image_menu() -> gio::Menu {
    let menu = gio::Menu::new();
    menu.append(Some("_Crop…"), Some("image.crop"));
    menu.append(Some("_Scale…"), Some("image.scale"));
    menu.append(Some("S_tretch…"), Some("image.stretch"));
    menu.append(Some("R_otate"), Some("image.rotate"));
    menu.append(Some("_Flip"), Some("image.flip"));
    menu.append(Some("_Reset"), Some("image.reset"));
    menu.append(Some("Re_move"), Some("image.remove"));
    menu
}

/// Spell checking in the system language, with suggestions in the editor's
/// context menu.
fn spell_checker(buffer: &sourceview5::Buffer, view: &sourceview5::View) -> libspelling::TextBufferAdapter {
    let spelling = libspelling::TextBufferAdapter::new(buffer, &libspelling::Checker::default());
    view.set_extra_menu(Some(&spelling.menu_model()));
    view.insert_action_group("spelling", Some(&spelling));
    spelling.set_enabled(true);
    spelling
}
