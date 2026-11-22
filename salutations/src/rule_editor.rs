//! Editing message rules: the Rules group in an account's preferences and
//! the dialog for one rule.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use salutations_core::rules::{self, Condition, Dmarc, Field, Filing, Rule, TextOp};

/// Runs a rule once on the messages already in an account's INBOX.
pub type ApplyRule = Rc<dyn Fn(&str, Rule)>;

/// The condition kinds, as the first menu of a condition shows them.
const KINDS: [&str; 7] = ["From", "To or Cc", "Subject", "List-ID", "Header", "Message", "Sender"];
const TEXT_OPS: [(&str, TextOp); 5] = [
    ("contains", TextOp::Contains),
    ("doesn't contain", TextOp::NotContains),
    ("is", TextOp::Is),
    ("starts with", TextOp::StartsWith),
    ("ends with", TextOp::EndsWith),
];
const MESSAGE_OPS: [&str; 5] = [
    "has attachments",
    "has no attachments",
    "passed sender verification",
    "failed sender verification",
    "has no sender verification",
];
const SENDER_OPS: [&str; 2] = ["is in my contacts", "is not in my contacts"];

fn text_field(kind: u32) -> Option<Field> {
    Some(match kind {
        0 => Field::From,
        1 => Field::ToCc,
        2 => Field::Subject,
        3 => Field::ListId,
        4 => Field::Header,
        _ => return None,
    })
}

/// Takes `row` out of its list. A preferences group's own remove refused
/// rows in libadwaita 1.8 (`gtk_box_remove: assertion 'GTK_IS_BOX (box)'`).
fn detach(row: &impl IsA<gtk::Widget>) {
    if let Some(list) = row.parent().and_downcast::<gtk::ListBox>() {
        list.remove(row);
    }
}

/// The editor's widgets for one condition.
struct ConditionRow {
    row: adw::PreferencesRow,
    kind: gtk::DropDown,
    op: gtk::DropDown,
    header: gtk::Entry,
    value: gtk::Entry,
}

impl ConditionRow {
    fn new(c: &Condition) -> Rc<Self> {
        let kind = gtk::DropDown::from_strings(&KINDS);
        let op = gtk::DropDown::from_strings(&[]);
        let header = gtk::Entry::builder().placeholder_text("Header name").hexpand(true).build();
        let value = gtk::Entry::builder().placeholder_text("Text").hexpand(true).build();
        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Remove condition")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        top.append(&kind);
        top.append(&header);
        let spacer = gtk::Box::builder().hexpand(true).build();
        top.append(&spacer);
        top.append(&remove);
        let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bottom.append(&op);
        bottom.append(&value);
        let column = gtk::Box::new(gtk::Orientation::Vertical, 6);
        column.set_margin_top(8);
        column.set_margin_bottom(8);
        column.set_margin_start(12);
        column.set_margin_end(8);
        column.append(&top);
        column.append(&bottom);
        // A preferences row, so the group puts it in its list.
        let row = adw::PreferencesRow::builder().child(&column).activatable(false).selectable(false).build();
        let this = Rc::new(Self { row, kind, op, header, value });
        let weak = Rc::downgrade(&this);
        let s = spacer.clone();
        this.kind.connect_selected_notify(move |_| {
            if let Some(this) = weak.upgrade() {
                this.kind_changed(0);
                s.set_visible(!WidgetExt::is_visible(&this.header));
            }
        });
        this.set(c);
        spacer.set_visible(!WidgetExt::is_visible(&this.header));
        let weak = Rc::downgrade(&this);
        remove.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                detach(&this.row);
            }
        });
        this
    }

    /// Fills the op menu for the kind shown and selects `op`.
    fn kind_changed(&self, op: u32) {
        let kind = self.kind.selected();
        let ops: Vec<&str> = match kind {
            5 => MESSAGE_OPS.to_vec(),
            6 => SENDER_OPS.to_vec(),
            _ => TEXT_OPS.iter().map(|(l, _)| *l).collect(),
        };
        self.op.set_model(Some(&gtk::StringList::new(&ops)));
        self.op.set_selected(op);
        let text = text_field(kind).is_some();
        self.value.set_visible(text);
        self.header.set_visible(kind == 4);
    }

    fn set(&self, c: &Condition) {
        let (kind, op) = match c {
            Condition::Text { field, header, op, value } => {
                self.header.set_text(header);
                self.value.set_text(value);
                let kind = match field {
                    Field::From => 0,
                    Field::ToCc => 1,
                    Field::Subject => 2,
                    Field::ListId => 3,
                    Field::Header => 4,
                };
                (kind, TEXT_OPS.iter().position(|(_, o)| o == op).unwrap_or(0) as u32)
            }
            Condition::Attachments(has) => (5, if *has { 0 } else { 1 }),
            Condition::Dmarc(d) => (
                5,
                match d {
                    Dmarc::Pass => 2,
                    Dmarc::Fail => 3,
                    Dmarc::None => 4,
                },
            ),
            Condition::InContacts(known) => (6, if *known { 0 } else { 1 }),
        };
        self.kind.set_selected(kind);
        self.kind_changed(op);
    }

    /// None while a text condition has no text, or a header no name.
    fn condition(&self) -> Option<Condition> {
        let op = self.op.selected();
        match (self.kind.selected(), text_field(self.kind.selected())) {
            (_, Some(field)) => {
                let value = self.value.text().trim().to_string();
                let header = self.header.text().trim().to_string();
                if value.is_empty() || (field == Field::Header && header.is_empty()) {
                    return None;
                }
                Some(Condition::Text { field, header, op: TEXT_OPS.get(op as usize)?.1, value })
            }
            (5, _) => Some(match op {
                0 => Condition::Attachments(true),
                1 => Condition::Attachments(false),
                2 => Condition::Dmarc(Dmarc::Pass),
                3 => Condition::Dmarc(Dmarc::Fail),
                _ => Condition::Dmarc(Dmarc::None),
            }),
            _ => Some(Condition::InContacts(op == 0)),
        }
    }
}

