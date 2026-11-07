//! Outgoing mail: building the MIME message from what the composer holds,
//! and the text a reply or forward starts with.
//!
//! Plain text is sent as `format=flowed` (RFC 3676). Markdown is sent as
//! `multipart/alternative`: its source as the flowed plain-text part and the
//! rendered HTML as the second part.

use std::sync::LazyLock;

use camel::prelude::*;
use camel::{InternetAddress, MimeMessage, MimePart, Multipart};
use regex::Regex;

use crate::render::{self, Address};

/// Lines are wrapped to this many characters, quote markers included.
const WIDTH: usize = 72;

/// Records the composer's format in a saved draft, so reopening it restores
/// the toggle; removed before sending.
pub const FORMAT_HEADER: &str = "X-Salutations-Format";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    #[default]
    Markdown,
    Plain,
}

impl Format {
    pub fn key(self) -> &'static str {
        match self {
            Format::Markdown => "markdown",
            Format::Plain => "plain",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Format::Markdown => "Markdown",
            Format::Plain => "Plain text",
        }
    }

    pub fn from_key(key: &str) -> Option<Format> {
        match key.trim() {
            "markdown" => Some(Format::Markdown),
            "plain" => Some(Format::Plain),
            _ => None,
        }
    }
}

/// A file added to the message.
#[derive(Debug, Clone)]
pub struct Attachment {
    pub name: String,
    pub mime_type: String,
    pub data: glib::Bytes,
}

/// What the composer holds.
#[derive(Debug, Clone, Default)]
pub struct Outgoing {
    pub from: Address,
    /// The recipient fields as typed: comma-separated addresses.
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub format: Format,
    /// Without angle brackets; kept across draft saves so the draft and the
    /// sent message are the same message.
    pub message_id: String,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub attachments: Vec<Attachment>,
    /// Forwarded as an attachment (`message/rfc822`).
    pub forwarded: Option<MimeMessage>,
    /// The value of the Autocrypt header carrying the sender's key.
    pub autocrypt: Option<String>,
    /// Sign and encrypt with the account's key (the composer's toggles).
    pub sign: bool,
    pub encrypt: bool,
    /// Marked high importance.
    pub important: bool,
    /// The Message-ID (without angle brackets) of the message this one
    /// forwards, sent as `X-Forwarded-Message-Id` as Thunderbird does, so
    /// the original can find its forward in Sent.
    pub forwarded_id: Option<String>,
}

/// Names the message a forward forwards.
pub const FORWARDED_HEADER: &str = "X-Forwarded-Message-Id";

/// Whether a message's `Importance` or `X-Priority` header marks it high
/// importance. X-Priority runs from 1 (highest) to 5; 1 and 2 are high.
pub fn is_important(importance: Option<&str>, x_priority: Option<&str>) -> bool {
    importance.is_some_and(|v| v.trim().eq_ignore_ascii_case("high"))
        || x_priority.and_then(|v| v.trim().chars().next()).is_some_and(|c| c == '1' || c == '2')
}

/// Records Sign and Encrypt in a saved draft; removed before sending.
pub const CRYPTO_HEADER: &str = "X-Salutations-Crypto";

/// A recipient field that does not parse as a list of addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadAddress(pub String);

/// Parses a typed recipient field. Empty fields give an empty list.
pub fn parse_addresses(field: &str) -> Result<Vec<Address>, BadAddress> {
    let field = field.trim().trim_end_matches([',', ';']).trim();
    if field.is_empty() {
        return Ok(Vec::new());
    }
    let list = InternetAddress::new();
    let n = list.unformat(&field.replace(';', ","));
    let parsed: Vec<Address> = (0..n.max(0))
        .filter_map(|i| list.get(i))
        .map(|(name, email)| Address {
            name: name.map(|n| n.to_string()).filter(|n| !n.is_empty()),
            email: email.map(|e| e.to_string()).unwrap_or_default(),
        })
        .collect();
    let bad = parsed.iter().find(|a| !valid_email(&a.email));
    match (n, bad) {
        (n, _) if n <= 0 => Err(BadAddress(field.to_string())),
        (_, Some(a)) => Err(BadAddress(if a.email.is_empty() { field.to_string() } else { a.email.clone() })),
        _ => Ok(parsed),
    }
}

fn valid_email(email: &str) -> bool {
    match email.rsplit_once('@') {
        Some((local, domain)) => !local.is_empty() && domain.contains('.') && !email.contains(char::is_whitespace),
        None => false,
    }
}

