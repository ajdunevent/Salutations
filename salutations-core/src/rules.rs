//! Message rules: per account, an ordered list run by Salutations on new
//! mail in each rule's folders (INBOX unless chosen). Kept in `~/.config/salutations/rules.ini`, one
//! group per rule, named `<account UID>:<position>`.

use std::path::PathBuf;

use crate::render::Address;

/// What a text condition looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    From,
    /// Any address in To or Cc.
    ToCc,
    Subject,
    ListId,
    /// The header the condition names.
    Header,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextOp {
    StartsWith,
    EndsWith,
    Contains,
    NotContains,
    Is,
}

/// The provider's DMARC result for the sender's domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dmarc {
    Pass,
    Fail,
    /// No result, or one other than pass and fail.
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    /// Compared without regard to case. `header` is used with
    /// [`Field::Header`] only.
    Text { field: Field, header: String, op: TextOp, value: String },
    /// Has attachments (true) or has none (false).
    Attachments(bool),
    Dmarc(Dmarc),
    /// The sender is in an address book (true) or is not (false).
    InContacts(bool),
}

/// Where a rule files a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filing {
    /// A folder of the same account, by full name.
    Folder(String),
    Archive,
    Junk,
    Trash,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Rule {
    pub name: String,
    /// Any condition is enough; otherwise all must hold.
    pub any: bool,
    pub conditions: Vec<Condition>,
    /// Mark as read (true) or unread (false); None leaves it.
    pub read: Option<bool>,
    /// Star (true) or unstar (false); None leaves it.
    pub star: Option<bool>,
    pub file: Option<Filing>,
    /// Later rules are not tried on a message this rule matched.
    pub stop: bool,
    /// The folders whose new mail the rule runs on; empty means INBOX.
    pub scope: Vec<String>,
}

/// What the rules look at in one message.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub from: Vec<Address>,
    pub to_cc: Vec<Address>,
    pub subject: String,
    /// Every header, when a rule needs them (see [`Rule::needs_headers`]).
    pub headers: Vec<(String, String)>,
    pub attachments: bool,
    pub dmarc: Option<Dmarc>,
    pub in_contacts: bool,
}

/// What the matching rules do to a message, together.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    pub read: Option<bool>,
    pub star: Option<bool>,
    pub file: Option<Filing>,
}

impl Outcome {
    pub fn is_empty(&self) -> bool {
        self.read.is_none() && self.star.is_none() && self.file.is_none()
    }
}

fn text_matches(op: TextOp, value: &str, candidates: &[String]) -> bool {
    let want = value.trim().to_lowercase();
    let hit = |c: &String| {
        let c = c.trim().to_lowercase();
        match op {
            TextOp::StartsWith => c.starts_with(&want),
            TextOp::EndsWith => c.ends_with(&want),
            TextOp::Contains | TextOp::NotContains => c.contains(&want),
            TextOp::Is => c == want,
        }
    };
    let any = candidates.iter().any(hit);
    if op == TextOp::NotContains { !any } else { any }
}

/// Each address as its address, its name, and both together, so "is"
/// and "starts with" work on whichever the user typed.
fn address_texts(list: &[Address]) -> Vec<String> {
    let mut out = Vec::new();
    for a in list {
        out.push(a.email.clone());
        if let Some(n) = a.name.as_deref().filter(|n| !n.trim().is_empty()) {
            out.push(n.to_string());
            out.push(format!("{n} <{}>", a.email));
        }
    }
    out
}

impl Facts {
    fn header_values(&self, name: &str) -> Vec<String> {
        let name = name.trim();
        self.headers.iter().filter(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim().to_string()).collect()
    }
}