/// What the rule dialog ended with.
pub enum Edit {
    Save(Rule),
    Delete,
    Cancel,
}

/// The rule dialog. `folders` are the account's (full name, path) pairs;
/// `existing` adds Delete.
pub async fn edit(parent: &impl IsA<gtk::Widget>, account: &str, folders: &[(String, String)], rule: Rule, existing: bool) -> Edit {
    let page = adw::PreferencesPage::new();
    let name = adw::EntryRow::builder().title("Name").text(&rule.name).build();
    let naming = adw::PreferencesGroup::new();
    naming.add(&name);
    // The folders whose new mail the rule runs on; empty is INBOX.
    let scope: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(rule.scope.clone()));
    let run_on = adw::ActionRow::builder().title("Run on").activatable(true).build();
    run_on.set_subtitle(&glib::markup_escape_text(&scope_text(&rule.scope, folders)));
    run_on.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    {
        let (scope, row, folders) = (scope.clone(), run_on.clone(), folders.to_vec());
        run_on.connect_activated(move |r| {
            let (scope, row, folders) = (scope.clone(), row.clone(), folders.clone());
            let Some(root) = r.root() else { return };
            glib::spawn_future_local(async move {
                let current = scope.borrow().clone();
                let chosen = if current.is_empty() { vec!["INBOX".to_string()] } else { current };
                let Some(picked) = crate::manage::pick_folders(&root, "Run on", &folders, &chosen).await else { return };
                let picked = if picked.len() == 1 && picked[0].eq_ignore_ascii_case("INBOX") { Vec::new() } else { picked };
                row.set_subtitle(&glib::markup_escape_text(&scope_text(&picked, &folders)));
                *scope.borrow_mut() = picked;
            });
        });
    }
    naming.add(&run_on);
    page.add(&naming);

    let conditions = adw::PreferencesGroup::builder().title("Conditions").build();
    let matching = adw::ComboRow::builder()
        .title("Apply when")
        .model(&gtk::StringList::new(&["All conditions match", "Any condition matches"]))
        .selected(rule.any as u32)
        .build();
    conditions.add(&matching);
    let rows: Rc<RefCell<Vec<Rc<ConditionRow>>>> = Rc::default();
    let add = adw::ButtonRow::builder().title("Add _condition").use_underline(true).start_icon_name("list-add-symbolic").build();
    // In a group of its own, since a group only appends.
    let adding = adw::PreferencesGroup::new();
    adding.add(&add);
    let add_row = {
        let (conditions, rows) = (conditions.clone(), rows.clone());
        Rc::new(move |c: &Condition| {
            let row = ConditionRow::new(c);
            conditions.add(&row.row);
            rows.borrow_mut().push(row);
        })
    };
    let starting = if rule.conditions.is_empty() {
        vec![Condition::Text { field: Field::From, header: String::new(), op: TextOp::Contains, value: String::new() }]
    } else {
        rule.conditions.clone()
    };
    for c in &starting {
        add_row(c);
    }
    let a = add_row.clone();
    add.connect_activated(move |_| {
        a(&Condition::Text { field: Field::From, header: String::new(), op: TextOp::Contains, value: String::new() })
    });
    page.add(&conditions);
    page.add(&adding);

    let actions = adw::PreferencesGroup::builder().title("Actions").build();
    let read = adw::ComboRow::builder()
        .title("Read")
        .model(&gtk::StringList::new(&["Don't change", "Mark as read", "Mark as unread"]))
        .selected(match rule.read {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        })
        .build();
    let star = adw::ComboRow::builder()
        .title("Star")
        .model(&gtk::StringList::new(&["Don't change", "Star", "Unstar"]))
        .selected(match rule.star {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        })
        .build();
    // Archive, Junk, and Trash have entries of their own; INBOX, Drafts, and
    // Sent are no place to file mail.
    let special = salutations_core::SpecialFolders::load(account).unwrap_or_default();
    let skip: Vec<String> = [special.archive, special.junk, special.trash, special.drafts, special.sent].into_iter().flatten().collect();
    let places: Vec<(String, String)> =
        folders.iter().filter(|(f, _)| !f.eq_ignore_ascii_case("INBOX") && !skip.contains(f)).cloned().collect();
    let mut labels: Vec<String> = ["Don't change", "Archive", "Junk", "Trash"].map(String::from).to_vec();
    labels.extend(places.iter().map(|(_, p)| p.clone()));
    let file_at = match &rule.file {
        None => 0,
        Some(Filing::Archive) => 1,
        Some(Filing::Junk) => 2,
        Some(Filing::Trash) => 3,
        Some(Filing::Folder(f)) => places.iter().position(|(n, _)| n == f).map_or(0, |i| i + 4),
    };
    let file = adw::ComboRow::builder()
        .title("Move to")
        .model(&gtk::StringList::new(&labels.iter().map(String::as_str).collect::<Vec<_>>()))
        .selected(file_at as u32)
        .build();
    let stop = adw::SwitchRow::builder()
        .title("Stop processing more rules")
        .subtitle("Later rules are not tried on messages this rule matches")
        .active(rule.stop)
        .build();
    for row in [read.upcast_ref::<gtk::Widget>(), star.upcast_ref(), file.upcast_ref(), stop.upcast_ref()] {
        actions.add(row);
    }
    page.add(&actions);

    let delete = adw::ButtonRow::builder().title("_Delete rule").use_underline(true).build();
    delete.add_css_class("destructive-action");
    if existing {
        let g = adw::PreferencesGroup::new();
        g.add(&delete);
        page.add(&g);
    }

    let cancel = gtk::Button::with_mnemonic("_Cancel");
    let save = gtk::Button::with_mnemonic("_Save");
    save.add_css_class("suggested-action");
    let header = adw::HeaderBar::builder().show_start_title_buttons(false).show_end_title_buttons(false).build();
    header.pack_start(&cancel);
    header.pack_end(&save);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&page));
    let dialog = adw::Dialog::builder()
        .title(if existing { "Edit rule" } else { "New rule" })
        .content_width(480)
        .content_height(640)
        .child(&view)
        .build();

    // The rule as the dialog shows it; None while incomplete.
    let collect = {
        let (name, rows, matching) = (name.clone(), rows.clone(), matching.clone());
        let (read, star, file, stop) = (read.clone(), star.clone(), file.clone(), stop.clone());
        let scope = scope.clone();
        move || -> Option<Rule> {
            let name = name.text().trim().to_string();
            let live: Vec<Rc<ConditionRow>> = rows.borrow().iter().filter(|r| r.row.parent().is_some()).cloned().collect();
            let conditions: Option<Vec<Condition>> = live.iter().map(|r| r.condition()).collect();
            let conditions = conditions.filter(|c| !c.is_empty())?;
            let toggle = |i: u32| match i {
                1 => Some(true),
                2 => Some(false),
                _ => None,
            };
            let file = match file.selected() {
                0 => None,
                1 => Some(Filing::Archive),
                2 => Some(Filing::Junk),
                3 => Some(Filing::Trash),
                i => places.get(i as usize - 4).map(|(n, _)| Filing::Folder(n.clone())),
            };
            let rule = Rule {
                name,
                any: matching.selected() == 1,
                conditions,
                read: toggle(read.selected()),
                star: toggle(star.selected()),
                file,
                stop: stop.is_active(),
                scope: scope.borrow().clone(),
            };
            (!rule.name.is_empty() && rule.does_anything()).then_some(rule)
        }
    };
    let (tx, rx) = futures_channel::oneshot::channel::<Edit>();
    let tx = Rc::new(RefCell::new(Some(tx)));
    let finish = {
        let (tx, dialog) = (tx.clone(), dialog.clone());
        Rc::new(move |e: Edit| {
            if let Some(tx) = tx.borrow_mut().take() {
                let _ = tx.send(e);
            }
            dialog.close();
        })
    };
    let f = finish.clone();
    cancel.connect_clicked(move |_| f(Edit::Cancel));
    let f = finish.clone();
    delete.connect_activated(move |_| f(Edit::Delete));
    let (f, c) = (finish.clone(), collect.clone());
    save.connect_clicked(move |_| {
        if let Some(rule) = c() {
            f(Edit::Save(rule));
        }
    });
    // Save is offered once the rule has a name, complete conditions, and
    // an action.
    let update = {
        let (save, c) = (save.clone(), collect.clone());
        Rc::new(move || save.set_sensitive(c().is_some()))
    };
    update();
    // Edits anywhere in the dialog: a poll is simpler than connecting to
    // every condition row's widgets as they come and go.
    let (u, d) = (update.clone(), dialog.downgrade());
    glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
        if d.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        u();
        glib::ControlFlow::Continue
    });
    dialog.connect_closed(move |_| {
        if let Some(tx) = tx.borrow_mut().take() {
            let _ = tx.send(Edit::Cancel);
        }
    });
    dialog.present(Some(parent));
    rx.await.unwrap_or(Edit::Cancel)
}

