//! Turns a `CamelMimeMessage` into what the viewer shows: header fields and
//! one HTML document for the body.

use std::sync::LazyLock;

use camel::prelude::*;
use camel::{DataWrapper, MimeMessage, MimePart, Multipart};
use regex::Regex;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Address {
    pub name: Option<String>,
    pub email: String,
}

impl Address {
    /// The name when there is one, otherwise the address.
    pub fn display(&self) -> &str {
        self.name.as_deref().filter(|n| !n.is_empty()).unwrap_or(&self.email)
    }

    /// The part after "@", lowercased.
    pub fn domain(&self) -> Option<String> {
        self.email.rsplit_once('@').map(|(_, d)| d.to_ascii_lowercase())
    }
}

#[derive(Debug, Clone)]
pub struct Rendered {
    pub subject: String,
    pub from: Vec<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    /// Seconds since the epoch, from the Date header.
    pub date: Option<i64>,
    /// A complete document; plain-text mail is escaped into a `<pre>`.
    pub body_html: String,
    pub is_plain: bool,
    /// Shown in the theme's colours: plain text, HTML that declares dark
    /// support, and HTML that sets no colours. Other HTML gets a white
    /// canvas because its colours were chosen against one.
    pub follows_theme: bool,
    /// The HTML refers to images, styles or fonts outside the message.
    pub has_remote: bool,
    /// Message-ID without angle brackets, for per-message choices that must
    /// survive a move to another folder.
    pub message_id: Option<String>,
    /// From the topmost `Authentication-Results` header, if any.
    pub verification: Option<crate::verify::Verification>,
    /// Marked high importance by its sender.
    pub important: bool,
    /// The sender's picture from a `Face:` or else an `X-Face:` header, as
    /// a PNG.
    pub face: Option<Vec<u8>>,
}

fn addresses(list: Option<camel::InternetAddress>) -> Vec<Address> {
    let Some(list) = list else {
        return Vec::new();
    };
    (0..list.length())
        .filter_map(|i| list.get(i))
        .filter_map(|(name, email)| {
            Some(Address {
                name: name.map(|n| n.to_string()).filter(|n| !n.is_empty()),
                email: email?.to_string(),
            })
        })
        .collect()
}

pub fn render(msg: &MimeMessage) -> Rendered {
    render_with(msg, None)
}

/// [`render`] with the body taken from `body` (a decrypted or verified
/// part) instead of the message's content.
pub fn render_with(msg: &MimeMessage, body: Option<&MimePart>) -> Rendered {
    let (date, _) = msg.date();
    let mut out = Rendered {
        subject: msg.subject().map(|s| s.to_string()).unwrap_or_default(),
        from: addresses(msg.from()),
        to: addresses(msg.recipients("To")),
        cc: addresses(msg.recipients("Cc")),
        // Camel returns CAMEL_MESSAGE_DATE_CURRENT (-1) without a Date header.
        date: (date > 0).then_some(date),
        body_html: String::new(),
        is_plain: true,
        follows_theme: true,
        has_remote: false,
        verification: msg.header("Authentication-Results").map(|h| crate::verify::parse(&h)),
        message_id: msg.message_id().map(|m| m.trim_matches(['<', '>', ' ']).to_string()).filter(|m| !m.is_empty()),
        important: crate::compose::is_important(msg.header("Importance").as_deref(), msg.header("X-Priority").as_deref()),
        face: msg
            .header("Face")
            .and_then(|h| crate::avatars::face_png(&h))
            .or_else(|| msg.header("X-Face").and_then(|h| crate::xface::png(&h))),
    };
    match find_body(body.unwrap_or(msg.upcast_ref())) {
        Some((part, true)) => {
            out.body_html = collapse_blockquotes(&strip_resource_hints(&part_text(&part)));
            out.is_plain = false;
            out.has_remote = has_remote_content(&out.body_html);
            out.follows_theme = declares_dark(&out.body_html) || !sets_colors(&out.body_html);
        }
        Some((part, false)) => out.body_html = plain_to_html(&part_text(&part)),
        None => out.body_html = plain_to_html(""),
    }
    out
}