fn address_list(addrs: &[Address]) -> InternetAddress {
    let list = InternetAddress::new();
    for a in addrs {
        list.add(a.name.as_deref().unwrap_or(""), &a.email);
    }
    list
}

/// "Name <address>" for one address; the address alone without a name.
pub fn format_address(a: &Address) -> String {
    let list = InternetAddress::new();
    list.add(a.name.as_deref().unwrap_or(""), &a.email);
    list.format().map(|s| s.to_string()).unwrap_or_else(|| a.email.clone())
}

pub fn format_addresses(addrs: &[Address]) -> String {
    addrs.iter().map(format_address).collect::<Vec<_>>().join(", ")
}

impl Outgoing {
    /// Every recipient, Bcc included, for the transport.
    pub fn recipients(&self) -> Result<InternetAddress, BadAddress> {
        let mut all = parse_addresses(&self.to)?;
        all.extend(parse_addresses(&self.cc)?);
        all.extend(parse_addresses(&self.bcc)?);
        Ok(address_list(&all))
    }

    /// Every recipient's address, lowercased; unparsable fields give none.
    pub fn recipient_emails(&self) -> Vec<String> {
        [&self.to, &self.cc, &self.bcc]
            .into_iter()
            .flat_map(|f| parse_addresses(f).unwrap_or_default())
            .map(|a| a.email.to_ascii_lowercase())
            .collect()
    }

    pub fn from_address(&self) -> InternetAddress {
        address_list(std::slice::from_ref(&self.from))
    }

    /// The message to send (`draft` false) or to save in Drafts (`draft`
    /// true). Bcc is kept in both: the SMTP transport removes it from what
    /// it transmits, and Sent keeps it for the user. Unparsable recipient
    /// fields are kept as typed in a draft and refused for sending.
    pub fn build(&self, draft: bool) -> Result<MimeMessage, BadAddress> {
        let msg = self.headers(draft)?;
        set_body(&msg, &self.content_part());
        Ok(msg)
    }

    /// The message's headers without its content; see [`set_body`].
    pub fn headers(&self, draft: bool) -> Result<MimeMessage, BadAddress> {
        let msg = MimeMessage::new();
        msg.set_from(Some(&self.from_address()));
        for (kind, field) in [("To", &self.to), ("Cc", &self.cc), ("Bcc", &self.bcc)] {
            match parse_addresses(field) {
                Ok(list) if !list.is_empty() => msg.set_recipients(kind, Some(&address_list(&list))),
                Ok(_) => {}
                Err(_) if draft => msg.set_header(kind, Some(field.trim())),
                Err(e) => return Err(e),
            }
        }
        msg.set_subject(Some(&self.subject));
        msg.set_date(camel::ffi::CAMEL_MESSAGE_DATE_CURRENT as _, 0);
        msg.set_message_id(Some(&self.message_id));
        if let Some(id) = &self.in_reply_to {
            msg.set_header("In-Reply-To", Some(&format!("<{id}>")));
        }
        if !self.references.is_empty() {
            let refs: Vec<String> = self.references.iter().map(|r| format!("<{r}>")).collect();
            msg.set_header("References", Some(&refs.join(" ")));
        }
        if let Some(id) = &self.forwarded_id {
            msg.set_header(FORWARDED_HEADER, Some(&format!("<{id}>")));
        }
        msg.set_header("User-Agent", Some(concat!("Salutations/", env!("CARGO_PKG_VERSION"))));
        if self.important {
            // Outlook reads Importance; most other clients read X-Priority.
            msg.set_header("Importance", Some("high"));
            msg.set_header("X-Priority", Some("1 (Highest)"));
        }
        if draft {
            msg.set_header(FORMAT_HEADER, Some(self.format.key()));
            let flags: Vec<&str> = [(self.sign, "sign"), (self.encrypt, "encrypt")]
                .into_iter()
                .filter_map(|(on, k)| on.then_some(k))
                .collect();
            msg.set_header(CRYPTO_HEADER, Some(&flags.join(",")));
        }
        if let Some(value) = &self.autocrypt {
            msg.set_header("Autocrypt", Some(value));
        }
        Ok(msg)
    }