/// A folder's path as shown, or its full name marked missing.
fn path_of(full_name: &str, folders: &[(String, String)]) -> String {
    folders.iter().find(|(n, _)| n == full_name).map_or_else(|| format!("{full_name} (missing)"), |(_, p)| p.clone())
}

/// The folders a rule runs on, as "Run on" shows them.
fn scope_text(scope: &[String], folders: &[(String, String)]) -> String {
    if scope.is_empty() {
        return "Inbox".to_string();
    }
    let names: Vec<String> = scope.iter().map(|f| path_of(f, folders)).collect();
    match names.len() {
        1 | 2 => names.join(" and "),
        n => format!("{}, and {} more", names[..2].join(", "), n - 2),
    }
}

/// What a rule does, for its row: "Mark as read, Move to Receipts"; "In
/// Lists: Mark as read" when it runs on folders other than Inbox.
pub fn summary(rule: &Rule, folders: &[(String, String)]) -> String {
    let mut parts: Vec<String> = Vec::new();
    match rule.read {
        Some(true) => parts.push("Mark as read".into()),
        Some(false) => parts.push("Mark as unread".into()),
        None => {}
    }
    match rule.star {
        Some(true) => parts.push("Star".into()),
        Some(false) => parts.push("Unstar".into()),
        None => {}
    }
    match &rule.file {
        Some(Filing::Archive) => parts.push("Archive".into()),
        Some(Filing::Junk) => parts.push("Move to Junk".into()),
        Some(Filing::Trash) => parts.push("Move to Trash".into()),
        Some(Filing::Folder(f)) => parts.push(format!("Move to {}", path_of(f, folders))),
        None => {}
    }
    if rule.stop {
        parts.push("then stop".into());
    }
    let mut s = parts.join(", ");
    if let Some(first) = s.get(..1) {
        s = first.to_uppercase() + &s[1..];
    }
    if rule.scope.is_empty() {
        s
    } else {
        let scope = match rule.scope.len() {
            1 | 2 => scope_text(&rule.scope, folders),
            n => format!("{n} folders"),
        };
        format!("In {scope}: {s}")
    }
}