impl Condition {
    pub fn holds(&self, facts: &Facts) -> bool {
        match self {
            Condition::Text { field, header, op, value } => {
                let candidates = match field {
                    Field::From => address_texts(&facts.from),
                    Field::ToCc => address_texts(&facts.to_cc),
                    Field::Subject => vec![facts.subject.clone()],
                    Field::ListId => facts.header_values("List-Id"),
                    Field::Header => facts.header_values(header),
                };
                text_matches(*op, value, &candidates)
            }
            Condition::Attachments(has) => facts.attachments == *has,
            Condition::Dmarc(d) => facts.dmarc.unwrap_or(Dmarc::None) == *d,
            Condition::InContacts(known) => facts.in_contacts == *known,
        }
    }

    fn needs_headers(&self) -> bool {
        matches!(self, Condition::Text { field: Field::ListId | Field::Header, .. })
    }
}

impl Rule {
    /// A rule without conditions matches nothing.
    pub fn matches(&self, facts: &Facts) -> bool {
        if self.conditions.is_empty() {
            return false;
        }
        if self.any {
            self.conditions.iter().any(|c| c.holds(facts))
        } else {
            self.conditions.iter().all(|c| c.holds(facts))
        }
    }

    /// Some condition reads headers the folder summary does not keep, so
    /// the message is fetched.
    pub fn needs_headers(&self) -> bool {
        self.conditions.iter().any(Condition::needs_headers)
    }

    pub fn needs_contacts(&self) -> bool {
        self.conditions.iter().any(|c| matches!(c, Condition::InContacts(_)))
    }

    pub fn does_anything(&self) -> bool {
        self.read.is_some() || self.star.is_some() || self.file.is_some()
    }

    /// The folders the rule runs on.
    pub fn folders(&self) -> Vec<String> {
        if self.scope.is_empty() { vec!["INBOX".to_string()] } else { self.scope.clone() }
    }

    pub fn runs_in(&self, folder: &str) -> bool {
        self.folders().iter().any(|f| f.eq_ignore_ascii_case(folder))
    }
}

/// Runs `rules` in order on one message. Filing a message ends its run, as
/// does a matching rule set to stop; read and star take the last matching
/// rule's choice.
pub fn apply(rules: &[Rule], facts: &Facts) -> Outcome {
    let mut out = Outcome::default();
    for rule in rules {
        if !rule.matches(facts) {
            continue;
        }
        if rule.read.is_some() {
            out.read = rule.read;
        }
        if rule.star.is_some() {
            out.star = rule.star;
        }
        if rule.file.is_some() {
            out.file = rule.file.clone();
            break;
        }
        if rule.stop {
            break;
        }
    }
    out
}

// Storage

pub fn path() -> PathBuf {
    glib::user_config_dir().join("salutations").join("rules.ini")
}

fn field_key(f: Field) -> &'static str {
    match f {
        Field::From => "from",
        Field::ToCc => "to-cc",
        Field::Subject => "subject",
        Field::ListId => "list-id",
        Field::Header => "header",
    }
}

fn op_key(o: TextOp) -> &'static str {
    match o {
        TextOp::StartsWith => "starts-with",
        TextOp::EndsWith => "ends-with",
        TextOp::Contains => "contains",
        TextOp::NotContains => "not-contains",
        TextOp::Is => "is",
    }
}

fn dmarc_key(d: Dmarc) -> &'static str {
    match d {
        Dmarc::Pass => "pass",
        Dmarc::Fail => "fail",
        Dmarc::None => "none",
    }
}

fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// A condition as one tab-separated line.
fn encode(c: &Condition) -> String {
    let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    match c {
        Condition::Text { field: Field::Header, header, op, value } => {
            format!("header\t{}\t{}\t{}", clean(header), op_key(*op), clean(value))
        }
        Condition::Text { field, op, value, .. } => format!("{}\t{}\t{}", field_key(*field), op_key(*op), clean(value)),
        Condition::Attachments(b) => format!("attachments\t{}", yes(*b)),
        Condition::Dmarc(d) => format!("dmarc\t{}", dmarc_key(*d)),
        Condition::InContacts(b) => format!("contacts\t{}", yes(*b)),
    }
}