    /// Everything under the headers: the text, and the attachments with it.
    /// Signing and encryption wrap this part.
    pub fn content_part(&self) -> MimePart {
        let body = self.body_part();
        if self.attachments.is_empty() && self.forwarded.is_none() {
            return body;
        }
        let mixed = Multipart::new();
        mixed.set_mime_type("multipart/mixed");
        mixed.set_boundary(None);
        mixed.add_part(&body);
        for a in &self.attachments {
            mixed.add_part(&attachment_part(a));
        }
        if let Some(fwd) = &self.forwarded {
            let part = MimePart::new();
            MediumExt::set_content(&part, Some(fwd));
            part.set_content_type(Some("message/rfc822"));
            part.set_disposition(Some("attachment"));
            let name = fwd.subject().map(|s| s.to_string()).filter(|s| !s.trim().is_empty());
            part.set_filename(Some(&format!("{}.eml", safe_filename(name.as_deref().unwrap_or("Forwarded message")))));
            mixed.add_part(&part);
        }
        let part = MimePart::new();
        MediumExt::set_content(&part, Some(&mixed));
        part
    }

    fn body_part(&self) -> MimePart {
        let plain = text_part(&flowed(&self.body), "text/plain; charset=utf-8; format=flowed");
        if self.format == Format::Plain {
            return plain;
        }
        let html = text_part(&markdown_html(&self.body), "text/html; charset=utf-8");
        let alt = Multipart::new();
        alt.set_mime_type("multipart/alternative");
        alt.set_boundary(None);
        alt.add_part(&plain);
        alt.add_part(&html);
        let part = MimePart::new();
        MediumExt::set_content(&part, Some(&alt));
        part
    }
}

/// Makes `part` the message's content: its data and, for a single part,
/// its transfer encoding.
pub fn set_body(msg: &MimeMessage, part: &MimePart) {
    let content = MediumExt::content(part);
    let single = content.as_ref().is_some_and(|c| !c.is::<Multipart>());
    MediumExt::set_content(msg, content.as_ref());
    if single {
        MimePartExt::set_encoding(msg, MimePartExt::encoding(part));
    }
}

fn text_part(text: &str, content_type: &str) -> MimePart {
    let part = MimePart::new();
    MimePartExt::set_content(&part, text.as_bytes(), Some(content_type));
    // 7bit only when every line is short ASCII; RFC 5322 limits lines to
    // 998 octets and flowed text can keep long words.
    let plain_ascii = text.is_ascii() && text.lines().all(|l| l.len() <= 76);
    MimePartExt::set_encoding(&part, if plain_ascii {
        camel::TransferEncoding::Encoding7bit
    } else {
        camel::TransferEncoding::EncodingQuotedprintable
    });
    part
}

fn attachment_part(a: &Attachment) -> MimePart {
    let part = MimePart::new();
    MimePartExt::set_content(&part, &a.data, Some(&a.mime_type));
    part.set_filename(Some(&a.name));
    part.set_disposition(Some("attachment"));
    // An armored key is ASCII and stays readable as sent.
    let encoding = if a.mime_type == crate::crypto::KEY_MIME_TYPE {
        camel::TransferEncoding::Encoding7bit
    } else {
        camel::TransferEncoding::EncodingBase64
    };
    MimePartExt::set_encoding(&part, encoding);
    part
}

fn safe_filename(s: &str) -> String {
    s.chars().map(|c| if matches!(c, '/' | '\\' | ':' | '\0') { '_' } else { c }).take(80).collect()
}

/// A new Message-ID for mail from `from`.
pub fn new_message_id(from: &str) -> String {
    let domain = from.rsplit_once('@').map(|(_, d)| d).filter(|d| !d.is_empty());
    camel::header_msgid_generate(domain).to_string()
}

// format=flowed

/// Splits a line into its quote depth and the text after the markers.
/// Markers may be separated by single spaces ("> > text"), as typed.
fn split_quote(line: &str) -> (usize, &str) {
    let mut depth = 0;
    let mut rest = line;
    loop {
        if let Some(r) = rest.strip_prefix('>') {
            depth += 1;
            rest = r;
        } else if depth > 0 && rest.starts_with(" >") {
            rest = &rest[1..];
        } else {
            break;
        }
    }
    if depth > 0 {
        rest = rest.strip_prefix(' ').unwrap_or(rest);
    }
    (depth, rest)
}