/// The part to display and whether it is HTML.
fn find_body(part: &MimePart) -> Option<(MimePart, bool)> {
    let ct = part.content_type();
    let is = |t: &str, s: &str| ct.as_ref().is_some_and(|c| c.is(t, s));
    if is("text", "html") {
        return Some((part.clone(), true));
    }
    if is("text", "plain") || ct.is_none() {
        return Some((part.clone(), false));
    }
    let content = part.content()?;
    if let Some(inner) = content.downcast_ref::<MimeMessage>() {
        return find_body(inner.upcast_ref());
    }
    let mp = content.downcast_ref::<Multipart>()?;
    let parts: Vec<MimePart> = (0..mp.number()).filter_map(|i| mp.part(i)).collect();
    if is("multipart", "alternative") {
        let found: Vec<_> = parts.iter().filter_map(find_body).collect();
        return found
            .iter()
            .rev()
            .find(|(_, html)| *html)
            .or(found.first())
            .cloned();
    }
    // mixed, related, signed and the rest: the first displayable part.
    parts.iter().find_map(find_body)
}

fn part_text(part: &MimePart) -> String {
    let Some(content) = part.content() else {
        return String::new();
    };
    let bytes = decode(&content);
    let charset = part
        .content_type()
        .and_then(|ct| ct.param("charset"))
        .map(|c| c.to_string())
        .unwrap_or_else(|| "UTF-8".into());
    if charset.eq_ignore_ascii_case("utf-8") || charset.eq_ignore_ascii_case("us-ascii") {
        return String::from_utf8_lossy(&bytes).into_owned();
    }
    match glib::convert_with_fallback(&bytes, "UTF-8", &charset, Some("?")) {
        Ok((converted, _)) => String::from_utf8_lossy(&converted).into_owned(),
        Err(_) => String::from_utf8_lossy(&bytes).into_owned(),
    }
}

/// A part's decoded bytes.
pub fn decode_public(content: &DataWrapper) -> Vec<u8> {
    decode(content)
}

/// A text part's content, converted to UTF-8.
pub fn plain_text(part: &MimePart) -> String {
    part_text(part)
}

fn decode(content: &DataWrapper) -> Vec<u8> {
    let out = gio::MemoryOutputStream::new_resizable();
    if content
        .decode_to_output_stream_sync(&out, None::<&gio::Cancellable>)
        .is_err()
    {
        return Vec::new();
    }
    let _ = out.close(None::<&gio::Cancellable>);
    out.steal_as_bytes().to_vec()
}

/// The body for quoting or editing as plain text, and whether the message
/// has an HTML body. A text/plain part is preferred (format=flowed is
/// decoded); HTML-only mail is converted.
pub fn reply_text(msg: &MimeMessage) -> (String, bool) {
    reply_text_in(msg.upcast_ref())
}

/// [`reply_text`] of a part, such as a decrypted body.
pub fn reply_text_of(part: &MimePart) -> String {
    reply_text_in(part).0
}

/// [`reply_text`] of a part, with whether it has an HTML body.
pub fn reply_text_in(part: &MimePart) -> (String, bool) {
    let html = find_body(part).filter(|(_, html)| *html).map(|(p, _)| p);
    match find_plain(part) {
        Some(part) => {
            let text = part_text(&part);
            let flowed = part
                .content_type()
                .and_then(|ct| ct.param("format"))
                .is_some_and(|f| f.eq_ignore_ascii_case("flowed"));
            let text = if flowed { crate::compose::unflow(&text) } else { text };
            (text.replace("\r\n", "\n"), html.is_some())
        }
        None => match html {
            Some(part) => (crate::compose::html_to_text(&part_text(&part)), true),
            None => (String::new(), false),
        },
    }
}

/// The first text/plain part that is not an attachment.
fn find_plain(part: &MimePart) -> Option<MimePart> {
    let ct = part.content_type();
    let attachment = part.disposition().is_some_and(|d| d.eq_ignore_ascii_case("attachment"));
    if ct.as_ref().is_none_or(|c| c.is("text", "plain")) {
        return (!attachment).then(|| part.clone());
    }
    let content = part.content()?;
    if ct.as_ref().is_some_and(|c| c.is("message", "rfc822")) {
        return None;
    }
    let mp = content.downcast_ref::<Multipart>()?;
    (0..mp.number()).filter_map(|i| mp.part(i)).find_map(|p| find_plain(&p))
}

/// Parts of a saved draft that are attachments, for reopening it.
pub fn attachments(msg: &MimeMessage) -> Vec<crate::compose::Attachment> {
    attachments_in(msg.upcast_ref())
}

