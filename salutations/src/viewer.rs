//! The reading pane: header block, remote-content banner and a WebKit view
//! with JavaScript off and remote loads blocked by a Content-Security-Policy.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use salutations_core::camel::{self, prelude::*};
use salutations_core::links::{self, Link};
use salutations_core::remote;
use salutations_core::render::{self, Address, Rendered};
use salutations_core::Settings;
use salutations_core::crypto;
use webkit::prelude::*;

thread_local! {
    /// The message whose `cid:` parts the web view may load.
    static SHOWN: RefCell<Option<camel::MimeMessage>> = const { RefCell::new(None) };
    /// Its decrypted content, where `cid:` parts of encrypted mail are.
    static SHOWN_BODY: RefCell<Option<camel::MimePart>> = const { RefCell::new(None) };
    /// Remote content is allowed for the shown message, so its rewritten
    /// URIs may be fetched.
    static REMOTE_ALLOWED: Cell<bool> = const { Cell::new(false) };
    /// What is removed from the shown message's remote addresses, and the
    /// address of the account it arrived in.
    static STRIP: RefCell<(remote::Strip, Option<String>)> = const { RefCell::new((remote::Strip::Nothing, None)) };
    /// Counts loaded pages, so a late failure from an earlier page is not
    /// put on the current one.
    static PAGE: Cell<u64> = const { Cell::new(0) };
    /// Loads of the current page that failed after removal: of tracking
    /// parameters, of the address.
    static STRIP_FAILED: Cell<(bool, bool)> = const { Cell::new((false, false)) };
    /// Shows the banner's offer to loosen removal after such a failure.
    static ON_STRIP_FAILED: RefCell<Option<Box<dyn Fn()>>> = const { RefCell::new(None) };
    /// A link dialog is open; another click on a link does nothing.
    static LINK_DIALOG: Cell<bool> = const { Cell::new(false) };
    /// The last print's settings, so the next print starts with the same
    /// printer and options.
    static PRINT_SETTINGS: RefCell<Option<gtk::PrintSettings>> = const { RefCell::new(None) };
}

/// Registers the `cid:` and remote image schemes on the default web
/// context; call once.
pub fn register_cid_scheme() {
    let ctx = webkit::WebContext::default().expect("default web context");
    // Fonts load with CORS checks, which a custom scheme fails otherwise.
    if let Some(sec) = ctx.security_manager() {
        sec.register_uri_scheme_as_cors_enabled(remote::SCHEME);
    }
    ctx.register_uri_scheme(remote::SCHEME, |req| {
        let url = req.uri().and_then(|u| remote::remote_url(&u));
        let req = req.clone();
        let Some(url) = url.filter(|_| REMOTE_ALLOWED.get()) else {
            let mut e = glib::Error::new(gio::IOErrorEnum::PermissionDenied, "Remote content is blocked");
            req.finish_error(&mut e);
            return;
        };
        let stripped = STRIP.with_borrow(|(level, address)| remote::strip(&url, *level, address.as_deref()));
        let page = PAGE.get();
        glib::spawn_future_local(async move {
            match remote::fetch(&stripped.url).await {
                Ok((bytes, mime)) => {
                    let bytes = if mime == "text/css" {
                        remote::rewrite_css(&String::from_utf8_lossy(&bytes), Some(&stripped.url)).into_bytes()
                    } else {
                        bytes
                    };
                    let len = bytes.len() as i64;
                    let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from_owned(bytes));
                    req.finish(&stream, len, Some(&mime));
                }
                Err(mut e) => {
                    if page == PAGE.get() && (stripped.tracking || stripped.address) {
                        let (t, a) = STRIP_FAILED.get();
                        STRIP_FAILED.set((t || stripped.tracking, a || stripped.address));
                        ON_STRIP_FAILED.with_borrow(|f| f.as_ref().map(|f| f()));
                    }
                    req.finish_error(&mut e);
                }
            }
        });
    });
    ctx.register_uri_scheme("cid", |req| {
        let cid = req.path().map(|p| p.to_string()).unwrap_or_default();
        let found = SHOWN_BODY
            .with_borrow(|b| b.as_ref().and_then(|b| render::inline_part_in(b, &cid)))
            .or_else(|| SHOWN.with_borrow(|m| m.as_ref().and_then(|m| render::inline_part(m, &cid))));
        match found {
            Some((bytes, mime)) => {
                let len = bytes.len() as i64;
                let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from_owned(bytes));
                req.finish(&stream, len, Some(&mime));
            }
            None => {
                let mut e = glib::Error::new(gio::IOErrorEnum::NotFound, "No such part");
                req.finish_error(&mut e);
            }
        }
    });
}

const CSP_BLOCKED: &str = "default-src 'none'; img-src cid: data:; style-src 'unsafe-inline'; font-src data:";
/// Images, stylesheets and fonts come only through the remote scheme,
/// which caches them; nothing loads from the network directly.
const CSP_ALLOWED: &str = "default-src 'none'; img-src cid: data: x-salutations-remote:; \
     style-src x-salutations-remote: 'unsafe-inline'; font-src x-salutations-remote: data:";

struct Shown {
    folder: camel::Folder,
    uid: String,
    rendered: Rendered,
    allow_once: bool,
    /// Tracking removal loosened for a message without a Message-ID.
    strip_once: Option<remote::Strip>,
    /// Encryption and signature, when the message had any.
    protection: Option<crypto::Protection>,
    /// The decrypted or verified content shown in place of the message's.
    body: Option<camel::MimePart>,
    attachments: Vec<salutations_core::compose::Attachment>,
}

/// Carries a message to the decrypting thread and back; only one thread
/// touches it at a time.
struct Handoff<T>(T);
unsafe impl<T> Send for Handoff<T> {}

type ShownHandler = Rc<dyn Fn(&camel::Folder, &str)>;
type LinkAction = Box<dyn Fn(&Rc<Viewer>)>;

pub struct Viewer {
    pub widget: gtk::Stack,
    subject: gtk::Label,
    avatar: adw::Avatar,
    /// Avatar and warning badge; the warning replaces the avatar when the
    /// sender failed verification.
    avatar_stack: gtk::Stack,
    from: gtk::Label,
    to: gtk::Label,
    cc_title: gtk::Label,
    cc: gtk::Label,
    date: gtk::Label,
    important: gtk::Image,
    /// The attachment tiles, with Show All and Save All under them.
    attachments_box: gtk::Box,
    /// "You replied to this message on …" and the like, under the headers.
    responses: gtk::Box,
    tiles: crate::tile_grid::TileGrid,
    show_all: gtk::Button,
    save_all: gtk::Button,
    /// Remote content: blocked, with Show, or allowed, with Remove.
    banner: gtk::Grid,
    banner_text: gtk::Label,
    /// Shown when the provider recorded a DMARC failure.
    verify_banner: gtk::Grid,
    verify_text: gtk::Label,
    /// Opens the sender verification details; shown when there are any.
    verify_button: gtk::MenuButton,
    show_menu: gtk::MenuButton,
    /// Loosens tracking removal after content failed to load without it.
    loosen: gtk::Button,
    /// The address of the account a folder belongs to.
    account_address: RefCell<Option<Box<dyn Fn(&camel::Folder) -> Option<String>>>>,
    actions: gio::SimpleActionGroup,
    web: webkit::WebView,
    shown: RefCell<Option<Shown>>,
    generation: Cell<u64>,
    /// The message being fetched; a second request for it is dropped
    /// rather than starting a duplicate fetch.
    loading: RefCell<Option<(camel::Folder, String)>>,
    on_shown: RefCell<Option<ShownHandler>>,
    /// Anchors of the loaded page, by the index in their rewritten href.
    links: RefCell<Vec<Link>>,
    /// Decrypts and verifies; None until the window provides it.
    gpg: RefCell<Option<camel::GpgContext>>,
    /// Signature and encryption status.
    crypto_banner: gtk::Grid,
    crypto_icon: gtk::Image,
    crypto_text: gtk::Label,
    /// Offers a key the message carries.
    key_banner: gtk::Box,
    key_text: gtk::Label,
    key_accept: gtk::Button,
    key_handler: RefCell<Option<glib::SignalHandlerId>>,
    /// The full header or its two-line compact form.
    head: gtk::Stack,
    compact_subject: gtk::Label,
    compact_from: gtk::Label,
    compact_date: gtk::Label,
    /// Paperclip with the attachment count; shown when there are any.
    compact_attach: gtk::Overlay,
    compact_count: gtk::Label,
    /// Hidden while compact: attachments, key offer, remote content.
    extras: [gtk::Box; 3],
    /// Hidden while compact unless it warns.
    crypto_rev: gtk::Box,
    compact: Cell<bool>,
    /// The sender, then To, then Cc; a header link's href is the index.
    people: RefCell<Vec<Address>>,
    /// Where To and Cc start in `people`.
    people_at: Cell<(usize, usize)>,
    contact_ctx: RefCell<Option<Rc<crate::contact_menu::Ctx>>>,
}

fn label(classes: &[&str]) -> gtk::Label {
    let l = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .selectable(true)
        .build();
    for c in classes {
        l.add_css_class(c);
    }
    l
}

/// The last 16 hex digits of a fingerprint or key ID, in groups of four.
fn short_key(k: &str) -> String {
    let k = k.to_ascii_uppercase();
    let tail = &k[k.len().saturating_sub(16)..];
    tail.as_bytes().chunks(4).map(|c| std::str::from_utf8(c).unwrap_or("")).collect::<Vec<_>>().join(" ")
}

/// A banner's row: icon, text, buttons. A grid, because a horizontal
/// GtkBox around a wrapping label asks for more height than it uses.
fn banner_grid() -> gtk::Grid {
    gtk::Grid::builder().column_spacing(12).build()
}