/// Encodes text as format=flowed with lines of at most [`WIDTH`]
/// characters where words allow.
pub fn flowed(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in text.replace("\r\n", "\n").split('\n') {
        // The signature separator is never flowed or trimmed.
        if line == "-- " {
            out.push(line.to_string());
            continue;
        }
        let (depth, content) = split_quote(line);
        let content = content.trim_end();
        let prefix = if depth > 0 { format!("{} ", ">".repeat(depth)) } else { String::new() };
        let width = WIDTH.saturating_sub(prefix.chars().count()).max(20);
        let pieces = wrap(content, width);
        let last = pieces.len() - 1;
        for (i, piece) in pieces.into_iter().enumerate() {
            let mut l = prefix.clone();
            // Space-stuffing (RFC 3676 4.4); quoted lines already have
            // their space after the markers.
            if depth == 0 && (piece.starts_with(' ') || piece.starts_with('>') || piece.starts_with("From ")) {
                l.push(' ');
            }
            l.push_str(&piece);
            if i != last {
                l.push(' ');
            }
            out.push(l);
        }
    }
    out.join("\n")
}

/// Breaks `text` at spaces into pieces of at most `width` characters; each
/// break replaces one space. Longer words stay whole.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut current = String::new();
    for (i, word) in text.split(' ').enumerate() {
        if i == 0 {
            current.push_str(word);
        } else if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > width {
            pieces.push(std::mem::replace(&mut current, word.to_string()));
        } else {
            current.push(' ');
            current.push_str(word);
        }
    }
    pieces.push(current);
    pieces
}

/// Decodes format=flowed text into one line per paragraph, quote markers
/// written as "> ".
pub fn unflow(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut open: Option<(usize, String)> = None;
    for raw in text.replace("\r\n", "\n").split('\n') {
        let depth = raw.chars().take_while(|&c| c == '>').count();
        let mut content = &raw[depth..];
        content = content.strip_prefix(' ').unwrap_or(content);
        let soft = content.ends_with(' ') && raw != "-- ";
        match open.take() {
            Some((d, mut acc)) if d == depth => {
                acc.push_str(content);
                if soft {
                    open = Some((d, acc));
                } else {
                    out.push(quoted(d, &acc));
                }
            }
            prev => {
                if let Some((d, acc)) = prev {
                    out.push(quoted(d, acc.trim_end()));
                }
                if soft {
                    open = Some((depth, content.to_string()));
                } else {
                    out.push(quoted(depth, content));
                }
            }
        }
    }
    if let Some((d, acc)) = open {
        out.push(quoted(d, acc.trim_end()));
    }
    out.join("\n")
}

fn quoted(depth: usize, text: &str) -> String {
    if depth == 0 {
        text.to_string()
    } else {
        format!("{}{}", "> ".repeat(depth), text)
    }
}

// Markdown

/// Renders Markdown to an HTML document. Line breaks are kept, as people
/// expect in mail, and HTML written in the text is shown as text.
pub fn markdown_html(text: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser};
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let events = Parser::new_ext(text, options).map(|e| match e {
        Event::SoftBreak => Event::HardBreak,
        // Quoted mail can contain markup; it must not become live HTML.
        Event::Html(h) | Event::InlineHtml(h) => Event::Text(h),
        e => e,
    });
    let mut body = String::new();
    pulldown_cmark::html::push_html(&mut body, events);
    format!(
        "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\"><style>\
         blockquote{{margin:0 0 0 .8ex;border-left:2px solid #ccc;padding-left:1ex}}\
         pre{{white-space:pre-wrap}}</style></head><body>\n{body}</body></html>\n"
    )
}

// Replies and forwards

/// What a reply or forward needs from the original message.
#[derive(Debug, Clone, Default)]
pub struct Original {
    pub from: Vec<Address>,
    pub reply_to: Vec<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub subject: String,
    /// Seconds since the epoch.
    pub date: Option<i64>,
    pub message_id: Option<String>,
    pub references: Vec<String>,
    /// The body as plain text, from its text/plain part or converted from
    /// HTML.
    pub text: String,
    /// The original was HTML-only or offered HTML.
    pub has_html: bool,
}

fn addresses(list: Option<InternetAddress>) -> Vec<Address> {
    let Some(list) = list else { return Vec::new() };
    (0..list.length())
        .filter_map(|i| list.get(i))
        .filter_map(|(name, email)| {
            Some(Address { name: name.map(|n| n.to_string()).filter(|n| !n.is_empty()), email: email?.to_string() })
        })
        .collect()
}

fn message_ids(header: &str) -> Vec<String> {
    static ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<([^<>\s]+)>").expect("valid regex"));
    ID.captures_iter(header).map(|c| c[1].to_string()).collect()
}