/// [`attachments`] of a part, such as a decrypted body.
pub fn attachments_in(part: &MimePart) -> Vec<crate::compose::Attachment> {
    let mut out = Vec::new();
    let Some(content) = part.content() else { return out };
    let Some(mp) = content.downcast_ref::<Multipart>() else { return out };
    for part in (0..mp.number()).filter_map(|i| mp.part(i)) {
        let attachment = part.disposition().is_some_and(|d| d.eq_ignore_ascii_case("attachment"));
        let is_rfc822 = part.content_type().is_some_and(|c| c.is("message", "rfc822"));
        if !attachment || is_rfc822 {
            continue;
        }
        let Some(data) = part.content() else { continue };
        out.push(crate::compose::Attachment {
            name: part.filename().map(|f| f.to_string()).unwrap_or_else(|| "attachment".into()),
            mime_type: part
                .content_type()
                .and_then(|ct| ct.simple())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "application/octet-stream".into()),
            data: glib::Bytes::from_owned(decode(&data)),
        });
    }
    out
}

/// Every attachment of a received message, at any depth, for listing
/// under its headers. Body text, signature and encryption control parts,
/// and images the HTML shows through `cid:` are left out. A forwarded
/// message is listed as a `.eml` file.
pub fn received_attachments(part: &MimePart, html: &str) -> Vec<crate::compose::Attachment> {
    let mut out = Vec::new();
    collect_attachments(part, html, &mut out);
    out
}

fn collect_attachments(part: &MimePart, html: &str, out: &mut Vec<crate::compose::Attachment>) {
    let Some(content) = part.content() else { return };
    let ct = part.content_type();
    let simple = ct.as_ref().and_then(|c| c.simple()).map(|s| s.to_ascii_lowercase()).unwrap_or_default();
    if let Some(mp) = content.downcast_ref::<Multipart>() {
        // A signed part's second child is the signature; an encrypted one's
        // content was shown already, decrypted.
        let n = match simple.as_str() {
            "multipart/signed" => mp.number().min(1),
            "multipart/encrypted" => 0,
            _ => mp.number(),
        };
        for p in (0..n).filter_map(|i| mp.part(i)) {
            collect_attachments(&p, html, out);
        }
        return;
    }
    if matches!(simple.as_str(), "application/pgp-signature" | "application/pgp-encrypted") {
        return;
    }
    let disposition = part.disposition().map(|d| d.to_ascii_lowercase());
    let filename = part.filename().map(|f| f.to_string()).filter(|f| !f.trim().is_empty());
    let attachment = disposition.as_deref() == Some("attachment");
    if let Some(msg) = content.downcast_ref::<MimeMessage>() {
        let stream = gio::MemoryOutputStream::new_resizable();
        if msg.write_to_output_stream_sync(&stream, None::<&gio::Cancellable>).is_err() {
            return;
        }
        let _ = stream.close(None::<&gio::Cancellable>);
        let subject = msg.subject().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        out.push(crate::compose::Attachment {
            name: filename.unwrap_or_else(|| format!("{}.eml", subject.as_deref().unwrap_or("Forwarded message"))),
            mime_type: "message/rfc822".into(),
            data: stream.steal_as_bytes(),
        });
        return;
    }
    // Text without a name or an attachment disposition is the body.
    if simple.starts_with("text/") && filename.is_none() && !attachment {
        return;
    }
    if let Some(id) = part.content_id() {
        let id = id.trim_matches(['<', '>', ' ']);
        if !id.is_empty() && html.contains(&format!("cid:{id}")) {
            return;
        }
    }
    out.push(crate::compose::Attachment {
        name: filename.unwrap_or_else(|| "attachment".into()),
        mime_type: if simple.is_empty() { "application/octet-stream".into() } else { simple },
        data: glib::Bytes::from_owned(decode(&content)),
    });
}

/// Bytes of the part whose Content-ID is `cid`, with its MIME type.
pub fn inline_part(msg: &MimeMessage, cid: &str) -> Option<(Vec<u8>, String)> {
    part_data(&msg.part_by_content_id(cid)?)
}

/// [`inline_part`] searched for in a part, such as a decrypted body.
pub fn inline_part_in(part: &MimePart, cid: &str) -> Option<(Vec<u8>, String)> {
    let id = part.content_id().map(|c| c.trim_matches(['<', '>']).to_string());
    if id.as_deref() == Some(cid.trim_matches(['<', '>'])) {
        return part_data(part);
    }
    let mp = part.content()?.downcast::<Multipart>().ok()?;
    (0..mp.number()).filter_map(|i| mp.part(i)).find_map(|p| inline_part_in(&p, cid))
}