/// Appends `w` to the banner's row.
fn add(grid: &gtk::Grid, w: &impl IsA<gtk::Widget>) {
    let mut n = 0;
    let mut c = grid.first_child();
    while let Some(w) = c {
        n += 1;
        c = w.next_sibling();
    }
    w.set_valign(gtk::Align::Center);
    grid.attach(w, n, 0, 1, 1);
}

/// A banner's text wraps; its narrowest is a few words, so the banner's
/// height stays bounded when the pane is narrow.
fn wrap(label: &gtk::Label) {
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label.set_width_chars(12);
}

/// `list` as links, numbered from `first`, for the contact menu.
fn name_links(list: &[Address], first: usize) -> String {
    list.iter()
        .enumerate()
        .map(|(i, a)| {
            format!(
                "<a href=\"{}\" title=\"{}\">{}</a>",
                first + i,
                glib::markup_escape_text(&a.email),
                glib::markup_escape_text(a.display())
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

impl Viewer {
    pub fn new() -> Rc<Self> {
        let settings = webkit::Settings::new();
        settings.set_enable_javascript(false);
        settings.set_enable_javascript_markup(false);
        settings.set_enable_page_cache(false);
        settings.set_enable_html5_database(false);
        settings.set_enable_html5_local_storage(false);
        settings.set_enable_webgl(false);
        settings.set_enable_media(false);
        settings.set_enable_back_forward_navigation_gestures(false);
        let web = webkit::WebView::builder()
            .settings(&settings)
            .network_session(&webkit::NetworkSession::new_ephemeral())
            .vexpand(true)
            .build();

        let subject = label(&["viewer-subject"]);
        let avatar = adw::Avatar::new(48, None, true);
        avatar.set_valign(gtk::Align::Start);
        let grid = gtk::Grid::builder().column_spacing(10).row_spacing(3).build();
        grid.add_css_class("viewer-headers");
        let field = |text: &str| {
            let l = gtk::Label::new(Some(text));
            l.set_xalign(0.0);
            l.set_yalign(0.0);
            l.add_css_class("field");
            l
        };
        let from = label(&[]);
        from.set_hexpand(true);
        let to = label(&[]);
        let cc = label(&[]);
        let date = label(&[]);
        let cc_title = field("Cc");
        // A horizontal GtkBox around wrapping labels reports a minimum
        // height above its natural height; one grid avoids it.
        // AdwAvatar draws its color from a class it manages, so the warning
        // is a separate widget rather than a restyled avatar.
        let warning = gtk::Image::from_icon_name("dialog-warning-symbolic");
        warning.set_pixel_size(24);
        warning.set_size_request(48, 48);
        warning.set_valign(gtk::Align::Start);
        warning.add_css_class("verify-failed");
        let avatar_stack = gtk::Stack::new();
        avatar_stack.add_named(&avatar, Some("avatar"));
        avatar_stack.add_named(&warning, Some("warning"));
        avatar_stack.set_valign(gtk::Align::Start);
        avatar_stack.set_margin_end(4);
        grid.attach(&avatar_stack, 0, 0, 1, 4);
        grid.attach(&field("From"), 1, 0, 1, 1);
        grid.attach(&from, 2, 0, 1, 1);
        // The same icon whatever the result, so it is not read as a badge.
        let verify_button = gtk::MenuButton::builder()
            .icon_name("security-medium-symbolic")
            .tooltip_text("Sender verification")
            .valign(gtk::Align::Center)
            .build();
        verify_button.add_css_class("flat");
        verify_button.add_css_class("verify-button");
        grid.attach(&verify_button, 3, 0, 1, 1);
        grid.attach(&field("To"), 1, 1, 1, 1);
        grid.attach(&to, 2, 1, 1, 1);
        grid.attach(&cc_title, 1, 2, 1, 1);
        grid.attach(&cc, 2, 2, 1, 1);
        grid.attach(&field("Date"), 1, 3, 1, 1);
        grid.attach(&date, 2, 3, 1, 1);
        let important = gtk::Image::from_icon_name("salutations-important-symbolic");
        important.set_tooltip_text(Some("High importance"));
        important.add_css_class("important");
        important.set_halign(gtk::Align::Center);
        important.set_visible(false);
        grid.attach(&important, 3, 3, 1, 1);
        let people = grid.clone();
        let header = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(16)
            .margin_top(18)
            .margin_bottom(12)
            .margin_start(18)
            .margin_end(18)
            .build();
        // Collapses the headers; the compact ones have the opposite button.
        let collapse = gtk::Button::builder()
            .icon_name("pan-up-symbolic")
            .tooltip_text("Show fewer details")
            .valign(gtk::Align::Start)
            .css_classes(["flat", "circular"])
            .build();
        subject.set_hexpand(true);
        let subject_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        subject_row.append(&subject);
        subject_row.append(&collapse);
        header.append(&subject_row);
        header.append(&people);
        let responses = gtk::Box::new(gtk::Orientation::Vertical, 2);
        responses.set_visible(false);
        header.append(&responses);

        let banner = banner_grid();
        banner.add_css_class("remote-banner");
        add(&banner, &gtk::Image::from_icon_name("image-x-generic-symbolic"));
        let text = gtk::Label::new(Some("This message contains remote content"));
        text.set_hexpand(true);
        text.set_xalign(0.0);
        wrap(&text);
        add(&banner, &text);
        let banner_text = text.clone();
        let show_menu = gtk::MenuButton::builder().label("Show").build();
        let loosen = gtk::Button::builder().visible(false).build();
        add(&banner, &loosen);
        add(&banner, &show_menu);
        banner.set_visible(false);

        let verify_banner = banner_grid();
        verify_banner.add_css_class("verify-banner");
        add(&verify_banner, &gtk::Image::from_icon_name("dialog-warning-symbolic"));
        let verify_text = gtk::Label::new(None);
        verify_text.set_hexpand(true);
        verify_text.set_xalign(0.0);
        wrap(&verify_text);
        add(&verify_banner, &verify_text);
        verify_banner.set_visible(false);

        let crypto_banner = banner_grid();
        crypto_banner.add_css_class("crypto-banner");
        let crypto_icon = gtk::Image::new();
        add(&crypto_banner, &crypto_icon);
        let crypto_text = gtk::Label::new(None);
        crypto_text.set_hexpand(true);
        crypto_text.set_xalign(0.0);
        wrap(&crypto_text);
        add(&crypto_banner, &crypto_text);
        crypto_banner.set_visible(false);

        let key_banner = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        key_banner.add_css_class("remote-banner");
        key_banner.append(&gtk::Image::from_icon_name("salutations-key-symbolic"));
        let key_text = gtk::Label::new(None);
        key_text.set_hexpand(true);
        key_text.set_xalign(0.0);
        key_text.set_wrap(true);
        key_banner.append(&key_text);
        let key_ignore = gtk::Button::with_label("Ignore");
        let key_accept = gtk::Button::with_label("Import");
        key_accept.add_css_class("suggested-action");
        key_banner.append(&key_ignore);
        key_banner.append(&key_accept);
        key_banner.set_visible(false);
        {
            let kb = key_banner.downgrade();
            key_ignore.connect_clicked(move |_| {
                if let Some(kb) = kb.upgrade() {
                    kb.set_visible(false);
                }
            });
        }

        let tiles = crate::tile_grid::TileGrid::default();
        let show_all = gtk::Button::builder().css_classes(["flat"]).visible(false).build();
        let save_all = gtk::Button::builder().label("Save all…").css_classes(["flat"]).build();
        let below = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        below.append(&show_all);
        let gap = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        gap.set_hexpand(true);
        below.append(&gap);
        below.append(&save_all);
        let attachments_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        attachments_box.set_margin_start(18);
        attachments_box.set_margin_end(18);
        attachments_box.set_margin_bottom(12);
        attachments_box.append(&tiles);
        attachments_box.append(&below);
        attachments_box.set_visible(false);

        let compact_subject = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["heading"])
            .build();
        let compact_count = gtk::Label::builder()
            .halign(gtk::Align::End)
            .valign(gtk::Align::Start)
            .css_classes(["attach-count"])
            .build();
        let compact_attach = gtk::Overlay::builder()
            .child(&gtk::Image::from_icon_name("mail-attachment-symbolic"))
            .tooltip_text("Attachments")
            .visible(false)
            .build();
        compact_attach.add_overlay(&compact_count);
        compact_attach.add_css_class("compact-attach");
        let expand = gtk::Image::from_icon_name("pan-down-symbolic");
        let first = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        first.append(&compact_subject);
        first.append(&compact_attach);
        first.append(&expand);
        let compact_from = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let compact_date = gtk::Label::builder().css_classes(["dim-label"]).build();
        let second = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        second.append(&compact_from);
        second.append(&compact_date);
        let compact_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(18)
            .margin_end(18)
            .tooltip_text("Show all details")
            .build();
        compact_box.append(&first);
        compact_box.append(&second);
        // Changes at once, in step with the page's scrolling; an animated
        // height would move the message while it ran.
        let head = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::None)
            .vhomogeneous(false)
            .build();
        head.add_named(&header, Some("full"));
        head.add_named(&compact_box, Some("compact"));
        head.set_visible_child_name("full");
        // Hidden while compact. Plain boxes: a revealer without a sliding
        // transition kept its child's height while hidden.
        let reveal = |w: &gtk::Widget| {
            let b = gtk::Box::new(gtk::Orientation::Vertical, 0);
            b.append(w);
            b
        };
        let attach_rev = reveal(attachments_box.upcast_ref());
        let crypto_rev = reveal(crypto_banner.upcast_ref());
        let key_rev = reveal(key_banner.upcast_ref());
        let remote_rev = reveal(banner.upcast_ref());

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&head);
        content.append(&attach_rev);
        content.append(&crypto_rev);
        content.append(&verify_banner);
        content.append(&key_rev);
        content.append(&remote_rev);

        let empty = adw::StatusPage::builder()
            .icon_name("mail-read-symbolic")
            .title("No message selected")
            .build();
        let loading = adw::Spinner::new();
        loading.set_size_request(32, 32);
        loading.set_halign(gtk::Align::Center);
        loading.set_valign(gtk::Align::Center);
        let failed = adw::StatusPage::builder()
            .icon_name("dialog-warning-symbolic")
            .title("Message unavailable")
            .build();

        let widget = gtk::Stack::builder().vhomogeneous(false).build();
        widget.add_named(&empty, Some("empty"));
        widget.add_named(&loading, Some("loading"));
        widget.add_named(&failed, Some("failed"));
        let selected = adw::StatusPage::builder().icon_name("mail-read-symbolic").build();
        widget.add_named(&selected, Some("selected"));
        // Measured without a width, wrapping header labels claim a minimum
        // height far above the window's; in a scrolled window they get the
        // pane's width, and headers longer than 400 px scroll.
        // The web view is not in it: inside a scrolled window, touch
        // scrolling did not reach the page, and on a phone's GL renderer
        // the page was drawn over once it settled.
        let holder = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .propagate_natural_height(true)
            .max_content_height(400)
            .child(&content)
            .build();
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        column.append(&holder);
        column.append(&web);
        widget.add_named(&column, Some("message"));
        widget.set_visible_child_name("empty");

        let this = Rc::new(Self {
            widget,
            subject,
            avatar,
            avatar_stack,
            from,
            to,
            cc_title,
            cc,
            date,
            important,
            attachments_box,
            responses,
            tiles,
            show_all,
            save_all,
            banner,
            banner_text,
            verify_banner,
            verify_text,
            verify_button,
            show_menu,
            loosen,
            account_address: RefCell::new(None),
            actions: gio::SimpleActionGroup::new(),
            web,
            shown: RefCell::new(None),
            generation: Cell::new(0),
            loading: RefCell::new(None),
            on_shown: RefCell::new(None),
            links: RefCell::new(Vec::new()),
            gpg: RefCell::new(None),
            crypto_banner,
            crypto_icon,
            crypto_text,
            key_banner,
            key_text,
            key_accept,
            key_handler: RefCell::new(None),
            head,
            compact_subject,
            compact_from,
            compact_date,
            compact_attach,
            compact_count,
            extras: [attach_rev, key_rev, remote_rev],
            crypto_rev,
            compact: Cell::new(false),
            people: RefCell::new(Vec::new()),
            people_at: Cell::new((0, 0)),
            contact_ctx: RefCell::new(None),
        });
        for (which, l) in [&this.from, &this.to, &this.cc].into_iter().enumerate() {
            this.contact_links(l, which);
        }
        let tap = gtk::GestureClick::new();
        let weak = Rc::downgrade(&this);
        tap.connect_released(move |_, _, _, _| {
            if let Some(this) = weak.upgrade() {
                this.choose_compact(false);
            }
        });
        compact_box.add_controller(tap);
        let weak = Rc::downgrade(&this);
        collapse.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.choose_compact(true);
            }
        });
        this.install_actions();
        let weak = Rc::downgrade(&this);
        this.web.connect_decide_policy(move |_, decision, kind| {
            use webkit::PolicyDecisionType as T;
            if !matches!(kind, T::NavigationAction | T::NewWindowAction) {
                return false;
            }
            let Some(nav) = decision.downcast_ref::<webkit::NavigationPolicyDecision>() else {
                return false;
            };
            let Some(action) = nav.navigation_action() else { return false };
            let uri = action.request().and_then(|r| r.uri()).map(|u| u.to_string()).unwrap_or_default();
            // The document itself loads from load_html() as about:blank.
            if action.navigation_type() == webkit::NavigationType::Other && uri.starts_with("about:") {
                decision.use_();
                return true;
            }
            decision.ignore();
            if action.is_user_gesture()
                && let Some(this) = weak.upgrade()
                && let Some(link) = this.link_for(&uri)
            {
                this.open_link(&link);
            }
            true
        });
        let weak = Rc::downgrade(&this);
        this.web.connect_context_menu(move |_, menu, hit| {
            let Some(this) = weak.upgrade() else { return false };
            let Some(link) = hit.link_uri().and_then(|u| this.link_for(&u)) else { return false };
            // WebKit's link items would open or copy the rewritten href.
            menu.remove_all();
            for item in this.link_menu(&link) {
                menu.append(&item);
            }
            false
        });
        // The page's colours and fonts are fixed when it loads.
        let manager = adw::StyleManager::default();
        // The widgets' colours change only once GTK restyles them, after
        // these notifications; reloading at once would take the old ones.
        let restyled = |weak: std::rc::Weak<Self>| {
            glib::idle_add_local_full(glib::Priority::LOW, move || {
                if let Some(this) = weak.upgrade() {
                    this.display();
                }
                glib::ControlFlow::Break
            });
        };
        let weak = Rc::downgrade(&this);
        manager.connect_dark_notify(move |_| restyled(weak.clone()));
        let weak = Rc::downgrade(&this);
        manager.connect_accent_color_notify(move |_| restyled(weak.clone()));
        let weak = Rc::downgrade(&this);
        crate::clock::connect_changed(move || {
            if let Some(this) = weak.upgrade() {
                this.display();
            }
        });
        let weak = Rc::downgrade(&this);
        crate::preferences::connect_changed(move || {
            if let Some(this) = weak.upgrade() {
                this.display();
            }
        });
        this
    }

    /// Shows `lines` under the headers, or nothing for none.
    pub fn set_responses(&self, lines: &[String]) {
        while let Some(c) = self.responses.first_child() {
            self.responses.remove(&c);
        }
        for line in lines {
            let l = gtk::Label::builder().label(line).xalign(0.0).wrap(true).css_classes(["dimmed", "caption"]).build();
            self.responses.append(&l);
        }
        self.responses.set_visible(!lines.is_empty());
    }

    /// Called once a message has been rendered in the pane.
    pub fn connect_shown(&self, f: impl Fn(&camel::Folder, &str) + 'static) {
        *self.on_shown.borrow_mut() = Some(Rc::new(f));
    }

    /// The shown message, for replying and forwarding.
    pub fn message(&self) -> Option<camel::MimeMessage> {
        self.shown.borrow().as_ref()?;
        SHOWN.with_borrow(|m| m.clone())
    }

    /// The shown message has an HTML body.
    pub fn is_html(&self) -> bool {
        self.shown.borrow().as_ref().is_some_and(|s| !s.rendered.is_plain)
    }

    pub fn current(&self) -> Option<(camel::Folder, String)> {
        self.shown.borrow().as_ref().map(|s| (s.folder.clone(), s.uid.clone()))
    }

    /// The message being loaded, else the shown one: what message actions
    /// apply to. A key pressed while the next message loads must not act
    /// on the one before it.
    pub fn target(&self) -> Option<(camel::Folder, String)> {
        self.loading.borrow().clone().or_else(|| self.current())
    }

    /// Drops the current message without changing what is drawn, for a
    /// page that is being navigated away from; showing the message again
    /// reloads it and starts the read timer anew.
    pub fn forget(&self) {
        REMOTE_ALLOWED.set(false);
        self.generation.set(self.generation.get() + 1);
        self.loading.borrow_mut().take();
        *self.shown.borrow_mut() = None;
        SHOWN.with_borrow_mut(|m| *m = None);
        SHOWN_BODY.with_borrow_mut(|m| *m = None);
    }

    /// Selection mode: the pane shows how many messages are checked
    /// instead of a message, so no message is loaded or marked read.
    pub fn show_count(&self, n: usize) {
        self.forget();
        let title = match n {
            0 => "No message selected".to_string(),
            1 => "1 message selected".to_string(),
            n => format!("{n} messages selected"),
        };
        if let Some(page) = self.widget.child_by_name("selected").and_downcast::<adw::StatusPage>() {
            page.set_title(&title);
        }
        self.widget.set_visible_child_name("selected");
    }

    pub fn clear(&self) {
        REMOTE_ALLOWED.set(false);
        self.generation.set(self.generation.get() + 1);
        self.loading.borrow_mut().take();
        *self.shown.borrow_mut() = None;
        SHOWN.with_borrow_mut(|m| *m = None);
        SHOWN_BODY.with_borrow_mut(|m| *m = None);
        self.widget.set_visible_child_name("empty");
    }

    pub fn show(self: &Rc<Self>, folder: camel::Folder, uid: String) {
        if self.current().is_some_and(|(f, u)| f == folder && u == uid)
            || self.loading.borrow().as_ref().is_some_and(|(f, u)| *f == folder && *u == uid)
        {
            return;
        }
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.set_responses(&[]);
        self.key_banner.set_visible(false);
        *self.loading.borrow_mut() = Some((folder.clone(), uid.clone()));
        self.widget.set_visible_child_name("loading");
        let this = self.clone();
        glib::spawn_future_local(async move {
            let mut result = folder.message_future(&uid, glib::ffi::G_PRIORITY_DEFAULT).await;
            // IMAPX answers with no message and no error when its fetch job
            // ends without a result, which a fetch merged into an identical
            // one can do. Asking again gets the message.
            if matches!(result, Ok(None)) {
                result = folder.message_future(&uid, glib::ffi::G_PRIORITY_DEFAULT).await;
            }
            if this.generation.get() != generation {
                return;
            }
            let result = result.and_then(|m| {
                m.ok_or_else(|| glib::Error::new(gio::IOErrorEnum::NotFound, "The server returned no message"))
            });
            match result {
                Ok(msg) => {
                    // Decrypting runs gpg and can wait for a passphrase.
                    let gpg = this.gpg.borrow().clone();
                    let (body, protection) = match gpg {
                        Some(ctx) => {
                            let job = Handoff((ctx, msg.clone()));
                            let out = gio::spawn_blocking(move || {
                                let Handoff((ctx, msg)) = job;
                                Handoff(crypto::unprotect_sync(&ctx, &msg))
                            })
                            .await
                            .ok()
                            .and_then(|h| h.0);
                            match out {
                                Some((b, p)) => (Some(b), Some(p)),
                                None => (None, None),
                            }
                        }
                        None => (None, None),
                    };
                    if this.generation.get() != generation {
                        return;
                    }
                    let rendered = render::render_with(&msg, body.as_ref());
                    let attachments =
                        render::received_attachments(body.as_ref().unwrap_or(msg.upcast_ref()), &rendered.body_html);
                    // Loading until shown: actions in between apply to it.
                    this.loading.borrow_mut().take();
                    SHOWN.with_borrow_mut(|m| *m = Some(msg));
                    SHOWN_BODY.with_borrow_mut(|m| *m = body.clone());
                    *this.shown.borrow_mut() = Some(Shown {
                        folder: folder.clone(),
                        uid: uid.clone(),
                        rendered,
                        allow_once: false,
                        strip_once: None,
                        protection,
                        body,
                        attachments,
                    });
                    this.display();
                    let cb = this.on_shown.borrow().clone();
                    if let Some(cb) = cb {
                        cb(&folder, &uid);
                    }
                }
                Err(e) => {
                    this.loading.borrow_mut().take();
                    *this.shown.borrow_mut() = None;
                    if let Some(page) = this.widget.child_by_name("failed").and_downcast::<adw::StatusPage>() {
                        page.set_description(Some(e.message()));
                    }
                    this.widget.set_visible_child_name("failed");
                }
            }
        });
    }

    fn display(&self) {
        let shown = self.shown.borrow();
        let Some(s) = shown.as_ref() else { return };
        let r = &s.rendered;
        self.subject.set_text(if r.subject.is_empty() { "(No subject)" } else { &r.subject });
        self.show_protection(s.protection.as_ref());
        let sender = r.from.first();
        match sender {
            Some(a) => {
                let v = r.verification.as_ref();
                // A domain's logo only for mail that domain demonstrably sent.
                let verified = a.domain().filter(|d| {
                    v.is_some_and(|v| v.dmarc.as_deref() == Some("pass") && v.from_domain.as_deref().is_some_and(|f| f.eq_ignore_ascii_case(d)))
                });
                if let Some(face) = &r.face
                    && !v.is_some_and(|v| v.failed())
                {
                    crate::people::learn_face(&a.email, face);
                }
                let network = !crate::people::junk_shown();
                crate::people::bind(&self.avatar, a.name.as_deref().unwrap_or_default(), &a.email, network, verified.as_deref());
            }
            None => {
                self.avatar.set_text(None);
                self.avatar.set_custom_image(None::<&gtk::gdk::Paintable>);
            }
        }
        self.show_verification(r);
        let from_text = sender
            .map(|a| match &a.name {
                Some(n) => format!(
                    "<a href=\"0\"><b>{}</b>  {}</a>",
                    glib::markup_escape_text(n),
                    glib::markup_escape_text(&a.email)
                ),
                None => format!("<a href=\"0\"><b>{}</b></a>", glib::markup_escape_text(&a.email)),
            })
            .unwrap_or_default();
        self.from.set_markup(&from_text);
        let people: Vec<Address> = sender.into_iter().chain(&r.to).chain(&r.cc).cloned().collect();
        let first = sender.is_some() as usize;
        self.people_at.set((first, first + r.to.len()));
        self.to.set_markup(&name_links(&r.to, first));
        self.cc.set_markup(&name_links(&r.cc, first + r.to.len()));
        *self.people.borrow_mut() = people;
        self.cc.set_visible(!r.cc.is_empty());
        self.cc_title.set_visible(!r.cc.is_empty());
        let date = r
            .date
            .and_then(|d| glib::DateTime::from_unix_local(d).ok())
            .and_then(|d| d.format(&format!("%a, %b %e, %Y, {}", crate::clock::time_format())).ok())
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        self.date.set_text(&date);
        self.compact_subject.set_text(&self.subject.text());
        self.compact_from.set_text(&sender.map(Address::display).unwrap_or_default());
        self.compact_date.set_text(&date);
        self.set_compact(Settings::get().compact_headers);
        self.important.set_visible(r.important);
        self.show_attachments(&s.attachments);

        let settings = Settings::get();
        let by_domain = sender.and_then(Address::domain).is_some_and(|d| settings.remote_domains.contains(&d));
        let by_sender = sender.is_some_and(|a| settings.remote_senders.contains(&a.email.to_ascii_lowercase()));
        let by_message = s.allow_once || r.message_id.as_deref().is_some_and(salutations_core::allowed::contains);
        let allowed = by_domain || by_sender || by_message;
        // Only the rule that allows the content can be removed: the
        // broadest one, since removing a narrower one would change nothing.
        let enable = |name: &str, on: bool| {
            if let Some(a) = self.actions.lookup_action(name).and_downcast::<gio::SimpleAction>() {
                a.set_enabled(on);
            }
        };
        enable("unallow-domain", by_domain);
        enable("unallow-sender", by_sender && !by_domain);
        enable("unallow-once", by_message && !by_sender && !by_domain);
        self.banner.set_visible(r.has_remote);
        if allowed {
            self.banner_text.set_text("Remote content is allowed for this message");
            self.show_menu.set_label("Remove");
            self.show_menu.set_menu_model(Some(&unallow_menu(sender)));
        } else {
            self.banner_text.set_text("This message contains remote content");
            self.show_menu.set_label("Show");
            self.show_menu.set_menu_model(Some(&remote_menu(sender)));
        }

        let style = page_style(&self.widget, r);
        // Theme-following pages sit on the window's colour. Not a
        // transparent view: a phone's GL renderer drew those blank.
        let canvas = if r.follows_theme { window_color(&self.widget) } else { gtk::gdk::RGBA::WHITE };
        self.web.set_background_color(&canvas);
        let csp = if allowed { CSP_ALLOWED } else { CSP_BLOCKED };
        REMOTE_ALLOWED.set(allowed);
        let address = self.account_address.borrow().as_ref().and_then(|f| f(&s.folder));
        STRIP.set((if allowed { self.strip_level() } else { remote::Strip::Nothing }, address));
        PAGE.set(PAGE.get() + 1);
        STRIP_FAILED.set((false, false));
        self.loosen.set_visible(false);
        self.show_remote_extra();
        let body = if allowed && r.has_remote { remote::rewrite(&r.body_html) } else { r.body_html.clone() };
        let (body, found) = links::rewrite(&links::linkify(&body));
        *self.links.borrow_mut() = found;
        let body = with_print_header(&body, &print_header(r, &date, &s.attachments));
        let doc = format!("<meta http-equiv=\"Content-Security-Policy\" content=\"{csp}\">{style}{body}");
        self.web.load_html(&doc, None);
        self.widget.set_visible_child_name("message");
    }

    /// A line for encrypted mail and good signatures, a warning for an
    /// uncertified or missing key, an error for a bad signature or a
    /// message that cannot be decrypted.
    fn show_protection(&self, p: Option<&crypto::Protection>) {
        use crypto::SignStatus as S;
        for c in ["error", "warning"] {
            self.crypto_banner.remove_css_class(c);
        }
        let Some(p) = p else {
            self.crypto_banner.set_visible(false);
            return;
        };
        let who = |s: &crypto::Signature| {
            let signer = s.signer.as_ref();
            let name = signer
                .and_then(|x| x.name.clone().or_else(|| x.email.clone()))
                .unwrap_or_else(|| "an unknown signer".into());
            match &s.key_id {
                Some(k) => format!("{name}, key {}", short_key(k)),
                None => name,
            }
        };
        let (icon, text, class) = if let Some(e) = &p.decrypt_error {
            ("dialog-warning-symbolic", format!("This message cannot be decrypted: {e}"), Some("error"))
        } else {
            let lock = if p.encrypted { "Encrypted" } else { "" };
            match &p.signature {
                Some(s) if s.status == S::Bad => (
                    "dialog-warning-symbolic",
                    format!("The signature on this message is not valid{}", s.error.as_ref().map(|e| format!(": {e}")).unwrap_or_default()),
                    Some("error"),
                ),
                Some(s) if s.status == S::NeedPublicKey => (
                    "dialog-warning-symbolic",
                    format!("{}Signed with a key that is not in your keyring", if p.encrypted { "Encrypted. " } else { "" }),
                    Some("warning"),
                ),
                Some(s) if s.status == S::Unknown => (
                    "dialog-warning-symbolic",
                    format!("{}Signed by {}, a key you have not verified", if p.encrypted { "Encrypted. " } else { "" }, who(s)),
                    Some("warning"),
                ),
                Some(s) => (
                    if p.encrypted { "channel-secure-symbolic" } else { "application-certificate-symbolic" },
                    format!("{}Signed by {}", if p.encrypted { "Encrypted. " } else { "" }, who(s)),
                    None,
                ),
                None if p.encrypted => ("channel-secure-symbolic", lock.to_string(), None),
                None => {
                    self.crypto_banner.set_visible(false);
                    return;
                }
            }
        };
        self.crypto_icon.set_icon_name(Some(icon));
        self.crypto_text.set_label(&text);
        self.crypto_text.set_tooltip_text(Some(&text));
        if let Some(c) = class {
            self.crypto_banner.add_css_class(c);
        }
        self.crypto_banner.set_visible(true);
        self.crypto_rev.set_visible(!self.compact.get() || self.crypto_warns());
    }

    /// The contact menu: New message, Add to contacts… or Show in
    /// Contacts, Copy address.
    pub fn set_contact_ctx(&self, ctx: Rc<crate::contact_menu::Ctx>) {
        *self.contact_ctx.borrow_mut() = Some(ctx);
    }

    /// A person in the headers opens the contact menu at the click, tap,
    /// right click or long press. `which` is 0 for From, 1 for To, 2 for Cc.
    fn contact_links(self: &Rc<Self>, label: &gtk::Label, which: usize) {
        let at: Rc<Cell<Option<(f64, f64)>>> = Rc::default();
        // A touch on a person: who, and where it went down; and whether the
        // long press already opened the menu.
        let touch: Rc<RefCell<Option<(Address, f64, f64)>>> = Rc::default();
        let fired = Rc::new(Cell::new(false));
        let click = gtk::GestureClick::new();
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let (a, weak, t, f) = (at.clone(), Rc::downgrade(self), touch.clone(), fired.clone());
        click.connect_pressed(move |g, _, x, y| {
            a.set(Some((x, y)));
            t.replace(None);
            let on_touch = g.current_event_device().is_some_and(|d| d.source() == gtk::gdk::InputSource::Touchscreen);
            let (Some(this), Some(label)) = (weak.upgrade(), g.widget().and_downcast::<gtk::Label>()) else { return };
            if !on_touch {
                return;
            }
            let Some(p) = this.person_at(&label, which, x, y) else { return };
            // A link in a label takes the touch on press to open on release,
            // which cancels any long press; claiming it first keeps the long
            // press below, grouped with this gesture, alive.
            g.set_state(gtk::EventSequenceState::Claimed);
            f.set(false);
            t.replace(Some((p, x, y)));
        });
        let (weak, t, f) = (Rc::downgrade(self), touch.clone(), fired.clone());
        click.connect_released(move |g, _, x, y| {
            let Some((p, x0, y0)) = t.take() else { return };
            // "stopped" is no guide: it also comes when a press outlasts the
            // double-click time. A tap is a release near where it began.
            let slop = 16.0;
            if f.get() || (x - x0).abs() > slop || (y - y0).abs() > slop {
                return;
            }
            let (Some(this), Some(label)) = (weak.upgrade(), g.widget().and_downcast::<gtk::Label>()) else { return };
            this.contact_menu(&label, Some(gtk::gdk::Rectangle::new(x0 as i32, y0 as i32, 1, 1)), &p);
        });
        let long = gtk::GestureLongPress::builder().touch_only(true).build();
        long.set_propagation_phase(gtk::PropagationPhase::Capture);
        let (weak, t, f) = (Rc::downgrade(self), touch.clone(), fired.clone());
        long.connect_pressed(move |g, _, _| {
            let Some((p, x, y)) = t.borrow().clone() else { return };
            let (Some(this), Some(label)) = (weak.upgrade(), g.widget().and_downcast::<gtk::Label>()) else { return };
            f.set(true);
            this.contact_menu(&label, Some(gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)), &p);
        });
        // A finger that moved too far cancels the long press itself, and
        // the release is too far away to be a tap. "cancelled" also comes
        // for a tap released early, so it clears nothing.
        label.add_controller(long.clone());
        label.add_controller(click.clone());
        long.group_with(&click);
        let weak = Rc::downgrade(self);
        label.connect_activate_link(move |label, uri| {
            let Some(this) = weak.upgrade() else { return glib::Propagation::Stop };
            let person = uri.parse::<usize>().ok().and_then(|i| this.people.borrow().get(i).cloned());
            // A keyboard activation has no click to point at.
            let rect = at.take().map(|(x, y)| gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1));
            if let Some(p) = person {
                this.contact_menu(label, rect, &p);
            }
            glib::Propagation::Stop
        });
        // Claimed before the label's menu (Copy link address).
        let menu_click = gtk::GestureClick::builder().button(gtk::gdk::BUTTON_SECONDARY).build();
        menu_click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        menu_click.connect_pressed(move |g, _, x, y| {
            let Some(this) = weak.upgrade() else { return };
            let Some(label) = g.widget().and_downcast::<gtk::Label>() else { return };
            let Some(p) = this.person_at(&label, which, x, y) else { return };
            g.set_state(gtk::EventSequenceState::Claimed);
            this.contact_menu(&label, Some(gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)), &p);
        });
        label.add_controller(menu_click);
    }

    fn contact_menu(&self, label: &gtk::Label, at: Option<gtk::gdk::Rectangle>, p: &Address) {
        let ctx = self.contact_ctx.borrow().clone();
        if let Some(ctx) = ctx {
            crate::contact_menu::popup(&ctx, label, at, p.name.clone(), p.email.clone(), true);
        }
    }

    /// The person whose name is at (`x`, `y`) in header label `which`.
    fn person_at(&self, label: &gtk::Label, which: usize, x: f64, y: f64) -> Option<Address> {
        let layout = label.layout();
        let (ox, oy) = label.layout_offsets();
        let scale = gtk::pango::SCALE as f64;
        let (inside, index, _) = layout.xy_to_index(((x - ox as f64) * scale) as i32, ((y - oy as f64) * scale) as i32);
        if !inside {
            return None;
        }
        let index = index as usize;
        let people = self.people.borrow();
        let (to_at, cc_at) = self.people_at.get();
        // From is one link, the whole line.
        let range = match which {
            0 => return (to_at > 0).then(|| people[0].clone()),
            1 => to_at..cc_at,
            _ => cc_at..people.len(),
        };
        // To and Cc are the names joined by ", ", as in name_links.
        let mut start = 0;
        for p in &people[range] {
            let end = start + p.display().len();
            if (start..end).contains(&index) {
                return Some(p.clone());
            }
            start = end + 2;
        }
        None
    }

    /// The user's choice of header, kept for later messages.
    fn choose_compact(&self, on: bool) {
        self.set_compact(on);
        let mut settings = Settings::get();
        if settings.compact_headers != on {
            settings.compact_headers = on;
            let _ = settings.save();
        }
    }

    fn crypto_warns(&self) -> bool {
        self.crypto_banner.has_css_class("error") || self.crypto_banner.has_css_class("warning")
    }

    /// Shows the two-line header, with only the warnings among the
    /// banners, or the full one.
    fn set_compact(&self, on: bool) {
        self.compact.set(on);
        self.head.set_visible_child_name(if on { "compact" } else { "full" });
        for r in &self.extras {
            r.set_visible(!on);
        }
        self.crypto_rev.set_visible(!on || self.crypto_warns());
        self.show_remote_extra();
    }

    /// Compact headers hide the remote-content banner, except while it
    /// offers to loosen tracking removal: without it, failed content would
    /// fail silently.
    fn show_remote_extra(&self) {
        // get_visible: is_visible would be false while the banner is hidden.
        self.extras[2].set_visible(!self.compact.get() || self.loosen.get_visible());
    }

    /// Tracking removal looks up the receiving account's address with `f`.
    pub fn set_account_address(&self, f: impl Fn(&camel::Folder) -> Option<String> + 'static) {
        *self.account_address.borrow_mut() = Some(Box::new(f));
    }

    /// How much tracking removal the shown message gets.
    fn strip_level(&self) -> remote::Strip {
        let shown = self.shown.borrow();
        let Some(s) = shown.as_ref() else { return remote::Strip::Nothing };
        if !Settings::get().remote_strip {
            return remote::Strip::Nothing;
        }
        match (&s.rendered.message_id, s.strip_once) {
            (Some(id), _) => salutations_core::allowed::strip_level(id),
            (None, Some(level)) => level,
            (None, None) => remote::Strip::All,
        }
    }

    /// The banner after loads failed because of tracking removal: first
    /// offering tracking codes back, then the address.
    fn update_strip_banner(&self) {
        use remote::Strip;
        let (tracking, address) = STRIP_FAILED.get();
        let level = self.strip_level();
        let offer = match level {
            Strip::All if tracking => Some(("Removed tracking stopped some content from loading", "Allow tracking codes")),
            Strip::All | Strip::Address if address => {
                Some(("Some content only loads if the sender can see your address", "Load anyway"))
            }
            _ => None,
        };
        match offer {
            Some((text, button)) => {
                self.banner_text.set_text(text);
                self.loosen.set_label(button);
                self.loosen.set_visible(true);
            }
            None => self.loosen.set_visible(false),
        }
        self.show_remote_extra();
    }

    /// Takes the next step of loosening and loads the message again.
    fn loosen_strip(&self) {
        use remote::Strip;
        let (tracking, _) = STRIP_FAILED.get();
        let next = match self.strip_level() {
            Strip::All if tracking => Strip::Address,
            _ => Strip::Nothing,
        };
        if let Some(s) = self.shown.borrow_mut().as_mut() {
            match &s.rendered.message_id {
                Some(id) => salutations_core::allowed::set_strip_level(id, next),
                None => s.strip_once = Some(next),
            }
        }
        self.display();
    }

    /// Decrypting and verifying use `ctx`.
    pub fn set_gpg(&self, ctx: camel::GpgContext) {
        *self.gpg.borrow_mut() = Some(ctx);
    }

    /// The shown message's protection, when it had any.
    pub fn protection(&self) -> Option<crypto::Protection> {
        self.shown.borrow().as_ref().and_then(|s| s.protection.clone())
    }

    /// The decrypted or verified content shown in place of the message's.
    pub fn body(&self) -> Option<camel::MimePart> {
        self.shown.borrow().as_ref().and_then(|s| s.body.clone())
    }

    /// Offers a key with a banner; `accept` runs on its button, which says
    /// `accept_label`. Any earlier offer is replaced.
    pub fn offer_key(&self, text: &str, accept_label: &str, accept: impl Fn() + 'static) {
        if let Some(h) = self.key_handler.borrow_mut().take() {
            self.key_accept.disconnect(h);
        }
        self.key_text.set_label(text);
        self.key_accept.set_label(accept_label);
        let kb = self.key_banner.downgrade();
        let h = self.key_accept.connect_clicked(move |_| {
            if let Some(kb) = kb.upgrade() {
                kb.set_visible(false);
            }
            accept();
        });
        *self.key_handler.borrow_mut() = Some(h);
        self.key_banner.set_visible(true);
    }

    /// Flags a DMARC failure with a banner and a warning in place of the
    /// avatar; a pass shows nothing, since a lookalike domain passes for
    /// itself. The details button lists what the provider checked.
    fn show_verification(&self, r: &Rendered) {
        let v = r.verification.as_ref();
        let failed = v.is_some_and(|v| v.failed());
        let domain = v
            .and_then(|v| v.from_domain.clone())
            .or_else(|| r.from.first().and_then(Address::domain))
            .unwrap_or_default();
        if failed {
            let text = format!("This message may not be from {domain}");
            self.verify_text.set_label(&text);
            self.verify_text.set_tooltip_text(Some(&format!(
                "{text}. It failed the verification that {domain} publishes for its mail."
            )));
            self.avatar_stack.set_visible_child_name("warning");
        } else {
            self.avatar_stack.set_visible_child_name("avatar");
        }
        self.verify_banner.set_visible(failed);
        self.verify_button.set_visible(v.is_some());
        let Some(v) = v else { return };
        let grid = gtk::Grid::builder()
            .column_spacing(12)
            .row_spacing(6)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();
        let rows = [
            ("Signed by", if v.signed_by.is_empty() { "No valid signature".to_string() } else { v.signed_by.join(", ") }),
            ("Mailed by", v.mailed_by.clone().unwrap_or_else(|| "Not verified".into())),
            (
                "DMARC",
                match v.dmarc.as_deref() {
                    Some("pass") => "Passed".to_string(),
                    Some("fail") => "Failed".to_string(),
                    Some(other) => other.to_string(),
                    None => "Not checked".to_string(),
                },
            ),
        ];
        for (i, (field, value)) in rows.into_iter().enumerate() {
            let f = gtk::Label::builder().label(field).xalign(0.0).css_classes(["dimmed"]).build();
            let v = gtk::Label::builder().label(value).xalign(0.0).build();
            grid.attach(&f, 0, i as i32, 1, 1);
            grid.attach(&v, 1, i as i32, 1, 1);
        }
        self.verify_button.set_popover(Some(&gtk::Popover::builder().child(&grid).build()));
    }

    /// The anchor behind a clicked URI. Anchors the rewrite missed arrive
    /// with their original href and no text.
    fn link_for(&self, uri: &str) -> Option<Link> {
        if uri.is_empty() {
            return None;
        }
        match uri.strip_prefix(links::SCHEME).and_then(|r| r.strip_prefix(':')) {
            Some(n) => self.links.borrow().get(n.parse::<usize>().ok()?).cloned(),
            None => Some(Link { href: uri.to_string(), text: String::new() }),
        }
    }

    fn launch(&self, uri: &str) {
        let window = self.widget.root().and_downcast::<gtk::Window>();
        gtk::UriLauncher::new(uri).launch(window.as_ref(), None::<&gio::Cancellable>, |_| {});
    }

    /// Opens a clicked link, asking first when the preference says so or
    /// when the link's text names a different site.
    fn open_link(self: &Rc<Self>, link: &Link) {
        if link.href.starts_with("mailto:") {
            self.launch(&link.href);
            return;
        }
        let settings = Settings::get();
        let tracking = links::tracking(&link.href);
        let shown = links::mismatch(link);
        if shown.is_none() && !settings.confirm_links {
            let strip = settings.remove_tracking && !tracking.is_empty();
            self.launch(&if strip { tracking.cleaned() } else { link.href.clone() });
            return;
        }
        if LINK_DIALOG.replace(true) {
            return;
        }
        let (heading, intro) = match &shown {
            Some(shown) => (
                "Link doesn’t go where it says",
                format!("The link text shows <b>{}</b>, but the link opens\n\n", glib::markup_escape_text(shown)),
            ),
            None => ("Open link?", String::new()),
        };
        let body = {
            let (href, tracking) = (link.href.clone(), tracking.clone());
            move |strip: bool| {
                let address = match (&tracking.redirect, strip) {
                    (Some(host), true) => format!(
                        "{}\n\nSkips the redirect through {}",
                        uri_markup(&tracking.target, &tracking.removed),
                        glib::markup_escape_text(host)
                    ),
                    (None, true) => uri_markup(&tracking.target, &tracking.removed),
                    (_, false) => uri_markup(&href, &[]),
                };
                format!("{intro}{address}")
            }
        };
        let dialog = adw::AlertDialog::builder()
            .heading(heading)
            .body(body(settings.remove_tracking && !tracking.is_empty()))
            .body_use_markup(true)
            .build();
        let check = gtk::CheckButton::builder()
            .label("_Remove site tracking")
            .use_underline(true)
            .active(settings.remove_tracking)
            .halign(gtk::Align::Center)
            .sensitive(!tracking.is_empty())
            .build();
        if tracking.is_empty() {
            check.set_tooltip_text(Some("This link has no known tracking to remove"));
        }
        let d = dialog.downgrade();
        check.connect_toggled(move |c| {
            if let Some(d) = d.upgrade() {
                d.set_body(&body(c.is_active()));
            }
        });
        dialog.set_extra_child(Some(&check));
        dialog.add_response("cancel", "_Cancel");
        dialog.add_response("open", "_Open");
        dialog.set_close_response("cancel");
        if shown.is_some() {
            dialog.set_response_appearance("open", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
        } else {
            dialog.set_response_appearance("open", adw::ResponseAppearance::Suggested);
            dialog.set_default_response(Some("open"));
        }
        let weak = Rc::downgrade(self);
        let href = link.href.clone();
        dialog.choose(Some(&self.widget), None::<&gio::Cancellable>, move |response| {
            LINK_DIALOG.set(false);
            if response != "open" {
                return;
            }
            let strip = check.is_active();
            if !tracking.is_empty() && strip != Settings::get().remove_tracking {
                let mut s = Settings::get();
                s.remove_tracking = strip;
                if let Err(e) = s.save() {
                    glib::g_warning!("salutations", "Could not save settings: {}", e);
                }
            }
            if let Some(this) = weak.upgrade() {
                this.launch(&if strip && !tracking.is_empty() { tracking.cleaned() } else { href });
            }
        });
    }

    fn link_menu(self: &Rc<Self>, link: &Link) -> Vec<webkit::ContextMenuItem> {
        let clean = links::strip_tracking(&link.href);
        let mut items = Vec::new();
        let mut add = |label: &str, f: LinkAction| {
            let action = gio::SimpleAction::new("link", None);
            let weak = Rc::downgrade(self);
            action.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    f(&this);
                }
            });
            items.push(webkit::ContextMenuItem::from_gaction(&action, label, None));
        };
        let l = link.clone();
        add("_Open link", Box::new(move |this| this.open_link(&l)));
        let h = link.href.clone();
        add("_Copy link", Box::new(move |this| this.widget.clipboard().set_text(&h)));
        if clean != link.href {
            add("Copy link without _tracking", Box::new(move |this| this.widget.clipboard().set_text(&clean)));
        }
        items
    }

    fn show_attachments(&self, list: &[salutations_core::compose::Attachment]) {
        let mut tiles = Vec::new();
        for (i, a) in list.iter().enumerate() {
            let tile = gtk::Box::new(gtk::Orientation::Horizontal, 2);
            tile.add_css_class("attachment-tile");
            let size = glib::format_size(a.data.len() as u64);
            let open = gtk::Button::builder()
                .child(&crate::tile_grid::file_label(&a.name, Some(&size), &a.mime_type))
                .action_name("viewer.attachment-open")
                .action_target(&(i as u32).to_variant())
                .tooltip_text(format!("Open {}", a.name))
                .css_classes(["flat"])
                .build();
            tile.append(&open);
            let menu = gio::Menu::new();
            let item = |label: &str, action: &str| {
                let it = gio::MenuItem::new(Some(label), None);
                it.set_action_and_target_value(Some(action), Some(&(i as u32).to_variant()));
                it
            };
            menu.append_item(&item("_Open", "viewer.attachment-open"));
            menu.append_item(&item("_Save as…", "viewer.attachment-save"));
            tile.append(
                &gtk::MenuButton::builder()
                    .icon_name("view-more-symbolic")
                    .menu_model(&menu)
                    .valign(gtk::Align::Center)
                    .tooltip_text("More")
                    .css_classes(["flat", "circular"])
                    .build(),
            );
            tiles.push(tile.upcast());
        }
        self.attachments_box.set_visible(!tiles.is_empty());
        self.compact_attach.set_visible(!tiles.is_empty());
        self.compact_count.set_text(&tiles.len().to_string());
        self.save_all.set_visible(list.len() > 1);
        self.tiles.set_expanded(false);
        self.tiles.set_tiles(tiles);
    }

    fn attachment(&self, index: u32) -> Option<salutations_core::compose::Attachment> {
        self.shown.borrow().as_ref()?.attachments.get(index as usize).cloned()
    }

    /// Writes the attachment to a private temporary folder and hands it to
    /// the default app (through the portal in the sandbox).
    fn open_attachment(&self, index: u32) {
        let Some(a) = self.attachment(index) else { return };
        let dir = attachments_dir().join(glib::uuid_string_random().as_str());
        let path = dir.join(safe_name(&a.name));
        let written = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, &a.data[..]));
        let parent = self.widget.root().and_downcast::<gtk::Window>();
        if let Err(e) = written {
            self.attachment_error(&format!("Cannot open {}: {e}", a.name));
            return;
        }
        let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&path)));
        let weak = self.widget.downgrade();
        launcher.launch(parent.as_ref(), None::<&gio::Cancellable>, move |result| {
            if let (Err(e), Some(w)) = (result, weak.upgrade())
                && !e.matches(gtk::DialogError::Dismissed)
            {
                error_dialog(&w, &format!("Cannot open {}: {}", a.name, e.message()));
            }
        });
    }

    /// Opens the print dialog for the shown message; its Print to File
    /// makes a PDF.
    pub fn print(&self) {
        let subject = match self.shown.borrow().as_ref() {
            Some(s) => s.rendered.subject.trim().to_string(),
            None => return,
        };
        let op = webkit::PrintOperation::new(&self.web);
        let settings = PRINT_SETTINGS.with_borrow(|s| s.as_ref().map(|s| s.copy())).unwrap_or_default();
        // Print to File names the PDF after the message.
        let name = safe_name(if subject.is_empty() { "Message" } else { &subject });
        settings.set(gtk::PRINT_SETTINGS_OUTPUT_BASENAME, Some(&name));
        op.set_print_settings(&settings);
        let weak = self.widget.downgrade();
        op.connect_failed(move |_, e| {
            if let Some(w) = weak.upgrade() {
                error_dialog(&w, &format!("Cannot print: {}", e.message()));
            }
        });
        let parent = self.widget.root().and_downcast::<gtk::Window>();
        if op.run_dialog(parent.as_ref()) == webkit::PrintOperationResponse::Print {
            PRINT_SETTINGS.set(op.print_settings());
        }
    }

    /// Saves the shown message as it arrived, as an .eml file.
    pub fn save_message(&self) {
        let subject = match self.shown.borrow().as_ref() {
            Some(s) => s.rendered.subject.trim().to_string(),
            None => return,
        };
        let Some(msg) = self.message() else { return };
        let stream = gio::MemoryOutputStream::new_resizable();
        if let Err(e) = msg.write_to_output_stream_sync(&stream, None::<&gio::Cancellable>) {
            return error_dialog(&self.widget, &format!("Cannot save the message: {}", e.message()));
        }
        let _ = stream.close(None::<&gio::Cancellable>);
        let bytes = glib::Bytes::from_owned(without_local_headers(&stream.steal_as_bytes()));
        let name = format!("{}.eml", safe_name(if subject.is_empty() { "Message" } else { &subject }));
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Email messages"));
        filter.add_mime_type("message/rfc822");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog =
            gtk::FileDialog::builder().title("Save message").initial_name(name).filters(&filters).modal(true).build();
        let parent = self.widget.root().and_downcast::<gtk::Window>();
        let weak = self.widget.downgrade();
        dialog.save(parent.as_ref(), None::<&gio::Cancellable>, move |result| {
            let Ok(file) = result else { return };
            let (weak, bytes) = (weak.clone(), bytes.clone());
            glib::spawn_future_local(async move {
                if let Err(e) = file
                    .replace_contents_future(bytes, None, false, gio::FileCreateFlags::REPLACE_DESTINATION)
                    .await
                    && let Some(w) = weak.upgrade()
                {
                    error_dialog(&w, &format!("Cannot save the message: {}", e.1.message()));
                }
            });
        });
    }

    fn save_attachment(&self, index: u32) {
        let Some(a) = self.attachment(index) else { return };
        let dialog = gtk::FileDialog::builder().title("Save attachment").initial_name(safe_name(&a.name)).modal(true).build();
        let parent = self.widget.root().and_downcast::<gtk::Window>();
        let weak = self.widget.downgrade();
        dialog.save(parent.as_ref(), None::<&gio::Cancellable>, move |result| {
            let Ok(file) = result else { return };
            let weak = weak.clone();
            glib::spawn_future_local(async move {
                let bytes = a.data.clone();
                if let Err(e) = file
                    .replace_contents_future(bytes, None, false, gio::FileCreateFlags::REPLACE_DESTINATION)
                    .await
                    && let Some(w) = weak.upgrade()
                {
                    error_dialog(&w, &format!("Cannot save {}: {}", a.name, e.1.message()));
                }
            });
        });
    }

    fn save_all(&self) {
        let list = match self.shown.borrow().as_ref() {
            Some(s) => s.attachments.clone(),
            None => return,
        };
        let dialog = gtk::FileDialog::builder().title("Save attachments to").modal(true).build();
        let parent = self.widget.root().and_downcast::<gtk::Window>();
        let weak = self.widget.downgrade();
        dialog.select_folder(parent.as_ref(), None::<&gio::Cancellable>, move |result| {
            let Ok(folder) = result else { return };
            let weak = weak.clone();
            glib::spawn_future_local(async move {
                let mut used = std::collections::HashSet::new();
                for a in list {
                    // Existing files are kept: a taken name gets a number.
                    let name = free_name(&folder, &safe_name(&a.name), &mut used);
                    let file = folder.child(&name);
                    if let Err(e) = file
                        .replace_contents_future(a.data.clone(), None, false, gio::FileCreateFlags::NONE)
                        .await
                        && let Some(w) = weak.upgrade()
                    {
                        error_dialog(&w, &format!("Cannot save {}: {}", a.name, e.1.message()));
                        return;
                    }
                }
            });
        });
    }

    fn attachment_error(&self, text: &str) {
        error_dialog(&self.widget, text);
    }

    fn install_actions(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.loosen.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.loosen_strip();
            }
        });
        let weak = Rc::downgrade(self);
        ON_STRIP_FAILED.set(Some(Box::new(move || {
            if let Some(this) = weak.upgrade() {
                this.update_strip_banner();
            }
        })));
        let group = self.actions.clone();
        let weak = Rc::downgrade(self);
        let once = gio::SimpleAction::new("remote-once", None);
        once.connect_activate(move |_, _| {
            if let Some(this) = weak.upgrade() {
                // Kept by Message-ID; a message without one is allowed only
                // while it stays selected.
                if let Some(s) = this.shown.borrow_mut().as_mut() {
                    match &s.rendered.message_id {
                        Some(id) => salutations_core::allowed::add(id),
                        None => s.allow_once = true,
                    }
                }
                this.display();
            }
        });
        group.add_action(&once);
        for (name, by_domain) in [("remote-sender", false), ("remote-domain", true)] {
            let weak = Rc::downgrade(self);
            let a = gio::SimpleAction::new(name, None);
            a.connect_activate(move |_, _| {
                let Some(this) = weak.upgrade() else { return };
                let sender = this.shown.borrow().as_ref().and_then(|s| s.rendered.from.first().cloned());
                let Some(sender) = sender else { return };
                let mut settings = Settings::get();
                if by_domain {
                    if let Some(d) = sender.domain().filter(|d| !settings.remote_domains.contains(d)) {
                        settings.remote_domains.push(d);
                    }
                } else {
                    let e = sender.email.to_ascii_lowercase();
                    if !settings.remote_senders.contains(&e) {
                        settings.remote_senders.push(e);
                    }
                }
                if let Err(e) = settings.save() {
                    glib::g_warning!("salutations", "Could not save settings: {}", e);
                }
                this.display();
            });
            group.add_action(&a);
        }
        let weak = Rc::downgrade(self);
        let a = gio::SimpleAction::new("unallow-once", None);
        a.connect_activate(move |_, _| {
            let Some(this) = weak.upgrade() else { return };
            if let Some(s) = this.shown.borrow_mut().as_mut() {
                s.allow_once = false;
                if let Some(id) = &s.rendered.message_id {
                    salutations_core::allowed::remove(id);
                }
            }
            this.display();
        });
        group.add_action(&a);
        for (name, by_domain) in [("unallow-sender", false), ("unallow-domain", true)] {
            let weak = Rc::downgrade(self);
            let a = gio::SimpleAction::new(name, None);
            a.connect_activate(move |_, _| {
                let Some(this) = weak.upgrade() else { return };
                let sender = this.shown.borrow().as_ref().and_then(|s| s.rendered.from.first().cloned());
                let Some(sender) = sender else { return };
                let mut settings = Settings::get();
                if by_domain {
                    let d = sender.domain();
                    settings.remote_domains.retain(|x| Some(x) != d.as_ref());
                } else {
                    let e = sender.email.to_ascii_lowercase();
                    settings.remote_senders.retain(|x| *x != e);
                }
                if let Err(e) = settings.save() {
                    glib::g_warning!("salutations", "Could not save settings: {}", e);
                }
                this.display();
            });
            group.add_action(&a);
        }
        for (name, save) in [("attachment-open", false), ("attachment-save", true)] {
            let weak = Rc::downgrade(self);
            let a = gio::SimpleAction::new(name, Some(glib::VariantTy::UINT32));
            a.connect_activate(move |_, v| {
                let (Some(this), Some(i)) = (weak.upgrade(), v.and_then(|v| v.get::<u32>())) else { return };
                if save {
                    this.save_attachment(i);
                } else {
                    this.open_attachment(i);
                }
            });
            group.add_action(&a);
        }
        let weak = Rc::downgrade(self);
        self.save_all.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.save_all();
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
            this.show_all.set_label(&if hidden > 0 { format!("Show all ({hidden} more)") } else { "Show fewer".into() });
        });
        self.widget.insert_action_group("viewer", Some(&group));
    }
}

