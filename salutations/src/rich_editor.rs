//! The rich text editor: an editable WebKit page, and the formatting bar
//! that drives it. The page's own scripts never run (JavaScript markup is
//! off); only the injected editor script does.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use salutations_core::compose::Inline;
use webkit::javascriptcore;
use webkit::prelude::*;

const SCRIPT: &str = include_str!("../data/rich-editor.js");
const CSP: &str = "default-src 'none'; img-src cid: data:; style-src 'unsafe-inline'";

thread_local! {
    /// The viewer's web view, whose web process the editors share.
    static RELATED: RefCell<Option<glib::WeakRef<webkit::WebView>>> = const { RefCell::new(None) };
    /// Images the open editors show, by Content-ID, for the `cid:` scheme.
    static IMAGES: RefCell<Vec<Inline>> = const { RefCell::new(Vec::new()) };
}

pub fn set_related(view: &webkit::WebView) {
    RELATED.with_borrow_mut(|r| *r = Some(view.downgrade()));
}

/// An editor's image by Content-ID: its bytes and type.
pub fn image(cid: &str) -> Option<(Vec<u8>, String)> {
    IMAGES.with_borrow(|i| i.iter().find(|i| i.cid == cid).map(|i| (i.data.to_vec(), i.mime_type.clone())))
}

pub fn add_image(image: Inline) {
    IMAGES.with_borrow_mut(|i| {
        i.retain(|x| x.cid != image.cid);
        i.push(image);
    });
}

/// The formatting at the cursor, as the page reports it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FormatState {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub mono: bool,
    /// small, normal, large, or huge.
    pub size: String,
    /// A CSS colour, or empty for the automatic one.
    pub color: String,
    /// A CSS colour, or empty for none.
    pub highlight: String,
    /// left, center, right, or justify.
    pub align: String,
    /// bullet, number, or empty.
    pub list: String,
}

type Handler = Rc<dyn Fn()>;

pub struct RichEditor {
    pub web: webkit::WebView,
    html: RefCell<String>,
    state: RefCell<FormatState>,
    loaded: Cell<bool>,
    /// Scripts asked for before the page loaded.
    queued: RefCell<Vec<String>>,
    on_changed: RefCell<Option<Handler>>,
    on_state: RefCell<Vec<Handler>>,
    on_signature_edited: RefCell<Option<Handler>>,
    /// Set from loading new content until the page first reports it.
    loading: Cell<bool>,
}

fn css_rgb(c: &gdk::RGBA) -> String {
    format!("rgb({}, {}, {})", (c.red() * 255.0).round() as u8, (c.green() * 255.0).round() as u8, (c.blue() * 255.0).round() as u8)
}