fn part_data(part: &MimePart) -> Option<(Vec<u8>, String)> {
    let mime = part
        .content_type()
        .and_then(|ct| ct.simple())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "application/octet-stream".into());
    Some((decode(&part.content()?), mime))
}

pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A quote of up to this many lines stays open.
const SHORT_QUOTE_LINES: usize = 3;
/// An HTML quote with up to this much text stays open.
const SHORT_QUOTE_CHARS: usize = 240;

/// The bar a collapsed quote is folded under. `<details>` opens and closes
/// without JavaScript, which the viewer keeps off.
fn folded(inner: &str) -> String {
    format!(
        "<details class=\"salutations-quote\"><summary><span class=\"show\">Show quoted text</span>\
         <span class=\"hide\">Hide quoted text</span></summary>{inner}</details>"
    )
}

fn is_quote_line(line: &str) -> bool {
    line.trim_start().starts_with('>')
}

/// Plain text in `<pre>` blocks, with runs of `>` lines longer than
/// `SHORT_QUOTE_LINES` folded. Blank lines between quoted lines belong to
/// the quote.
fn plain_to_html(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let quoted: Vec<bool> = (0..lines.len())
        .map(|i| {
            if is_quote_line(lines[i]) {
                return true;
            }
            if !lines[i].trim().is_empty() {
                return false;
            }
            let before = lines[..i].iter().rev().find(|l| !l.trim().is_empty());
            let after = lines[i + 1..].iter().find(|l| !l.trim().is_empty());
            before.is_some_and(|l| is_quote_line(l)) && after.is_some_and(|l| is_quote_line(l))
        })
        .collect();
    let mut out = String::new();
    let mut i = 0;
    while i < lines.len() {
        let q = quoted[i];
        let start = i;
        while i < lines.len() && quoted[i] == q {
            i += 1;
        }
        let block = escape(&lines[start..i].join("\n"));
        if !q {
            out.push_str(&format!("<pre class=\"salutations-plain\">{block}</pre>"));
        } else if i - start <= SHORT_QUOTE_LINES {
            out.push_str(&format!("<pre class=\"salutations-plain quoted\">{block}</pre>"));
        } else {
            out.push_str(&folded(&format!("<pre class=\"salutations-plain quoted\">{block}</pre>")));
        }
    }
    if out.is_empty() {
        out.push_str("<pre class=\"salutations-plain\"></pre>");
    }
    out
}

static BLOCKQUOTE_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<(/?)blockquote\b[^>]*>").expect("valid blockquote pattern"));

/// Folds each outermost `<blockquote>` holding more than
/// `SHORT_QUOTE_CHARS` of text. Unbalanced tags leave the rest unchanged.
fn collapse_blockquotes(html: &str) -> String {
    let mut out = String::with_capacity(html.len() + 256);
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut copied = 0usize;
    for m in BLOCKQUOTE_TAG.captures_iter(html) {
        let whole = m.get(0).expect("whole match");
        let closing = !m[1].is_empty();
        if !closing {
            if depth == 0 {
                start = whole.start();
            }
            depth += 1;
        } else if depth > 0 {
            depth -= 1;
            if depth == 0 {
                let quote = &html[start..whole.end()];
                out.push_str(&html[copied..start]);
                let text = clean_preview(quote);
                if text.chars().count() > SHORT_QUOTE_CHARS {
                    out.push_str(&folded(quote));
                } else {
                    out.push_str(quote);
                }
                copied = whole.end();
            }
        }
    }
    out.push_str(&html[copied..]);
    out
}

static REMOTE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(?:\b(?:src|srcset|background|poster)\s*=\s*["']?\s*(?:https?:)?//)|(?:url\(\s*["']?\s*(?:https?:)?//)|(?:@import\s+["']?(?:https?:)?//)|(?:<link\b[^>]*\bhref\s*=\s*["']?\s*(?:https?:)?//)"#,
    )
    .expect("valid remote-content pattern")
});