/// `uri` as Pango markup: its registrable domain in bold, so
/// "chase.com.evil.example" reads as evil.example, and the byte ranges in
/// `struck` crossed out and dimmed. Long addresses are cut.
fn uri_markup(uri: &str, struck: &[std::ops::Range<usize>]) -> String {
    const MAX: usize = 240;
    let bold = links::host(uri).and_then(|host| {
        let base = links::base_domain(&host);
        let end = uri.to_ascii_lowercase().find(&host)? + host.len();
        Some(end - base.len()..end)
    });
    let mut out = String::new();
    let mut run = String::new();
    let mut style = (false, false);
    let flush = |out: &mut String, run: &mut String, (b, s): (bool, bool)| {
        if run.is_empty() {
            return;
        }
        let text = glib::markup_escape_text(run);
        match (b, s) {
            (false, false) => out.push_str(&text),
            (true, false) => out.push_str(&format!("<b>{text}</b>")),
            (b, true) => out.push_str(&format!(
                "<span strikethrough=\"true\" alpha=\"55%\"{}>{text}</span>",
                if b { " weight=\"bold\"" } else { "" }
            )),
        }
        run.clear();
    };
    for (n, (i, c)) in uri.char_indices().enumerate() {
        if n == MAX {
            flush(&mut out, &mut run, style);
            out.push('…');
            return out;
        }
        let now = (
            bold.as_ref().is_some_and(|r| r.contains(&i)),
            struck.iter().any(|r| r.contains(&i)),
        );
        if now != style {
            flush(&mut out, &mut run, style);
            style = now;
        }
        run.push(c);
    }
    flush(&mut out, &mut run, style);
    out
}