/// Asks whether to run a just-saved rule on the messages already in INBOX.
pub async fn offer_apply(parent: &impl IsA<gtk::Widget>, account: &str, rule: Rule, apply: &ApplyRule) {
    let dialog = adw::AlertDialog::new(
        Some("Apply to messages in Inbox?"),
        Some("The rule runs on new mail as it arrives. It can also run once now on the messages already in Inbox."),
    );
    dialog.add_responses(&[("later", "_Not now"), ("apply", "_Apply")]);
    dialog.set_response_appearance("apply", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("apply"));
    dialog.set_close_response("later");
    if dialog.choose_future(Some(parent)).await == "apply" {
        apply(account, rule);
    }
}

fn save(account: &str, rules: &[Rule]) {
    if let Err(e) = rules::save(account, rules) {
        glib::g_warning!("salutations", "Could not save rules: {}", e);
    }
}

/// Adds `rule` at the end of the account's rules.
pub fn append(account: &str, rule: Rule) {
    let mut all = rules::load(account);
    all.push(rule);
    save(account, &all);
}

/// The Rules group of an account's preferences: its rules in order, each
/// opening the rule dialog, with up and down buttons, and New Rule.
pub fn group(account: &str, folders: Vec<(String, String)>, apply: ApplyRule) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Rules")
        .description("Run on new mail, in this order")
        .build();
    let state = Rc::new(GroupState {
        group: group.downgrade(),
        account: account.to_string(),
        folders,
        apply,
        rows: RefCell::new(Vec::new()),
    });
    state.rebuild();
    // The group keeps its state for as long as it lives.
    group.connect_destroy(move |_| {
        let _ = &state;
    });
    group
}