impl Original {
    pub fn from_message(msg: &MimeMessage) -> Original {
        Self::from_parts(msg, None)
    }

    /// [`Original::from_message`] with the body from `body`, such as the
    /// decrypted content of an encrypted message.
    pub fn from_parts(msg: &MimeMessage, body: Option<&MimePart>) -> Original {
        let (date, _) = msg.date();
        let (text, has_html) = render::reply_text_in(body.unwrap_or(msg.upcast_ref()));
        Original {
            from: addresses(msg.from()),
            reply_to: addresses(msg.reply_to()),
            to: addresses(msg.recipients("To")),
            cc: addresses(msg.recipients("Cc")),
            subject: msg.subject().map(|s| s.to_string()).unwrap_or_default(),
            date: (date > 0).then_some(date),
            message_id: msg
                .message_id()
                .map(|m| m.trim_matches(['<', '>', ' ']).to_string())
                .filter(|m| !m.is_empty()),
            references: msg.header("References").map(|r| message_ids(&r)).unwrap_or_default(),
            text,
            has_html,
        }
    }
}

fn is_own(a: &Address, own: &[String]) -> bool {
    own.iter().any(|o| o.eq_ignore_ascii_case(&a.email))
}

fn push_unique(list: &mut Vec<Address>, a: &Address) {
    if !list.iter().any(|x| x.email.eq_ignore_ascii_case(&a.email)) {
        list.push(a.clone());
    }
}

/// To and Cc of a reply. `own` holds the account's address and aliases,
/// which never receive the reply unless the user wrote the original only
/// to themselves.
pub fn reply_recipients(orig: &Original, all: bool, own: &[String]) -> (Vec<Address>, Vec<Address>) {
    let from_me = orig.from.iter().all(|a| is_own(a, own)) && !orig.from.is_empty();
    // Replying to one's own message (from Sent) continues to its recipients.
    let primary: Vec<Address> = if from_me {
        orig.to.clone()
    } else if !orig.reply_to.is_empty() {
        orig.reply_to.clone()
    } else {
        orig.from.clone()
    };
    let mut to = Vec::new();
    for a in &primary {
        push_unique(&mut to, a);
    }
    let mut cc = Vec::new();
    if all {
        if !from_me {
            for a in orig.to.iter().filter(|a| !is_own(a, own)) {
                push_unique(&mut to, a);
            }
        }
        for a in orig.cc.iter().filter(|a| !is_own(a, own)) {
            if !to.iter().any(|t| t.email.eq_ignore_ascii_case(&a.email)) {
                push_unique(&mut cc, a);
            }
        }
    }
    if to.iter().any(|a| !is_own(a, own)) {
        to.retain(|a| !is_own(a, own));
    }
    (to, cc)
}

fn prefixed(prefix: &str, subject: &str) -> String {
    let trimmed = subject.trim();
    let lower = trimmed.to_lowercase();
    let p = prefix.to_lowercase();
    if lower.starts_with(&format!("{p}:")) || lower.starts_with(&format!("{p} :")) {
        trimmed.to_string()
    } else {
        format!("{prefix}: {trimmed}")
    }
}

pub fn reply_subject(subject: &str) -> String {
    prefixed("Re", subject)
}

pub fn forward_subject(subject: &str) -> String {
    prefixed("Fwd", subject)
}

/// In-Reply-To and References for a reply to `orig`.
pub fn threading(orig: &Original) -> (Option<String>, Vec<String>) {
    let mut refs = orig.references.clone();
    if let Some(id) = &orig.message_id
        && !refs.contains(id)
    {
        refs.push(id.clone());
    }
    (orig.message_id.clone(), refs)
}

/// The header lines naming the original, as shown under the separator.
fn header_block(orig: &Original, date: &str, with_to: bool) -> String {
    let mut lines = vec![format!("From: {}", format_addresses(&orig.from))];
    if !date.is_empty() {
        lines.push(format!("Date: {date}"));
    }
    if with_to && !orig.to.is_empty() {
        lines.push(format!("To: {}", format_addresses(&orig.to)));
    }
    if !orig.cc.is_empty() {
        lines.push(format!("Cc: {}", format_addresses(&orig.cc)));
    }
    lines.push(format!("Subject: {}", orig.subject));
    lines.join("\n")
}