/// The window background behind the message pane, opaque.
fn window_color(widget: &impl IsA<gtk::Widget>) -> gtk::gdk::RGBA {
    #[allow(deprecated)]
    let named = widget.as_ref().style_context().lookup_color("window_bg_color");
    let dark = adw::StyleManager::default().is_dark();
    let fallback = if dark { gtk::gdk::RGBA::new(0.133, 0.133, 0.149, 1.0) } else { gtk::gdk::RGBA::new(0.98, 0.98, 0.98, 1.0) };
    let c = named.unwrap_or(fallback);
    gtk::gdk::RGBA::new(c.red(), c.green(), c.blue(), 1.0)
}

fn css_rgb(c: &gtk::gdk::RGBA) -> String {
    format!("rgb({}, {}, {})", (c.red() * 255.0) as u8, (c.green() * 255.0) as u8, (c.blue() * 255.0) as u8)
}

/// Theme-following pages get the window's text, background and accent
/// colours; the rest get WebKit's light defaults on white.
fn page_style(widget: &impl IsA<gtk::Widget>, r: &Rendered) -> String {
    let manager = adw::StyleManager::default();
    let dark = manager.is_dark();
    let mut css = String::from(
        "body{margin:4px 18px 18px;}\
         details.salutations-quote{margin:6px 0;}\
         details.salutations-quote>summary{display:inline-block;list-style:none;cursor:pointer;\
         padding:2px 10px;border-radius:6px;font-size:0.85em;background:rgba(127,127,127,0.15);}\
         details.salutations-quote>summary::-webkit-details-marker{display:none;}\
         details.salutations-quote[open]>summary .show,\
         details.salutations-quote:not([open])>summary .hide{display:none;}\
         pre.quoted{opacity:0.75;border-left:3px solid rgba(127,127,127,0.4);padding-left:8px;}",
    );
    // The headers block appears only on paper; theme colours are dropped
    // there so a dark theme does not print a dark page.
    css.push_str(
        ".salutations-print{display:none;}\
         @media print{.salutations-print{display:block;margin:0 0 12px;padding-bottom:8px;\
         border-bottom:1px solid #888;font-family:sans-serif;font-size:10pt;color:#000;}\
         .salutations-print h1{font-size:14pt;margin:0 0 6px;}\
         .salutations-print div{margin:1px 0;}}",
    );
    if r.follows_theme {
        css.push_str("@media print{:root{color-scheme:light !important;color:#000 !important;background-color:#fff !important;}\
             a:link,a:visited{color:#1a5fb4 !important;}}");
    }
    if r.follows_theme {
        let fg = css_rgb(&widget.as_ref().color());
        let bg = css_rgb(&window_color(widget));
        let link = css_rgb(&manager.accent_color().to_standalone_rgba(dark));
        let scheme = if dark { "dark" } else { "light" };
        // A dark color-scheme paints WebKit's dark canvas over the
        // view's background colour unless the page sets one.
        css.push_str(&format!(
            ":root{{color-scheme:{scheme};color:{fg};background-color:{bg};}}a:link,a:visited{{color:{link};}}"
        ));
    } else {
        css.push_str(":root{color-scheme:light;}");
    }
    if r.is_plain {
        let font = if Settings::get().plain_monospace {
            "font-family:monospace;font-size:10.5pt;".to_string()
        } else {
            interface_font()
        };
        css.push_str(&format!(
            "pre.salutations-plain{{white-space:pre-wrap;overflow-wrap:anywhere;margin:0;{font}}}"
        ));
    }
    format!("<style>{css}</style>")
}