fn decode(line: &str) -> Option<Condition> {
    let parts: Vec<&str> = line.split('\t').collect();
    let op = |k: &str| {
        Some(match k {
            "starts-with" => TextOp::StartsWith,
            "ends-with" => TextOp::EndsWith,
            "contains" => TextOp::Contains,
            "not-contains" => TextOp::NotContains,
            "is" => TextOp::Is,
            _ => return None,
        })
    };
    let text = |field: Field, header: &str, o: &str, v: &str| {
        Some(Condition::Text { field, header: header.to_string(), op: op(o)?, value: v.to_string() })
    };
    match parts.as_slice() {
        ["header", h, o, v] => text(Field::Header, h, o, v),
        ["from", o, v] => text(Field::From, "", o, v),
        ["to-cc", o, v] => text(Field::ToCc, "", o, v),
        ["subject", o, v] => text(Field::Subject, "", o, v),
        ["list-id", o, v] => text(Field::ListId, "", o, v),
        ["attachments", b] => Some(Condition::Attachments(*b == "yes")),
        ["contacts", b] => Some(Condition::InContacts(*b == "yes")),
        ["dmarc", d] => Some(Condition::Dmarc(match *d {
            "pass" => Dmarc::Pass,
            "fail" => Dmarc::Fail,
            _ => Dmarc::None,
        })),
        _ => None,
    }
}

fn group_account(group: &str) -> Option<(&str, usize)> {
    let (uid, n) = group.rsplit_once(':')?;
    Some((uid, n.parse().ok()?))
}

fn read_rule(kf: &glib::KeyFile, group: &str) -> Rule {
    let s = |k: &str| kf.string(group, k).map(|v| v.to_string()).unwrap_or_default();
    let toggle = |k: &str, on: &str, off: &str| match s(k).as_str() {
        v if v == on => Some(true),
        v if v == off => Some(false),
        _ => None,
    };
    let file = match s("file").as_str() {
        "archive" => Some(Filing::Archive),
        "junk" => Some(Filing::Junk),
        "trash" => Some(Filing::Trash),
        f => f.strip_prefix("folder:").filter(|n| !n.is_empty()).map(|n| Filing::Folder(n.to_string())),
    };
    Rule {
        name: s("name"),
        any: s("match") == "any",
        conditions: kf
            .string_list(group, "conditions")
            .map(|l| l.iter().filter_map(|c| decode(c)).collect())
            .unwrap_or_default(),
        read: toggle("read", "read", "unread"),
        star: toggle("star", "star", "unstar"),
        file,
        stop: kf.boolean(group, "stop").unwrap_or(false),
        scope: kf.string_list(group, "scope").map(|l| l.iter().map(|f| f.to_string()).collect()).unwrap_or_default(),
    }
}

fn write_rule(kf: &glib::KeyFile, group: &str, r: &Rule) {
    kf.set_string(group, "name", &r.name);
    kf.set_string(group, "match", if r.any { "any" } else { "all" });
    let conditions: Vec<String> = r.conditions.iter().map(encode).collect();
    kf.set_value(group, "conditions", &crate::settings::raw_list(&conditions));
    let toggle = |v: Option<bool>, on: &'static str, off: &'static str| v.map_or("", |b| if b { on } else { off });
    kf.set_string(group, "read", toggle(r.read, "read", "unread"));
    kf.set_string(group, "star", toggle(r.star, "star", "unstar"));
    let file = match &r.file {
        None => String::new(),
        Some(Filing::Archive) => "archive".into(),
        Some(Filing::Junk) => "junk".into(),
        Some(Filing::Trash) => "trash".into(),
        Some(Filing::Folder(f)) => format!("folder:{f}"),
    };
    kf.set_string(group, "file", &file);
    kf.set_boolean(group, "stop", r.stop);
    if r.scope.is_empty() {
        let _ = kf.remove_key(group, "scope");
    } else {
        kf.set_value(group, "scope", &crate::settings::raw_list(&r.scope));
    }
}