/// Prefixes every line with "> ", keeping existing quotes one level deeper.
pub fn quote(text: &str) -> String {
    text.trim_end()
        .lines()
        .map(|l| if l.is_empty() { ">".to_string() } else if l.starts_with('>') { format!(">{l}") } else { format!("> {l}") })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The quoted original under its "--- Previous Message ---" block. `date`
/// is formatted by the caller in the user's clock format.
pub fn reply_quote(orig: &Original, date: &str) -> String {
    format!("--- Previous Message ---\n{}\n\n{}\n", header_block(orig, date, false), quote(&orig.text))
}

/// The original, unquoted, under its "--- Forwarded Message ---" block.
pub fn forward_inline(orig: &Original, date: &str) -> String {
    format!("--- Forwarded Message ---\n{}\n\n{}\n", header_block(orig, date, true), orig.text.trim_end())
}

/// Inserts `signature` into a body that may end with a quoted block, at
/// the top (`above_quote`, after the user's text) or at the end.
pub fn with_signature(body: &str, signature: &str, above_quote: bool) -> String {
    let sig = signature.trim_end();
    if sig.is_empty() {
        return body.to_string();
    }
    let marker = ["--- Previous Message ---", "--- Forwarded Message ---"]
        .iter()
        .filter_map(|m| body.find(m))
        .min();
    match marker {
        Some(at) if above_quote => {
            let (top, rest) = body.split_at(at);
            format!("{}\n\n{sig}\n\n{rest}", top.trim_end())
        }
        _ => format!("{}\n\n{sig}\n", body.trim_end()),
    }
}

// HTML to text, for quoting HTML-only mail

/// Converts HTML to readable plain text: block elements become line breaks,
/// list items get "- ", blockquotes become "> " quotes, and scripts, styles
/// and the head are dropped.
pub fn html_to_text(html: &str) -> String {
    static TAG: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?s)<!--.*?-->|<(/?)([a-zA-Z][a-zA-Z0-9]*)\b[^>]*?(/?)>").expect("valid regex"));
    let mut out = String::new();
    let mut depth = 0usize;
    let mut skip: Option<String> = None;
    let mut pre = 0usize;
    let mut last = 0;
    let newline = |out: &mut String, depth: usize| {
        let trimmed = out.trim_end_matches(' ').len();
        out.truncate(trimmed);
        out.push('\n');
        out.push_str(&"> ".repeat(depth));
    };
    for m in TAG.captures_iter(html) {
        let whole = m.get(0).expect("match");
        if skip.is_none() {
            push_text(&mut out, &html[last..whole.start()], pre > 0, depth);
        }
        last = whole.end();
        let Some(name) = m.get(2).map(|n| n.as_str().to_ascii_lowercase()) else { continue };
        let closing = !m[1].is_empty();
        if let Some(s) = &skip {
            if closing && *s == name {
                skip = None;
            }
            continue;
        }
        match (name.as_str(), closing) {
            ("head" | "style" | "script" | "title", false) if m[3].is_empty() => skip = Some(name),
            ("br", _) => newline(&mut out, depth),
            ("blockquote", false) => {
                depth += 1;
                newline(&mut out, depth);
            }
            ("blockquote", true) => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
            }
            ("pre", false) => {
                pre += 1;
                newline(&mut out, depth);
            }
            ("pre", true) => {
                pre = pre.saturating_sub(1);
                newline(&mut out, depth);
            }
            ("li", false) => {
                newline(&mut out, depth);
                out.push_str("- ");
            }
            ("p" | "div" | "tr" | "table" | "ul" | "ol" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "hr" | "section"
            | "article" | "header" | "footer", _) => newline(&mut out, depth),
            ("td" | "th", true) => out.push(' '),
            _ => {}
        }
    }
    if skip.is_none() {
        push_text(&mut out, &html[last..], pre > 0, depth);
    }
    tidy(&out)
}

fn push_text(out: &mut String, raw: &str, pre: bool, depth: usize) {
    let text = unescape(raw);
    if pre {
        out.push_str(&text.replace('\n', &format!("\n{}", "> ".repeat(depth))));
        return;
    }
    for c in text.chars() {
        if c.is_whitespace() && c != '\u{a0}' {
            if !out.ends_with([' ', '\n']) && !out.is_empty() {
                out.push(' ');
            }
        } else {
            out.push(if c == '\u{a0}' { ' ' } else { c });
        }
    }
}