/// `raw` without the `X-Evolution-*` headers Camel adds to messages it
/// fetched, which name the local account.
fn without_local_headers(raw: &[u8]) -> Vec<u8> {
    // The header block ends at the first blank line, in either line ending.
    let lf = raw.windows(2).position(|w| w == b"\n\n").map(|i| i + 1);
    let crlf = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 2);
    let end = lf.into_iter().chain(crlf).min().unwrap_or(raw.len());
    let mut out = Vec::with_capacity(raw.len());
    let mut dropping = false;
    for line in raw[..end].split_inclusive(|&b| b == b'\n') {
        let continuation = line.first().is_some_and(|b| *b == b' ' || *b == b'\t');
        if !continuation {
            dropping = line.len() >= 12 && line[..12].eq_ignore_ascii_case(b"x-evolution-");
        }
        if !dropping {
            out.extend_from_slice(line);
        }
    }
    out.extend_from_slice(&raw[end..]);
    out
}

/// The subject, people, date and attachment names, for printing.
fn print_header(r: &Rendered, date: &str, attachments: &[salutations_core::compose::Attachment]) -> String {
    use salutations_core::compose::format_addresses;
    let esc = |t: &str| glib::markup_escape_text(t).to_string();
    let subject = if r.subject.is_empty() { "(No subject)" } else { &r.subject };
    let mut out = format!("<div class=\"salutations-print\"><h1>{}</h1>", esc(subject));
    let mut row = |label: &str, value: String| {
        if !value.is_empty() {
            out.push_str(&format!("<div><b>{label}:</b> {}</div>", esc(&value)));
        }
    };
    row("From", format_addresses(&r.from));
    row("To", format_addresses(&r.to));
    row("Cc", format_addresses(&r.cc));
    row("Date", date.to_string());
    let names: Vec<&str> = attachments.iter().map(|a| a.name.as_str()).collect();
    row("Attachments", names.join(", "));
    out.push_str("</div>");
    out
}