fn load_file() -> glib::KeyFile {
    let kf = glib::KeyFile::new();
    let _ = kf.load_from_file(path(), glib::KeyFileFlags::KEEP_COMMENTS);
    kf
}

/// `rule` as text, for passing it between processes.
pub fn to_text(rule: &Rule) -> String {
    let kf = glib::KeyFile::new();
    write_rule(&kf, "rule", rule);
    kf.to_data().to_string()
}

pub fn from_text(text: &str) -> Option<Rule> {
    let kf = glib::KeyFile::new();
    kf.load_from_data(text, glib::KeyFileFlags::NONE).ok()?;
    kf.has_group("rule").then(|| read_rule(&kf, "rule"))
}

/// The account's rules, in order.
pub fn load(account: &str) -> Vec<Rule> {
    let kf = load_file();
    let mut found: Vec<(usize, Rule)> = kf
        .groups()
        .iter()
        .filter_map(|g| {
            let (uid, n) = group_account(g)?;
            (uid == account).then(|| (n, read_rule(&kf, g)))
        })
        .collect();
    found.sort_by_key(|(n, _)| *n);
    found.into_iter().map(|(_, r)| r).collect()
}

/// Follows a folder renamed or moved from `old` to `new` in the
/// account's rules: where they file and the folders they run on.
pub fn folder_renamed(account: &str, old: &str, new: &str) -> Result<(), glib::Error> {
    let mut rules = load(account);
    let mut changed = false;
    for r in rules.iter_mut() {
        if let Some(Filing::Folder(f)) = &mut r.file
            && let Some(p) = crate::renamed_path(f, old, new)
        {
            *f = p;
            changed = true;
        }
        for f in r.scope.iter_mut() {
            if let Some(p) = crate::renamed_path(f, old, new) {
                *f = p;
                changed = true;
            }
        }
    }
    if changed { save(account, &rules) } else { Ok(()) }
}