/// Trims each line and keeps a blank line only between two lines of the
/// same quote depth, never two in a row.
fn tidy(text: &str) -> String {
    let lines: Vec<(usize, String)> = text
        .lines()
        .map(|l| {
            let depth = l.chars().take_while(|c| *c == '>' || *c == ' ').filter(|c| *c == '>').count();
            (depth, l.trim().to_string())
        })
        .collect();
    let blank = |l: &str| l.trim_end_matches(['>', ' ']).is_empty();
    let mut out: Vec<String> = Vec::new();
    let mut prev_depth: Option<usize> = None;
    let mut pending_blank = false;
    for (depth, line) in lines {
        if blank(&line) {
            pending_blank = prev_depth.is_some();
            continue;
        }
        if pending_blank && prev_depth == Some(depth) {
            out.push("> ".repeat(depth).trim_end().to_string());
        }
        pending_blank = false;
        prev_depth = Some(depth);
        out.push(line);
    }
    out.join("\n")
}

fn unescape(s: &str) -> String {
    static ENTITY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"&(#[0-9]+|#[xX][0-9a-fA-F]+|[a-zA-Z]+);").expect("valid regex"));
    ENTITY
        .replace_all(s, |c: &regex::Captures| {
            let e = &c[1];
            let ch = if let Some(n) = e.strip_prefix("#x").or_else(|| e.strip_prefix("#X")) {
                u32::from_str_radix(n, 16).ok().and_then(char::from_u32)
            } else if let Some(n) = e.strip_prefix('#') {
                n.parse().ok().and_then(char::from_u32)
            } else {
                Some(match e {
                    "amp" => '&',
                    "lt" => '<',
                    "gt" => '>',
                    "quot" => '"',
                    "apos" => '\'',
                    "nbsp" => '\u{a0}',
                    "ndash" => '–',
                    "mdash" => '—',
                    "lsquo" => '‘',
                    "rsquo" => '’',
                    "ldquo" => '“',
                    "rdquo" => '”',
                    "hellip" => '…',
                    "bull" => '•',
                    "copy" => '©',
                    "reg" => '®',
                    "trade" => '™',
                    "zwnj" | "zwj" | "shy" => '\u{200b}',
                    _ => return c[0].to_string(),
                })
            };
            ch.filter(|&c| c != '\u{200b}').map(String::from).unwrap_or_default()
        })
        .into_owned()
}

// Reopening drafts

/// A saved draft read back into the composer's fields.
pub fn from_draft(msg: &MimeMessage) -> Outgoing {
    from_draft_with(msg, None)
}