static PREVIEW_DROP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)<style\b.*?(?:</style\s*>|$)|<script\b.*?(?:</script\s*>|$)|<head\b.*?(?:</head\s*>|$)|<!--.*?(?:-->|$)",
    )
    .expect("valid preview block pattern")
});
static PREVIEW_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)</?[A-Za-z!][^>]*(?:>|$)").expect("valid preview tag pattern"));

/// A message preview as plain text. Previews from the server or from an
/// HTML-only message can hold markup, style sheets and entities, possibly
/// cut off mid-tag. A `<` not followed by a letter, `/` or `!` is kept, so
/// "a < b" in plain text survives.
pub fn clean_preview(text: &str) -> String {
    let text = if text.contains('<') {
        let t = PREVIEW_DROP.replace_all(text, " ");
        PREVIEW_TAG.replace_all(&t, " ").into_owned()
    } else {
        text.to_string()
    };
    let text = if text.contains('&') { crate::links::decode_entities(&text) } else { text };
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    strip_leading_css(&text).to_string()
}

static CSS_SELECTOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*[@A-Za-z0-9_.#:,\-\[\]="'>*+~()% ]+\{"#).expect("valid CSS selector pattern")
});
static CSS_DECLARATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*-*[A-Za-z][A-Za-z-]*\s*:").expect("valid CSS declaration pattern"));