/// Replaces the account's rules.
pub fn save(account: &str, rules: &[Rule]) -> Result<(), glib::Error> {
    let kf = load_file();
    for g in kf.groups().iter() {
        if group_account(g).is_some_and(|(uid, _)| uid == account) {
            let _ = kf.remove_group(g);
        }
    }
    for (i, r) in rules.iter().enumerate() {
        write_rule(&kf, &format!("{account}:{i}"), r);
    }
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    kf.save_to_file(&p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(name: &str, email: &str) -> Address {
        Address { name: (!name.is_empty()).then(|| name.to_string()), email: email.to_string() }
    }

    fn text(field: Field, op: TextOp, value: &str) -> Condition {
        Condition::Text { field, header: String::new(), op, value: value.into() }
    }

    fn facts() -> Facts {
        Facts {
            from: vec![addr("Northside Library", "news@northside.example")],
            to_cc: vec![addr("Jordan", "jordan@example.org"), addr("", "family@example.org")],
            subject: "November at the library".into(),
            headers: vec![("List-Id".into(), "Library news <news.northside.example>".into()), ("X-Spam".into(), "no".into())],
            attachments: false,
            dmarc: Some(Dmarc::Pass),
            in_contacts: false,
        }
    }

    #[test]
    fn text_conditions() {
        let f = facts();
        assert!(text(Field::From, TextOp::EndsWith, "@NORTHSIDE.example").holds(&f));
        assert!(text(Field::From, TextOp::Is, "northside library").holds(&f));
        assert!(text(Field::From, TextOp::Is, "news@northside.example").holds(&f));
        assert!(!text(Field::From, TextOp::Is, "northside").holds(&f));
        assert!(text(Field::ToCc, TextOp::StartsWith, "family@").holds(&f));
        assert!(text(Field::ToCc, TextOp::NotContains, "boss@").holds(&f));
        assert!(!text(Field::ToCc, TextOp::NotContains, "jordan").holds(&f));
        assert!(text(Field::Subject, TextOp::Contains, "library").holds(&f));
        assert!(text(Field::ListId, TextOp::Contains, "news.northside.example").holds(&f));
        let header = |name: &str, op, v: &str| Condition::Text { field: Field::Header, header: name.into(), op, value: v.into() };
        assert!(header("x-spam", TextOp::Is, "no").holds(&f));
        // A missing header contains nothing.
        assert!(header("X-Other", TextOp::NotContains, "a").holds(&f));
        assert!(!header("X-Other", TextOp::Contains, "").holds(&f));
    }

    #[test]
    fn other_conditions() {
        let f = facts();
        assert!(Condition::Attachments(false).holds(&f));
        assert!(Condition::Dmarc(Dmarc::Pass).holds(&f));
        assert!(Condition::Dmarc(Dmarc::None).holds(&Facts::default()));
        assert!(Condition::InContacts(false).holds(&f));
    }

    #[test]
    fn rules_in_order() {
        let f = facts();
        let read = Rule { conditions: vec![text(Field::From, TextOp::Contains, "northside")], read: Some(true), ..Default::default() };
        let star = Rule { conditions: vec![text(Field::Subject, TextOp::Contains, "november")], star: Some(true), ..Default::default() };
        let file = Rule {
            conditions: vec![text(Field::Subject, TextOp::Contains, "x"), Condition::Attachments(false)],
            any: true,
            file: Some(Filing::Folder("Lists".into())),
            ..Default::default()
        };
        let unread = Rule { conditions: vec![Condition::Attachments(false)], read: Some(false), ..Default::default() };
        assert_eq!(
            apply(&[read.clone(), star.clone(), file.clone(), unread.clone()], &f),
            Outcome { read: Some(true), star: Some(true), file: Some(Filing::Folder("Lists".into())) }
        );
        let stop = Rule { stop: true, ..read.clone() };
        assert_eq!(apply(&[stop, star], &f), Outcome { read: Some(true), ..Default::default() });
        assert!(apply(&[Rule { read: Some(true), ..Default::default() }], &f).is_empty());
        assert!(!read.needs_headers());
        assert!(Rule { conditions: vec![text(Field::ListId, TextOp::Is, "x")], ..Default::default() }.needs_headers());
    }

    #[test]
    fn encoding() {
        let conditions = vec![
            text(Field::From, TextOp::Is, "a\tb"),
            Condition::Text { field: Field::Header, header: "X-Spam".into(), op: TextOp::NotContains, value: "yes; maybe".into() },
            Condition::Attachments(true),
            Condition::Dmarc(Dmarc::Fail),
            Condition::InContacts(false),
        ];
        for c in &conditions {
            let back = decode(&encode(c)).expect("decodes");
            if let Condition::Text { field: Field::From, value, .. } = &back {
                assert_eq!(value, "a b");
            } else {
                assert_eq!(&back, c);
            }
        }
        let kf = glib::KeyFile::new();
        let rule = Rule {
            name: "Lists; and more".into(),
            any: true,
            conditions: conditions[1..].to_vec(),
            read: Some(false),
            star: None,
            file: Some(Filing::Folder("Lists/Library".into())),
            stop: true,
            scope: vec!["Lists".into(), "Odd; name".into()],
        };
        write_rule(&kf, "acct:0", &rule);
        let kf2 = glib::KeyFile::new();
        kf2.load_from_data(&kf.to_data(), glib::KeyFileFlags::NONE).unwrap();
        assert_eq!(read_rule(&kf2, "acct:0"), rule);
        assert_eq!(group_account("a:b:12"), Some(("a:b", 12)));
        assert!(rule.runs_in("Odd; name") && !rule.runs_in("INBOX"));
        assert!(Rule::default().runs_in("inbox"));
    }
}