/// [`from_draft`] with the body taken from `content` (a decrypted draft).
pub fn from_draft_with(msg: &MimeMessage, content: Option<&MimePart>) -> Outgoing {
    let field = |kind: &str| {
        let parsed = addresses(msg.recipients(kind));
        if parsed.is_empty() {
            // Kept as typed when it did not parse (see `build`).
            msg.header(kind).map(|h| h.trim().to_string()).unwrap_or_default()
        } else {
            format_addresses(&parsed)
        }
    };
    let format = msg.header(FORMAT_HEADER).and_then(|f| Format::from_key(&f)).unwrap_or(Format::Plain);
    let (body, _) = match content {
        Some(c) => render::reply_text_in(c),
        None => render::reply_text(msg),
    };
    let crypto = msg.header(CRYPTO_HEADER).map(|h| h.to_string()).unwrap_or_default();
    let in_reply_to = msg.header("In-Reply-To").and_then(|h| message_ids(&h).into_iter().next());
    Outgoing {
        from: addresses(msg.from()).into_iter().next().unwrap_or_default(),
        to: field("To"),
        cc: field("Cc"),
        bcc: field("Bcc"),
        subject: msg.subject().map(|s| s.to_string()).unwrap_or_default(),
        body,
        format,
        message_id: msg
            .message_id()
            .map(|m| m.trim_matches(['<', '>', ' ']).to_string())
            .filter(|m| !m.is_empty())
            .unwrap_or_default(),
        in_reply_to,
        references: msg.header("References").map(|r| message_ids(&r)).unwrap_or_default(),
        attachments: render::attachments_in(content.unwrap_or(msg.upcast_ref())),
        forwarded: None,
        autocrypt: None,
        sign: crypto.split(',').any(|f| f.trim() == "sign"),
        encrypt: crypto.split(',').any(|f| f.trim() == "encrypt"),
        important: is_important(msg.header("Importance").as_deref(), msg.header("X-Priority").as_deref()),
        forwarded_id: msg.header(FORWARDED_HEADER).and_then(|h| message_ids(&h).into_iter().next()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn importance() {
        assert!(is_important(Some("High"), None));
        assert!(is_important(None, Some("1 (Highest)")));
        assert!(is_important(None, Some(" 2")));
        assert!(!is_important(Some("normal"), Some("3 (Normal)")));
        assert!(!is_important(None, None));
    }

    #[test]
    fn flowed_round_trip() {
        let text = "A long line that goes on and on well beyond the seventy-two character limit of a flowed line.\n> quoted text\n\nFrom here\n-- \nSig";
        let enc = flowed(text);
        for line in enc.lines() {
            assert!(line.chars().count() <= 73, "{line:?}");
        }
        assert!(enc.contains("\n From here"));
        assert!(enc.contains("\n-- \n"));
        assert_eq!(unflow(&enc), text);
    }

    #[test]
    fn flowed_nested_quotes() {
        let text = "> > deep\n> shallow";
        assert_eq!(flowed(text), ">> deep\n> shallow");
        assert_eq!(unflow(&flowed(text)), text);
    }

    #[test]
    fn markdown_keeps_breaks_and_escapes_html() {
        let html = markdown_html("one\ntwo <img src=x>\n\n> quoted");
        assert!(html.contains("one<br />"));
        assert!(html.contains("&lt;img src=x&gt;"));
        assert!(html.contains("<blockquote>"));
    }

    #[test]
    fn html_text() {
        let t = html_to_text("<html><head><style>p{}</style></head><body><p>Hi&nbsp;there</p><blockquote><p>old</p></blockquote><ul><li>a</li><li>b</li></ul></body></html>");
        assert_eq!(t, "Hi there\n> old\n- a\n- b");
    }

    #[test]
    fn subjects() {
        assert_eq!(reply_subject("Re: x"), "Re: x");
        assert_eq!(reply_subject("x"), "Re: x");
        assert_eq!(forward_subject("FWD: x"), "FWD: x");
        assert_eq!(forward_subject("x"), "Fwd: x");
    }

    fn a(email: &str) -> Address {
        Address { name: None, email: email.into() }
    }

    #[test]
    fn reply_all_drops_own_addresses() {
        let orig = Original {
            from: vec![a("bob@example.org")],
            to: vec![a("me@example.org"), a("carol@example.org")],
            cc: vec![a("dave@example.org"), a("ME@example.org")],
            ..Default::default()
        };
        let own = vec!["me@example.org".to_string()];
        let (to, cc) = reply_recipients(&orig, true, &own);
        assert_eq!(to.iter().map(|a| a.email.as_str()).collect::<Vec<_>>(), ["bob@example.org", "carol@example.org"]);
        assert_eq!(cc.iter().map(|a| a.email.as_str()).collect::<Vec<_>>(), ["dave@example.org"]);
        let (to, cc) = reply_recipients(&orig, false, &own);
        assert_eq!(to.len(), 1);
        assert!(cc.is_empty());
    }

    #[test]
    fn reply_to_own_message_goes_to_recipients() {
        let orig = Original { from: vec![a("me@example.org")], to: vec![a("bob@example.org")], ..Default::default() };
        let (to, _) = reply_recipients(&orig, false, &["me@example.org".into()]);
        assert_eq!(to[0].email, "bob@example.org");
    }

    #[test]
    fn previous_block_lists_cc_only_when_present() {
        let mut orig = Original { from: vec![a("bob@example.org")], subject: "Hi".into(), text: "hello".into(), ..Default::default() };
        let q = reply_quote(&orig, "Thu");
        assert_eq!(q, "--- Previous Message ---\nFrom: bob@example.org\nDate: Thu\nSubject: Hi\n\n> hello\n");
        orig.cc = vec![a("c@example.org")];
        assert!(reply_quote(&orig, "Thu").contains("\nCc: c@example.org\n"));
    }

    #[test]
    fn signature_position() {
        let body = "Hi\n\n--- Previous Message ---\nFrom: x\n\n> q\n";
        assert_eq!(with_signature(body, "Me", true), "Hi\n\nMe\n\n--- Previous Message ---\nFrom: x\n\n> q\n");
        assert_eq!(with_signature(body, "Me", false), "Hi\n\n--- Previous Message ---\nFrom: x\n\n> q\n\nMe\n");
    }
}