/// Style sheet rules at the start of a preview, left when a server builds
/// the preview from an HTML part with its tags removed but its `<style>`
/// text kept ("div.preheader { display: none }…"). Rules are removed whole,
/// nested blocks such as `@media` included; a rule cut off by the end of
/// the preview takes the rest with it.
fn strip_leading_css(text: &str) -> &str {
    let mut rest = text;
    while CSS_SELECTOR.is_match(rest) {
        let open = rest.find('{').expect("the pattern ends at a brace");
        let mut depth = 0usize;
        let mut end = None;
        for (i, c) in rest.char_indices().skip_while(|(i, _)| *i < open) {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        end = Some(i + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        // A rule's body starts with a declaration ("display: none") or a
        // nested rule; "Meet at 5 {room 2}" is text.
        let body = &rest[open + 1..end.map_or(rest.len(), |e| e - 1)];
        if !(CSS_DECLARATION.is_match(body) || CSS_SELECTOR.is_match(body)) {
            break;
        }
        match end {
            Some(e) => rest = rest[e..].trim_start(),
            None => return "",
        }
    }
    rest
}

static HINTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<link\b[^>]*\brel\s*=\s*["']?[^"'>]*\b(?:dns-prefetch|preconnect|prefetch|preload|prerender|modulepreload)\b[^>]*>"#)
        .expect("valid resource hint pattern")
});

/// Removes `<link>` hints that ask the engine to resolve or contact a host
/// ahead of use. The Content-Security-Policy has no directive covering DNS
/// prefetch, and the hints serve no purpose in mail.
fn strip_resource_hints(html: &str) -> String {
    HINTS.replace_all(html, "").into_owned()
}

/// Anchors are not counted: following a link is the user's action, not a
/// load.
fn has_remote_content(html: &str) -> bool {
    REMOTE.is_match(html)
}

static DARK_META: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<meta\b[^>]*\bcolor-scheme\b[^>]*>"#).expect("valid color-scheme meta pattern")
});
static DARK_CSS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\bcolor-scheme\s*:[^;}"']*\bdark\b"#).expect("valid color-scheme CSS pattern")
});
static COLORS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(?:\b(?:bgcolor|background|text|link|vlink|alink|color)\s*=)|(?:(?:^|[\s;{"'])(?:color|background(?:-color|-image)?)\s*:)"#,
    )
    .expect("valid colour pattern")
});

/// A `color-scheme` meta tag or CSS property that includes "dark".
fn declares_dark(html: &str) -> bool {
    let meta = DARK_META.find_iter(html).any(|m| {
        let tag = m.as_str().to_ascii_lowercase().replace(['"', '\''], "");
        tag.contains("name=color-scheme") && tag.contains("dark")
    });
    meta || DARK_CSS.is_match(html)
}

/// Any colour or background set by attribute or CSS. Text that merely
/// looks like one (a URL with "?color=") also counts; a false match only
/// costs the theme's colours, a missed one could leave dark text on dark.
fn sets_colors(html: &str) -> bool {
    COLORS.is_match(html)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_content() {
        assert!(has_remote_content(r#"<img src="https://example.com/a.png">"#));
        assert!(has_remote_content(r#"<IMG SRC='//cdn.example.com/a.png'>"#));
        assert!(has_remote_content(r#"<div style="background: url( 'http://x/y.png')">"#));
        assert!(has_remote_content(r#"<link rel="stylesheet" href="https://x/s.css">"#));
        assert!(!has_remote_content(r#"<a href="https://example.com">x</a><img src="cid:a@b">"#));
        assert!(!has_remote_content(r#"<img src="data:image/png;base64,AAAA">"#));
    }

    #[test]
    fn colour_schemes() {
        assert!(declares_dark(r#"<meta name="color-scheme" content="light dark">"#));
        assert!(declares_dark(r#"<META content='dark light' NAME='color-scheme'>"#));
        assert!(declares_dark(r#"<style>:root { color-scheme: light dark; }</style>"#));
        assert!(!declares_dark(r#"<meta name="color-scheme" content="light only">"#));
        assert!(!declares_dark(r#"<meta name="description" content="dark matter">"#));
        assert!(sets_colors(r#"<p style="color:#333">x</p>"#));
        assert!(sets_colors(r#"<style>p{color:#333}</style>"#));
        assert!(sets_colors(r##"<table bgcolor="#eee">"##));
        assert!(sets_colors(r#"<font color=red>x</font>"#));
        assert!(sets_colors(r#"<td style="background: #fff">"#));
        assert!(!sets_colors(r#"<p>Hi,</p><blockquote>quoted</blockquote><a href="https://x">x</a>"#));
        assert!(!sets_colors(r#"<style>:root{color-scheme:light}</style><p style="border-color:red">x</p>"#));
    }

    #[test]
    fn plain_quotes() {
        let long = "Sure.\n\nOn Monday, Sam wrote:\n> one\n> two\n>\n\n> three\n> four\nBye";
        let html = plain_to_html(long);
        assert!(html.starts_with("<pre class=\"salutations-plain\">Sure.\n\nOn Monday, Sam wrote:</pre><details"));
        assert!(html.contains("&gt; one\n&gt; two\n&gt;\n\n&gt; three\n&gt; four</pre></details>"));
        assert!(html.ends_with("<pre class=\"salutations-plain\">Bye</pre>"));
        let short = plain_to_html("Yes\n> a\n> b");
        assert!(!short.contains("<details"));
        assert!(short.contains("<pre class=\"salutations-plain quoted\">&gt; a\n&gt; b</pre>"));
    }

    #[test]
    fn html_quotes() {
        let filler = "x ".repeat(200);
        let html = format!("<p>Hi</p><blockquote>outer {filler}<blockquote>inner</blockquote></blockquote><blockquote>short</blockquote>");
        let out = collapse_blockquotes(&html);
        assert_eq!(out.matches("<details").count(), 1);
        assert!(out.contains("<blockquote>inner</blockquote></blockquote></details><blockquote>short</blockquote>"));
        assert_eq!(collapse_blockquotes("<blockquote>open"), "<blockquote>open");
    }

    #[test]
    fn previews() {
        assert_eq!(
            clean_preview("<html><head><style>p{color:red}</style></head><body><p>Hi&nbsp;there, <b>Alice</b></p>"),
            "Hi there, Alice"
        );
        assert_eq!(clean_preview("<!-- x --><div class=\"a\">Sale ends</div><table width=\"100"), "Sale ends");
        assert_eq!(clean_preview("a < b and  c > d"), "a < b and c > d");
        assert_eq!(clean_preview("<style>body{margin:0"), "");
        assert_eq!(
            clean_preview("div.preheader { display: none !important; } @media screen { .x { color: red } } Sign up today"),
            "Sign up today"
        );
        assert_eq!(clean_preview("table td { padding: 0"), "");
        assert_eq!(clean_preview("Meet at 5 {room 2}"), "Meet at 5 {room 2}");
    }

    #[test]
    fn resource_hints() {
        let html = r#"<head><link rel="dns-prefetch" href="//t.example"><LINK REL='preconnect stylesheet' href="https://c.example"><link rel="stylesheet" href="https://s.example/a.css"></head>"#;
        assert_eq!(
            strip_resource_hints(html),
            r#"<head><link rel="stylesheet" href="https://s.example/a.css"></head>"#
        );
    }

    #[test]
    fn escaping() {
        assert_eq!(escape(r#"<a href="x">&'"#), "&lt;a href=&quot;x&quot;&gt;&amp;&#39;");
    }
}