/// `doc` with `header` at the start of its body. Placed before the first
/// `<body>` tag, the block would open the body early and push the
/// message's head content into it.
fn with_print_header(doc: &str, header: &str) -> String {
    let lower = doc.to_ascii_lowercase();
    let at = lower
        .find("<body")
        .filter(|&i| lower[i + 5..].starts_with(|c: char| c == '>' || c.is_ascii_whitespace()))
        .and_then(|i| lower[i..].find('>').map(|j| i + j + 1));
    match at {
        Some(i) => format!("{}{header}{}", &doc[..i], &doc[i..]),
        None => format!("{header}{doc}"),
    }
}

/// The GTK interface font as CSS declarations.
fn interface_font() -> String {
    let desc = gtk::Settings::default()
        .and_then(|s| s.gtk_font_name())
        .map(|n| gtk::pango::FontDescription::from_string(&n));
    let family = desc
        .as_ref()
        .and_then(|d| d.family())
        .map(|f| format!("\"{}\", ", f.replace('"', "")))
        .unwrap_or_default();
    let size = desc
        .as_ref()
        .map(|d| d.size())
        .filter(|s| *s > 0)
        .map_or(11.0, |s| s as f64 / gtk::pango::SCALE as f64);
    format!("font-family:{family}sans-serif;font-size:{size}pt;")
}