struct GroupState {
    group: glib::WeakRef<adw::PreferencesGroup>,
    account: String,
    folders: Vec<(String, String)>,
    apply: ApplyRule,
    rows: RefCell<Vec<gtk::Widget>>,
}

impl GroupState {
    fn rebuild(self: &Rc<Self>) {
        let Some(group) = self.group.upgrade() else { return };
        for row in self.rows.borrow_mut().drain(..) {
            detach(&row);
        }
        let all = rules::load(&self.account);
        let count = all.len();
        for (i, rule) in all.into_iter().enumerate() {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&rule.name))
                .subtitle(glib::markup_escape_text(&summary(&rule, &self.folders)))
                .activatable(true)
                .build();
            let up = gtk::Button::builder()
                .icon_name("go-up-symbolic")
                .tooltip_text("Move up")
                .sensitive(i > 0)
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            let down = gtk::Button::builder()
                .icon_name("go-down-symbolic")
                .tooltip_text("Move down")
                .sensitive(i + 1 < count)
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            for (button, delta) in [(&up, -1isize), (&down, 1)] {
                let weak = Rc::downgrade(self);
                button.connect_clicked(move |_| {
                    let Some(this) = weak.upgrade() else { return };
                    let mut all = rules::load(&this.account);
                    let j = i as isize + delta;
                    if j < 0 || j as usize >= all.len() {
                        return;
                    }
                    all.swap(i, j as usize);
                    save(&this.account, &all);
                    // Not from inside the button's handler: it is removed.
                    glib::idle_add_local_once(move || this.rebuild());
                });
            }
            row.add_suffix(&up);
            row.add_suffix(&down);
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            let weak = Rc::downgrade(self);
            row.connect_activated(move |row| {
                let (Some(this), row) = (weak.upgrade(), row.clone()) else { return };
                glib::spawn_future_local(async move { this.open(Some(i), &row).await });
            });
            group.add(&row);
            self.rows.borrow_mut().push(row.upcast());
        }
        let add = adw::ButtonRow::builder().title("_New rule").use_underline(true).start_icon_name("list-add-symbolic").build();
        let weak = Rc::downgrade(self);
        add.connect_activated(move |b| {
            let (Some(this), b) = (weak.upgrade(), b.clone()) else { return };
            glib::spawn_future_local(async move { this.open(None, &b).await });
        });
        group.add(&add);
        self.rows.borrow_mut().push(add.upcast());
    }

    /// Edits rule `index`, or a new one.
    async fn open(self: Rc<Self>, index: Option<usize>, parent: &impl IsA<gtk::Widget>) {
        let mut all = rules::load(&self.account);
        let rule = index.and_then(|i| all.get(i).cloned()).unwrap_or_default();
        match edit(parent, &self.account, &self.folders, rule, index.is_some()).await {
            Edit::Cancel => {}
            Edit::Delete => {
                if let Some(i) = index.filter(|i| *i < all.len()) {
                    all.remove(i);
                }
                save(&self.account, &all);
                self.rebuild();
            }
            Edit::Save(rule) => {
                match index.filter(|i| *i < all.len()) {
                    Some(i) => all[i] = rule.clone(),
                    None => all.push(rule.clone()),
                }
                save(&self.account, &all);
                self.rebuild();
                if let Some(group) = self.group.upgrade() {
                    offer_apply(&group, &self.account, rule, &self.apply).await;
                }
            }
        }
    }
}