fn js_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '<' => out.push_str("\\u003c"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

impl RichEditor {
    pub fn new() -> Rc<Self> {
        let settings = webkit::Settings::new();
        settings.set_enable_javascript(true);
        settings.set_enable_javascript_markup(false);
        settings.set_enable_page_cache(false);
        settings.set_enable_html5_database(false);
        settings.set_enable_html5_local_storage(false);
        settings.set_enable_webgl(false);
        settings.set_enable_media(false);
        settings.set_enable_back_forward_navigation_gestures(false);
        settings.set_javascript_can_access_clipboard(false);
        let manager = webkit::UserContentManager::new();
        manager.add_script(&webkit::UserScript::new(
            SCRIPT,
            webkit::UserContentInjectedFrames::TopFrame,
            webkit::UserScriptInjectionTime::End,
            &[],
            &[],
        ));
        manager.register_script_message_handler("salutations", None);
        let mut builder = webkit::WebView::builder().settings(&settings).user_content_manager(&manager).vexpand(true);
        match RELATED.with_borrow(|r| r.as_ref().and_then(|w| w.upgrade())) {
            Some(related) => builder = builder.related_view(&related),
            None => builder = builder.network_session(&webkit::NetworkSession::new_ephemeral()),
        }
        let web = builder.build();
        web.add_css_class("rich-editor");
        if let Some(ctx) = web.context() {
            ctx.set_spell_checking_enabled(true);
            let langs: Vec<String> = glib::language_names()
                .iter()
                .map(|l| l.split('.').next().unwrap_or("").to_string())
                .filter(|l| l.contains('_'))
                .take(1)
                .collect();
            if !langs.is_empty() {
                ctx.set_spell_checking_languages(&langs.iter().map(String::as_str).collect::<Vec<_>>());
            }
        }
        let this = Rc::new(Self {
            web,
            html: RefCell::new(String::new()),
            state: RefCell::new(FormatState::default()),
            loaded: Cell::new(false),
            queued: RefCell::new(Vec::new()),
            on_changed: RefCell::new(None),
            on_state: RefCell::new(Vec::new()),
            on_signature_edited: RefCell::new(None),
            loading: Cell::new(false),
        });
        let weak = Rc::downgrade(&this);
        manager.connect_script_message_received(Some("salutations"), move |_, value| {
            if let Some(this) = weak.upgrade() {
                this.received(value);
            }
        });
        let weak = Rc::downgrade(&this);
        this.web.connect_load_changed(move |_, event| {
            let Some(this) = weak.upgrade() else { return };
            if event == webkit::LoadEvent::Finished {
                this.loaded.set(true);
                for s in this.queued.take() {
                    this.run(&s);
                }
            }
        });
        // Nothing navigates away from the page, whatever is clicked.
        this.web.connect_decide_policy(|_, decision, kind| {
            if kind == webkit::PolicyDecisionType::NavigationAction
                && let Some(nav) = decision.downcast_ref::<webkit::NavigationPolicyDecision>()
                && nav.navigation_action().is_some_and(|a| a.navigation_type() != webkit::NavigationType::Other)
            {
                decision.ignore();
                return true;
            }
            if kind == webkit::PolicyDecisionType::NewWindowAction {
                decision.ignore();
                return true;
            }
            false
        });
        this.web.connect_context_menu(|_, menu, _| {
            use webkit::ContextMenuAction as A;
            let keep = |a: A| {
                matches!(
                    a,
                    A::SpellingGuess
                        | A::NoGuessesFound
                        | A::IgnoreSpelling
                        | A::LearnSpelling
                        | A::Cut
                        | A::Copy
                        | A::Paste
                        | A::PasteAsPlainText
                        | A::Delete
                        | A::SelectAll
                        | A::InputMethods
                        | A::Unicode
                        | A::InsertEmoji
                        | A::Custom
                )
            };
            for item in menu.items() {
                if !item.is_separator() && !keep(item.stock_action()) {
                    menu.remove(&item);
                }
            }
            menu.n_items() == 0
        });
        // The page's colours come from the widget's style, which is known
        // only once it is in a window.
        let weak = Rc::downgrade(&this);
        this.web.connect_map(move |_| {
            if let Some(this) = weak.upgrade() {
                this.run(&format!("salutations.theme({})", js_string(&this.theme())));
            }
        });
        let manager = adw::StyleManager::default();
        let restyled = |weak: std::rc::Weak<Self>| {
            glib::idle_add_local_full(glib::Priority::LOW, move || {
                if let Some(this) = weak.upgrade() {
                    this.run(&format!("salutations.theme({})", js_string(&this.theme())));
                }
                glib::ControlFlow::Break
            });
        };
        let weak = Rc::downgrade(&this);
        manager.connect_dark_notify(move |_| restyled(weak.clone()));
        let weak = Rc::downgrade(&this);
        manager.connect_accent_color_notify(move |_| restyled(weak.clone()));
        this
    }

    fn theme(&self) -> String {
        let manager = adw::StyleManager::default();
        let dark = manager.is_dark();
        #[allow(deprecated)]
        let bg = self.web.style_context().lookup_color("view_bg_color").unwrap_or(if dark {
            gdk::RGBA::new(0.118, 0.118, 0.118, 1.0)
        } else {
            gdk::RGBA::WHITE
        });
        let fg = css_rgb(&self.web.color());
        let link = css_rgb(&manager.accent_color().to_standalone_rgba(dark));
        format!(
            ":root{{color-scheme:{};color:{fg};background-color:{};}}a:link,a:visited{{color:{link};}}",
            if dark { "dark" } else { "light" },
            css_rgb(&bg)
        )
    }

    fn received(&self, value: &javascriptcore::Value) {
        let get = |k: &str| value.object_get_property(k);
        let text = |k: &str| get(k).filter(|v| v.is_string()).map(|v| v.to_str().to_string()).unwrap_or_default();
        let flag = |k: &str| get(k).is_some_and(|v| v.to_boolean());
        match text("t").as_str() {
            "html" => {
                *self.html.borrow_mut() = text("html");
                // The page's first report, after loading, is not an edit.
                if !self.loading.replace(false) {
                    let f = self.on_changed.borrow().clone();
                    if let Some(f) = f {
                        f();
                    }
                }
            }
            "state" => {
                let s = FormatState {
                    bold: flag("bold"),
                    italic: flag("italic"),
                    underline: flag("underline"),
                    strike: flag("strike"),
                    mono: flag("mono"),
                    size: text("size"),
                    color: text("color"),
                    highlight: text("highlight"),
                    align: text("align"),
                    list: text("list"),
                };
                if *self.state.borrow() != s {
                    *self.state.borrow_mut() = s;
                    let handlers = self.on_state.borrow().clone();
                    for f in handlers {
                        f();
                    }
                }
            }
            "sig" => {
                let f = self.on_signature_edited.borrow().clone();
                if let Some(f) = f {
                    f();
                }
            }
            _ => {}
        }
    }

    /// Runs `script` in the page, once it has loaded, with the keyboard
    /// focus back in the page after a menu or popover.
    fn command(&self, script: &str) {
        self.web.grab_focus();
        self.run(script);
    }

    /// Runs `script` in the page, once it has loaded.
    fn run(&self, script: &str) {
        if !self.loaded.get() {
            self.queued.borrow_mut().push(script.to_string());
            return;
        }
        self.web.evaluate_javascript(script, None, None, None::<&gio::Cancellable>, |_| {});
    }

    /// Replaces the content with `html`, the inside of a body element.
    pub fn set_html(&self, html: &str) {
        self.loaded.set(false);
        self.queued.borrow_mut().clear();
        *self.html.borrow_mut() = html.to_string();
        self.loading.set(true);
        let doc = format!(
            "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
             <meta http-equiv=\"Content-Security-Policy\" content=\"{CSP}\">\
             <style>html{{height:100%;{}}}body{{margin:0;padding:12px;min-height:calc(100% - 24px);outline:none;\
             overflow-wrap:anywhere;}}\
             blockquote{{margin:0 0 0 .8ex;border-left:2px solid rgba(127,127,127,.5);padding-left:1ex;}}\
             img{{max-width:100%;height:auto;}}</style><style id=\"salutations-theme\">{}</style></head>\
             <body contenteditable=\"true\" spellcheck=\"true\">{html}</body></html>",
            crate::viewer::interface_font(),
            self.theme()
        );
        self.web.load_html(&doc, None);
    }

    /// The content as HTML, the inside of the body element.
    pub fn html(&self) -> String {
        self.html.borrow().clone()
    }

    pub fn state(&self) -> FormatState {
        self.state.borrow().clone()
    }

    pub fn connect_changed(&self, f: impl Fn() + 'static) {
        *self.on_changed.borrow_mut() = Some(Rc::new(f));
    }

    pub fn connect_state(&self, f: impl Fn() + 'static) {
        self.on_state.borrow_mut().push(Rc::new(f));
    }

    pub fn connect_signature_edited(&self, f: impl Fn() + 'static) {
        *self.on_signature_edited.borrow_mut() = Some(Rc::new(f));
    }

    pub fn style(&self, name: &str, on: bool) {
        self.command(&format!("salutations.style({}, {on})", js_string(name)));
    }

    pub fn normal(&self) {
        self.command("salutations.normal()");
    }

    pub fn exec(&self, command: &str) {
        self.command(&format!("salutations.exec({}, null)", js_string(command)));
    }

    pub fn size(&self, size: &str) {
        self.command(&format!("salutations.size({})", js_string(size)));
    }

    pub fn color(&self, css: &str) {
        self.command(&format!("salutations.color({})", js_string(css)));
    }

    pub fn highlight(&self, css: &str) {
        self.command(&format!("salutations.highlight({})", js_string(css)));
    }

    pub fn align(&self, align: &str) {
        let cmd = match align {
            "center" => "justifyCenter",
            "right" => "justifyRight",
            "justify" => "justifyFull",
            _ => "justifyLeft",
        };
        self.exec(cmd);
    }

    pub fn list(&self, kind: &str) {
        self.command(&format!("salutations.list({})", js_string(kind)));
    }

    pub fn link(&self, url: &str, text: &str) {
        self.command(&format!("salutations.link({}, {})", js_string(url), js_string(text)));
    }

    pub fn table(&self, cols: u32, rows: u32, header: bool) {
        self.command(&format!("salutations.table({cols}, {rows}, {header})"));
    }

    pub fn image(&self, cid: &str, name: &str, width: Option<i32>) {
        let w = width.map_or("0".to_string(), |w| w.to_string());
        self.command(&format!("salutations.image({}, {}, {w})", js_string(cid), js_string(name)));
    }

    /// Puts the marked signature `html` in place of any other, or takes
    /// it out with None.
    pub fn signature(&self, html: Option<&str>, above: bool) {
        self.run(&format!("salutations.signature({}, {above})", html.map_or("null".to_string(), js_string)));
    }

    pub fn caret_to_start(&self) {
        self.run("salutations.caretToStart()");
    }

    pub fn grab_focus(&self) {
        self.web.grab_focus();
        self.run("salutations.focus()");
    }
}

const COLORS: [(&str, &str); 8] = [
    ("Red", "#c01c28"),
    ("Orange", "#c64600"),
    ("Yellow", "#9c6e03"),
    ("Green", "#26a269"),
    ("Blue", "#1c71d8"),
    ("Purple", "#813d9c"),
    ("Brown", "#865e3c"),
    ("Gray", "#77767b"),
];

const HIGHLIGHTS: [(&str, &str); 6] = [
    ("Yellow", "#f9f06b"),
    ("Green", "#8ff0a4"),
    ("Blue", "#99c1f1"),
    ("Pink", "#f5c2e7"),
    ("Orange", "#ffbe6f"),
    ("Gray", "#deddda"),
];

/// "rgb(192, 28, 40)" or "#c01c28" as (r, g, b).
fn parse_color(css: &str) -> Option<(u8, u8, u8)> {
    let c = gdk::RGBA::parse(css.trim()).ok()?;
    Some(((c.red() * 255.0).round() as u8, (c.green() * 255.0).round() as u8, (c.blue() * 255.0).round() as u8))
}

fn same_color(a: &str, b: &str) -> bool {
    parse_color(a).is_some_and(|x| parse_color(b) == Some(x))
}

fn hex(css: &str) -> Option<String> {
    parse_color(css).map(|(r, g, b)| format!("#{r:02x}{g:02x}{b:02x}"))
}

/// A row of swatches with a first choice for none; picks call `pick` with
/// a colour, or empty for none.
type Mark = Rc<dyn Fn(&str)>;

fn swatches(none_label: &str, colors: &[(&str, &str)], round: bool, pick: impl Fn(&str) + 'static) -> (gtk::Popover, Mark) {
    let pick: Rc<dyn Fn(&str)> = Rc::new(pick);
    let grid = gtk::Grid::builder().row_spacing(6).column_spacing(6).build();
    let popover = gtk::Popover::builder().child(&grid).build();
    let mut buttons: Vec<(gtk::ToggleButton, String)> = Vec::new();
    let none = gtk::ToggleButton::with_label(none_label);
    none.add_css_class("flat");
    grid.attach(&none, 0, 0, 4, 1);
    buttons.push((none, String::new()));
    for (i, (name, css)) in colors.iter().enumerate() {
        let b = gtk::ToggleButton::builder().tooltip_text(*name).build();
        b.add_css_class("swatch");
        if round {
            b.add_css_class("circular");
        }
        let provider = gtk::CssProvider::new();
        provider.load_from_string(&format!("button.swatch{{background:{css};min-width:28px;min-height:28px;padding:0;}}"));
        #[allow(deprecated)]
        b.style_context().add_provider(&provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        grid.attach(&b, (i % 4) as i32, 1 + (i / 4) as i32, 1, 1);
        buttons.push((b, css.to_string()));
    }
    let buttons = Rc::new(buttons);
    for (b, css) in buttons.iter() {
        let (pick, popover, css) = (pick.clone(), popover.downgrade(), css.clone());
        b.connect_clicked(move |_| {
            if let Some(p) = popover.upgrade() {
                p.popdown();
            }
            pick(&css);
        });
    }
    let bs = buttons.clone();
    let mark: Mark = Rc::new(move |current: &str| {
        for (b, css) in bs.iter() {
            b.set_active(if css.is_empty() { current.is_empty() } else { same_color(css, current) });
        }
    });
    (popover, mark)
}

/// The formatting buttons for a [`RichEditor`]: text style, size, colour,
/// highlight, alignment, and lists.
pub struct FormatBar {
    pub widget: gtk::Box,
}

impl FormatBar {
    pub fn new(editor: &Rc<RichEditor>) -> Self {
        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        widget.add_css_class("format-bar");
        let actions = gio::SimpleActionGroup::new();
        widget.insert_action_group("rich", Some(&actions));

        // Text style: one "A" drawn with the styles at the cursor.
        let style_label = gtk::Label::new(Some("A"));
        style_label.add_css_class("style-a");
        let styles = gio::Menu::new();
        let section = gio::Menu::new();
        for (name, label) in [("bold", "_Bold"), ("italic", "_Italic"), ("underline", "_Underline"), ("strikeThrough", "_Strikethrough"), ("mono", "_Monospace")] {
            let a = gio::SimpleAction::new_stateful(name, None, &false.to_variant());
            let e = Rc::downgrade(editor);
            let n = name.to_string();
            a.connect_activate(move |a, _| {
                let on = !a.state().and_then(|s| s.get::<bool>()).unwrap_or(false);
                a.set_state(&on.to_variant());
                if let Some(e) = e.upgrade() {
                    e.style(&n, on);
                }
            });
            actions.add_action(&a);
            section.append(Some(label), Some(&format!("rich.{name}")));
        }
        styles.append_section(None, &section);
        let normal = gio::SimpleAction::new("normal", None);
        let e = Rc::downgrade(editor);
        normal.connect_activate(move |_, _| {
            if let Some(e) = e.upgrade() {
                e.normal();
            }
        });
        actions.add_action(&normal);
        let section = gio::Menu::new();
        section.append(Some("_Normal text"), Some("rich.normal"));
        styles.append_section(None, &section);
        let style_button = gtk::MenuButton::builder().child(&style_label).menu_model(&styles).tooltip_text("Text style").build();

        // Size.
        let size = gio::SimpleAction::new_stateful("size", Some(glib::VariantTy::STRING), &"normal".to_variant());
        let e = Rc::downgrade(editor);
        size.connect_change_state(move |a, v| {
            let (Some(v), Some(e)) = (v, e.upgrade()) else { return };
            a.set_state(v);
            e.size(&v.get::<String>().unwrap_or_default());
        });
        actions.add_action(&size);
        let sizes = gio::Menu::new();
        for (key, label) in [("small", "_Small"), ("normal", "_Normal"), ("large", "_Large"), ("huge", "_Huge")] {
            let item = gio::MenuItem::new(Some(label), None);
            item.set_action_and_target_value(Some("rich.size"), Some(&key.to_variant()));
            sizes.append_item(&item);
        }
        let size_button = gtk::MenuButton::builder().icon_name("salutations-text-size-symbolic").menu_model(&sizes).tooltip_text("Size").build();

        // Colour and highlight.
        let color_label = gtk::Label::new(None);
        let e = Rc::downgrade(editor);
        let (color_popover, color_mark) = swatches("_Automatic", &COLORS, true, move |css| {
            if let Some(e) = e.upgrade() {
                e.color(css);
            }
        });
        let color_button = gtk::MenuButton::builder().child(&color_label).popover(&color_popover).tooltip_text("Text color").build();
        let highlight_label = gtk::Label::new(None);
        let e = Rc::downgrade(editor);
        let (highlight_popover, highlight_mark) = swatches("_None", &HIGHLIGHTS, false, move |css| {
            if let Some(e) = e.upgrade() {
                e.highlight(css);
            }
        });
        let highlight_button = gtk::MenuButton::builder().child(&highlight_label).popover(&highlight_popover).tooltip_text("Highlight").build();
        for p in [&color_popover, &highlight_popover] {
            if let Some(grid) = p.child() {
                // Mnemonic labels on the first button.
                if let Some(b) = grid.first_child().and_downcast::<gtk::ToggleButton>() {
                    b.set_use_underline(true);
                }
            }
        }

        // Alignment.
        let align = gio::SimpleAction::new_stateful("align", Some(glib::VariantTy::STRING), &"left".to_variant());
        let e = Rc::downgrade(editor);
        align.connect_change_state(move |a, v| {
            let (Some(v), Some(e)) = (v, e.upgrade()) else { return };
            a.set_state(v);
            e.align(&v.get::<String>().unwrap_or_default());
        });
        actions.add_action(&align);
        let aligns = gio::Menu::new();
        for (key, label) in [("left", "_Left"), ("center", "_Center"), ("right", "_Right"), ("justify", "_Justify")] {
            let item = gio::MenuItem::new(Some(label), None);
            item.set_action_and_target_value(Some("rich.align"), Some(&key.to_variant()));
            aligns.append_item(&item);
        }
        let align_button = gtk::MenuButton::builder().icon_name("format-justify-left-symbolic").menu_model(&aligns).tooltip_text("Alignment").build();

        // Lists and indenting.
        let list = gio::SimpleAction::new_stateful("list", Some(glib::VariantTy::STRING), &"".to_variant());
        let e = Rc::downgrade(editor);
        list.connect_change_state(move |a, v| {
            let (Some(v), Some(e)) = (v, e.upgrade()) else { return };
            let kind = v.get::<String>().unwrap_or_default();
            let current = a.state().and_then(|s| s.get::<String>()).unwrap_or_default();
            // Choosing the list the cursor is in takes it out of the list.
            let target = if kind == current { current.clone() } else { kind.clone() };
            a.set_state(&(if kind == current { String::new() } else { kind }).to_variant());
            e.list(&target);
        });
        actions.add_action(&list);
        for (name, cmd) in [("indent", "indent"), ("outdent", "outdent")] {
            let a = gio::SimpleAction::new(name, None);
            let e = Rc::downgrade(editor);
            a.connect_activate(move |_, _| {
                if let Some(e) = e.upgrade() {
                    e.exec(cmd);
                }
            });
            actions.add_action(&a);
        }
        let lists = gio::Menu::new();
        let section = gio::Menu::new();
        for (key, label) in [("bullet", "_Bulleted list"), ("number", "N_umbered list")] {
            let item = gio::MenuItem::new(Some(label), None);
            item.set_action_and_target_value(Some("rich.list"), Some(&key.to_variant()));
            section.append_item(&item);
        }
        lists.append_section(None, &section);
        let section = gio::Menu::new();
        section.append(Some("Indent _more"), Some("rich.indent"));
        section.append(Some("Indent _less"), Some("rich.outdent"));
        lists.append_section(None, &section);
        let list_button = gtk::MenuButton::builder().icon_name("view-list-bullet-symbolic").menu_model(&lists).tooltip_text("Lists and indenting").build();

        for b in [&style_button, &size_button, &color_button, &highlight_button, &align_button, &list_button] {
            b.add_css_class("format-button");
            b.add_css_class("raised");
            widget.append(b);
        }

        let update = {
            let (editor, actions) = (Rc::downgrade(editor), actions.clone());
            let (style_label, color_label, highlight_label, align_button) =
                (style_label.clone(), color_label.clone(), highlight_label.clone(), align_button.clone());
            move || {
                let Some(e) = editor.upgrade() else { return };
                let s = e.state();
                let set = |name: &str, v: glib::Variant| {
                    if let Some(a) = actions.lookup_action(name).and_downcast::<gio::SimpleAction>() {
                        a.set_state(&v);
                    }
                };
                for (name, on) in [("bold", s.bold), ("italic", s.italic), ("underline", s.underline), ("strikeThrough", s.strike), ("mono", s.mono)] {
                    set(name, on.to_variant());
                }
                set("size", if s.size.is_empty() { "normal".to_string() } else { s.size.clone() }.to_variant());
                set("align", if s.align.is_empty() { "left".to_string() } else { s.align.clone() }.to_variant());
                set("list", s.list.to_variant());
                let mut attrs = String::new();
                if s.bold {
                    attrs.push_str(" weight=\"bold\"");
                }
                if s.italic {
                    attrs.push_str(" style=\"italic\"");
                }
                if s.underline {
                    attrs.push_str(" underline=\"single\"");
                }
                if s.strike {
                    attrs.push_str(" strikethrough=\"true\"");
                }
                if s.mono {
                    attrs.push_str(" font_family=\"monospace\"");
                }
                style_label.set_markup(&format!("<span{attrs}>A</span>"));
                let color = hex(&s.color);
                color_label.set_markup(&match &color {
                    Some(c) => format!("<span weight=\"bold\" foreground=\"{c}\" underline=\"single\" underline_color=\"{c}\">A</span>"),
                    None => "<span weight=\"bold\" underline=\"single\">A</span>".to_string(),
                });
                color_mark(&s.color);
                let highlight = hex(&s.highlight);
                highlight_label.set_markup(&match &highlight {
                    Some(h) => format!("<span background=\"{h}\" foreground=\"#000000\"> a </span>"),
                    None => "<span background=\"#80808040\"> a </span>".to_string(),
                });
                highlight_mark(&s.highlight);
                align_button.set_icon_name(match s.align.as_str() {
                    "center" => "format-justify-center-symbolic",
                    "right" => "format-justify-right-symbolic",
                    "justify" => "format-justify-fill-symbolic",
                    _ => "format-justify-left-symbolic",
                });
            }
        };
        update();
        editor.connect_state(update);
        Self { widget }
    }
}

/// A grid to pick a table's size, up to 8 × 8, with a header row switch.
pub fn table_picker(pick: impl Fn(u32, u32, bool) + 'static) -> gtk::Popover {
    const N: u32 = 8;
    let grid = gtk::Grid::builder().row_spacing(3).column_spacing(3).halign(gtk::Align::Center).build();
    let label = gtk::Label::new(Some("1 × 1"));
    label.add_css_class("heading");
    let header = adw::SwitchRow::builder().title("Header row").active(true).build();
    let list = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
    list.append(&header);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.append(&grid);
    content.append(&label);
    content.append(&list);
    let popover = gtk::Popover::builder().child(&content).build();
    let cells: Rc<RefCell<Vec<(u32, u32, gtk::Button)>>> = Rc::default();
    let hover = {
        let (cells, label) = (cells.clone(), label.clone());
        Rc::new(move |c: u32, r: u32| {
            for (x, y, b) in cells.borrow().iter() {
                if *x <= c && *y <= r {
                    b.add_css_class("picked");
                } else {
                    b.remove_css_class("picked");
                }
            }
            label.set_text(&format!("{c} × {r}"));
        })
    };
    let pick = Rc::new(pick);
    for r in 1..=N {
        for c in 1..=N {
            let b = gtk::Button::builder().tooltip_text(format!("{c} columns, {r} rows")).build();
            b.add_css_class("table-cell");
            let motion = gtk::EventControllerMotion::new();
            let h = hover.clone();
            motion.connect_enter(move |_, _, _| h(c, r));
            b.add_controller(motion);
            let h = hover.clone();
            let focus = gtk::EventControllerFocus::new();
            focus.connect_enter(move |_| h(c, r));
            b.add_controller(focus);
            let (pick, popover, header) = (pick.clone(), popover.downgrade(), header.clone());
            b.connect_clicked(move |_| {
                if let Some(p) = popover.upgrade() {
                    p.popdown();
                }
                pick(c, r, header.is_active());
            });
            grid.attach(&b, c as i32 - 1, r as i32 - 1, 1, 1);
            cells.borrow_mut().push((c, r, b));
        }
    }
    hover(1, 1);
    popover
}

/// Asks for a link's address and, without a selection, its text.
pub async fn ask_link(parent: &impl IsA<gtk::Widget>) -> Option<(String, String)> {
    let address = adw::EntryRow::builder().title("Address").activates_default(true).build();
    address.set_input_purpose(gtk::InputPurpose::Url);
    let text = adw::EntryRow::builder().title("Text").activates_default(true).build();
    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
    list.append(&address);
    list.append(&text);
    let note = gtk::Label::builder()
        .label("The text is used when nothing is selected")
        .wrap(true)
        .xalign(0.0)
        .css_classes(["dimmed", "caption"])
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.append(&list);
    content.append(&note);
    let dialog = adw::AlertDialog::builder().heading("Insert link").extra_child(&content).build();
    dialog.add_responses(&[("cancel", "_Cancel"), ("insert", "_Insert")]);
    dialog.set_response_appearance("insert", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("insert"));
    dialog.set_close_response("cancel");
    dialog.set_response_enabled("insert", false);
    let d = dialog.downgrade();
    address.connect_changed(move |e| {
        if let Some(d) = d.upgrade() {
            d.set_response_enabled("insert", !e.text().trim().is_empty());
        }
    });
    address.grab_focus();
    if dialog.choose_future(Some(parent)).await != "insert" {
        return None;
    }
    let mut url = address.text().trim().to_string();
    if !url.contains(':') {
        url = if url.contains('@') && !url.contains('/') { format!("mailto:{url}") } else { format!("https://{url}") };
    }
    Some((url, text.text().trim().to_string()))
}