/// Where opened attachments are written; emptied at startup, since the
/// apps they were handed to may still have them open until then.
fn attachments_dir() -> std::path::PathBuf {
    salutations_core::cache_dir().join("opened")
}

pub fn clear_opened_attachments() {
    let _ = std::fs::remove_dir_all(attachments_dir());
}

/// A sender's file name, safe as one path component.
fn safe_name(name: &str) -> String {
    let n: String = name.chars().map(|c| if c == '/' || c == '\\' || c.is_control() { '_' } else { c }).collect();
    let n = n.trim().trim_start_matches('.').to_string();
    if n.is_empty() { "attachment".into() } else { n }
}

/// `name`, or "name (2).ext" and so on when it exists in `folder` or was
/// used earlier in the same save.
fn free_name(folder: &gio::File, name: &str, used: &mut std::collections::HashSet<String>) -> String {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    let mut candidate = name.to_string();
    let mut n = 2;
    while used.contains(&candidate) || folder.child(&candidate).query_exists(None::<&gio::Cancellable>) {
        candidate = format!("{stem} ({n}){ext}");
        n += 1;
    }
    used.insert(candidate.clone());
    candidate
}

fn error_dialog(widget: &impl IsA<gtk::Widget>, text: &str) {
    let dialog = adw::AlertDialog::new(None, Some(text));
    dialog.add_response("ok", "_OK");
    dialog.present(Some(widget));
}

/// Each item removes one rule; `display` enables the one in force.
fn unallow_menu(sender: Option<&Address>) -> gio::Menu {
    let menu = gio::Menu::new();
    menu.append(Some("For this message"), Some("viewer.unallow-once"));
    if let Some(a) = sender {
        menu.append(Some(&format!("For {}", a.email)), Some("viewer.unallow-sender"));
        if let Some(d) = a.domain() {
            menu.append(Some(&format!("For {d}")), Some("viewer.unallow-domain"));
        }
    }
    menu
}

fn remote_menu(sender: Option<&Address>) -> gio::Menu {
    let menu = gio::Menu::new();
    menu.append(Some("For this message"), Some("viewer.remote-once"));
    if let Some(a) = sender {
        menu.append(Some(&format!("Always for {}", a.email)), Some("viewer.remote-sender"));
        if let Some(d) = a.domain() {
            menu.append(Some(&format!("Always for {d}")), Some("viewer.remote-domain"));
        }
    }
    menu
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_headers() {
        let raw = b"From: a@b\r\nX-Evolution-Source: x\r\n folded\r\nSubject: s\r\n\r\nX-Evolution-Source: body\r\n";
        assert_eq!(without_local_headers(raw), b"From: a@b\r\nSubject: s\r\n\r\nX-Evolution-Source: body\r\n");
        let raw = b"x-evolution-a: 1\nTo: c@d\n\nhi\n";
        assert_eq!(without_local_headers(raw), b"To: c@d\n\nhi\n");
    }

    #[test]
    fn print_header_placement() {
        assert_eq!(with_print_header("<html><BODY class=x>hi", "H"), "<html><BODY class=x>Hhi");
        assert_eq!(with_print_header("<bodyx>hi", "H"), "H<bodyx>hi");
        assert_eq!(with_print_header("<pre>hi</pre>", "H"), "H<pre>hi</pre>");
    }
}
