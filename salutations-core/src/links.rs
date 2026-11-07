//! Links in displayed mail: anchors are renumbered so a click names the
//! exact anchor and its text, and destinations are checked and cleaned.

use std::sync::LazyLock;

use regex::Regex;

/// Rewritten anchors point at `x-salutations-link:N`. The web view never
/// loads these; the viewer looks N up in the table from [`rewrite`].
pub const SCHEME: &str = "x-salutations-link";

#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub href: String,
    /// Visible text with tags removed, entities decoded and whitespace
    /// collapsed; empty for an image-only link.
    pub text: String,
}

static ANCHOR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<a\b([^>]*)>(.*?)</a\s*>").expect("valid anchor pattern"));
static HREF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)\bhref\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#).expect("valid href pattern")
});
static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]*>").expect("valid tag pattern"));
static URL_TEXT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:https?://)?(?:[^@/\s]+@)?((?:[a-z0-9-]+\.)+[a-z]{2,63})\.?(?::\d+)?(?:[/?#]\S*)?$")
        .expect("valid URL text pattern")
});

/// Rewrites the href of every closed `<a>` in `html`. Fragment-only hrefs
/// stay, since they move within the message.
pub fn rewrite(html: &str) -> (String, Vec<Link>) {
    let mut links = Vec::new();
    let out = ANCHOR.replace_all(html, |c: &regex::Captures| {
        let (attrs, inner) = (&c[1], &c[2]);
        let Some(h) = HREF.captures(attrs) else {
            return c[0].to_string();
        };
        let raw = h.get(1).or(h.get(2)).or(h.get(3)).map_or("", |m| m.as_str());
        let href = decode_entities(raw).trim().to_string();
        if href.is_empty() || href.starts_with('#') {
            return c[0].to_string();
        }
        let text = decode_entities(&TAG.replace_all(inner, " "));
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let n = links.len();
        links.push(Link { href, text });
        let whole = h.get(0).expect("whole match");
        let attrs = format!("{}href=\"{SCHEME}:{n}\"{}", &attrs[..whole.start()], &attrs[whole.end()..]);
        format!("<a{attrs}>{inner}</a>")
    });
    (out.into_owned(), links)
}

pub(crate) fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let end = rest.char_indices().take(12).find(|(_, c)| *c == ';').map(|(i, _)| i);
        let decoded = end.and_then(|e| {
            let name = &rest[1..e];
            let c = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ if name.starts_with("#x") || name.starts_with("#X") => {
                    u32::from_str_radix(&name[2..], 16).ok().and_then(char::from_u32)
                }
                _ if name.starts_with('#') => name[1..].parse().ok().and_then(char::from_u32),
                _ => None,
            };
            c.map(|c| (c, e))
        });
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &rest[e + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

pub fn host(uri: &str) -> Option<String> {
    glib::Uri::parse(uri, glib::UriFlags::NONE)
        .ok()?
        .host()
        .map(|h| h.trim_end_matches('.').to_ascii_lowercase())
        .filter(|h| !h.is_empty())
}

/// The registrable domain ("chase.com" for "secure.chase.com"), from the
/// public suffix list; the host itself for IP addresses and bare suffixes.
pub fn base_domain(host: &str) -> String {
    let fallback = || host.to_ascii_lowercase();
    let Ok(c) = std::ffi::CString::new(host) else { return fallback() };
    let mut error = std::ptr::null_mut();
    // libsoup returns a pointer into the hostname. soup3 0.9's wrapper frees
    // its temporary copy of the hostname before reading that pointer, so the
    // result is read here while `c` is alive.
    let p = unsafe { soup::ffi::soup_tld_get_base_domain(c.as_ptr(), &mut error) };
    if p.is_null() {
        if !error.is_null() {
            unsafe { glib::ffi::g_error_free(error) };
        }
        return fallback();
    }
    unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().to_ascii_lowercase()
}

/// The host the link's text names when that text is itself an address on
/// another site. Text that is not an address cannot be checked.
pub fn mismatch(link: &Link) -> Option<String> {
    let shown = URL_TEXT.captures(link.text.trim())?.get(1)?.as_str().to_ascii_lowercase();
    let real = host(&link.href)?;
    (base_domain(&shown) != base_domain(&real)).then_some(shown)
}

/// Query parameters that only identify the click, modelled on the list
/// behind Firefox's "Copy Link Without Site Tracking". `utm_` is a prefix.
const TRACKING: &[&str] = &[
    "fbclid", "gclid", "gclsrc", "dclid", "gbraid", "wbraid", "msclkid", "yclid", "twclid", "ttclid",
    "igshid", "li_fat_id", "mc_cid", "mc_eid", "mkt_tok", "_hsenc", "_hsmi", "__hssc", "__hstc",
    "__hsfp", "hsctatracking", "oly_anon_id", "oly_enc_id", "rb_clickid", "s_cid", "vero_conv",
    "vero_id", "wickedid", "_openstat", "ml_subscriber", "ml_subscriber_hash", "_kx", "ss_email_id",
    "bbeml", "ck_subscriber_id", "trk_contact", "trk_msg", "trk_module", "trk_sid", "sc_cid",
];

/// What "Remove site tracking" would change in an address.
#[derive(Debug, Clone, PartialEq)]
pub struct Tracking {
    /// Host of a redirect wrapper that would be skipped.
    pub redirect: Option<String>,
    /// The address after unwrapping, before parameters are removed.
    pub target: String,
    /// Byte ranges of `target` that would be removed, each a parameter with
    /// one of its separators.
    pub removed: Vec<std::ops::Range<usize>>,
}

impl Tracking {
    pub fn is_empty(&self) -> bool {
        self.redirect.is_none() && self.removed.is_empty()
    }

    pub fn cleaned(&self) -> String {
        let mut out = String::with_capacity(self.target.len());
        let mut at = 0;
        for r in &self.removed {
            out.push_str(&self.target[at..r.start]);
            at = r.end;
        }
        out.push_str(&self.target[at..]);
        out
    }
}

pub(crate) fn is_tracking(param: &str) -> bool {
    let key = param.split('=').next().unwrap_or("").to_ascii_lowercase();
    key.starts_with("utm_") || TRACKING.contains(&key.as_str())
}

pub fn tracking(uri: &str) -> Tracking {
    let (redirect, target) = match unwrap_redirect(uri) {
        Some((host, target)) => (Some(host), target),
        None => (None, uri.to_string()),
    };
    let main_end = target.find('#').unwrap_or(target.len());
    let mut removed = Vec::new();
    if let Some(q) = target[..main_end].find('?') {
        // (start, end) of each parameter, after the '?'.
        let mut params = Vec::new();
        let mut start = q + 1;
        for p in target[q + 1..main_end].split('&') {
            params.push((start, start + p.len(), is_tracking(p)));
            start += p.len() + 1;
        }
        let first_kept = params.iter().position(|p| !p.2);
        for (i, &(a, b, drop)) in params.iter().enumerate() {
            if !drop {
                continue;
            }
            let r = match first_kept {
                // Nothing kept: the '?' goes with the first parameter.
                None if i == 0 => a - 1..b,
                // Before the first kept parameter: take the '&' after it.
                Some(k) if i < k => a..b + 1,
                _ => a - 1..b,
            };
            removed.push(r);
        }
    }
    Tracking { redirect, target, removed }
}

/// The destination without known redirect wrappers or tracking parameters;
/// `uri` unchanged when there is nothing to remove.
pub fn strip_tracking(uri: &str) -> String {
    tracking(uri).cleaned()
}

/// Google's `/url?q=` and Outlook Safe Links carry the destination in a
/// parameter. Other click trackers hide it on their server. Returns the
/// wrapper's host and the destination.
fn unwrap_redirect(uri: &str) -> Option<(String, String)> {
    // ENCODED keeps %26 in the wrapped address from splitting the query.
    let parsed = glib::Uri::parse(uri, glib::UriFlags::ENCODED).ok()?;
    let host = parsed.host()?.to_ascii_lowercase();
    let key = if host.ends_with(".safelinks.protection.outlook.com") {
        "url"
    } else if (host.starts_with("google.") || host.starts_with("www.google.")) && parsed.path() == "/url" {
        "q"
    } else {
        return None;
    };
    let query = parsed.query()?;
    let value = query.split('&').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        (k == key || (key == "q" && k == "url")).then_some(v)
    })?;
    let target = glib::uri_unescape_string(value.replace('+', " "), None::<&str>)?.to_string();
    (target.starts_with("http://") || target.starts_with("https://")).then_some((host, target))
}

static BARE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:(?:https?://|www\.)[^\s<>\x22']+|[a-z0-9._%+-]+@[a-z0-9-]+(?:\.[a-z0-9-]+)*\.[a-z]{2,63}\b)")
        .expect("valid bare link pattern")
});

/// Makes web addresses and email addresses written as plain text into
/// links, outside existing links and outside `head`, `script`, `style`,
/// `title` and `textarea`. Text in `html` is HTML-escaped, so the address
/// text is used as the href as it stands.
pub fn linkify(html: &str) -> String {
    let mut out = String::with_capacity(html.len() + 64);
    let mut rest = html;
    // Depth of <a> and of elements whose text is not shown as text.
    let (mut in_a, mut hidden) = (0usize, 0usize);
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            out.push_str(&if in_a == 0 && hidden == 0 { link_text(rest) } else { rest.to_string() });
            break;
        };
        let text = &rest[..lt];
        out.push_str(&if in_a == 0 && hidden == 0 { link_text(text) } else { text.to_string() });
        rest = &rest[lt..];
        if rest.starts_with("<!--") {
            let end = rest.find("-->").map_or(rest.len(), |e| e + 3);
            out.push_str(&rest[..end]);
            rest = &rest[end..];
            continue;
        }
        let end = tag_end(rest);
        let tag = &rest[..end];
        let name: String = tag[1..].trim_start_matches('/').chars().take_while(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
        let closing = tag[1..].starts_with('/');
        let self_closing = tag.ends_with("/>");
        match name.as_str() {
            "a" if !self_closing => {
                if closing {
                    in_a = in_a.saturating_sub(1);
                } else {
                    in_a += 1;
                }
            }
            "head" | "script" | "style" | "title" | "textarea" if !self_closing => {
                if closing {
                    hidden = hidden.saturating_sub(1);
                } else {
                    hidden += 1;
                }
            }
            _ => {}
        }
        out.push_str(tag);
        rest = &rest[end..];
    }
    out
}

/// The length of the tag at the start of `s`, quotes respected.
fn tag_end(s: &str) -> usize {
    let mut quote = None;
    for (i, c) in s.char_indices().skip(1) {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '>') => return i + 1,
            _ => {}
        }
    }
    s.len()
}

fn link_text(text: &str) -> String {
    BARE.replace_all(text, |c: &regex::Captures| {
        let m = &c[0];
        // Sentence punctuation after an address is not part of it; a
        // closing bracket is, when the address opened one.
        let mut end = m.len();
        while end > 0 {
            let ch = m[..end].chars().last().expect("non-empty");
            let unbalanced = match ch {
                ')' => m[..end].matches('(').count() < m[..end].matches(')').count(),
                ']' => m[..end].matches('[').count() < m[..end].matches(']').count(),
                '.' | ',' | ';' | ':' | '!' | '?' | '*' => true,
                _ => false,
            };
            if !unbalanced {
                break;
            }
            end -= ch.len_utf8();
        }
        // An entity cut by the punctuation rule ("&amp;" ends in ";").
        if let Some(amp) = m[..end].rfind('&')
            && !m[amp..end].contains(';')
            && m[amp..].starts_with(&m[amp..end])
            && m.len() > end
            && m[end..].starts_with(';')
        {
            end += 1;
        }
        let (addr, tail) = m.split_at(end);
        if addr.is_empty() {
            return m.to_string();
        }
        let href = if addr.contains("://") {
            addr.to_string()
        } else if addr.to_ascii_lowercase().starts_with("www.") {
            format!("https://{addr}")
        } else {
            format!("mailto:{addr}")
        };
        format!("<a href=\"{href}\">{addr}</a>{tail}")
    })
    .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewriting() {
        let (html, links) = rewrite(
            r##"<p><A class="b" HREF="https://x.example/a?b=1&amp;c=2">Read <b>more</b></a> <a href='#top'>top</a> <a href=https://y.example>y</a></p>"##,
        );
        assert_eq!(
            links,
            vec![
                Link { href: "https://x.example/a?b=1&c=2".into(), text: "Read more".into() },
                Link { href: "https://y.example".into(), text: "y".into() },
            ]
        );
        assert!(html.contains(r#"<a class="b" href="x-salutations-link:0">Read <b>more</b></a>"#));
        assert!(html.contains(r##"<a href='#top'>top</a>"##));
        assert!(html.contains(r#"<a href="x-salutations-link:1">y</a>"#));
    }

    #[test]
    fn mismatches() {
        let link = |href: &str, text: &str| Link { href: href.into(), text: text.into() };
        assert_eq!(
            mismatch(&link("https://evil.example/login", "https://www.chase.com")),
            Some("www.chase.com".into())
        );
        assert_eq!(mismatch(&link("https://secure.chase.com/x", "www.chase.com")), None);
        assert_eq!(mismatch(&link("https://chase.com.evil.example/", "chase.com")), Some("chase.com".into()));
        assert_eq!(mismatch(&link("https://evil.example/", "Log in to Chase")), None);
        assert_eq!(mismatch(&link("https://a.co.uk/", "b.co.uk")), Some("b.co.uk".into()));
    }

    #[test]
    fn stripping() {
        assert_eq!(
            strip_tracking("https://x.example/p?id=4&utm_source=news&UTM_Medium=mail&fbclid=abc#s"),
            "https://x.example/p?id=4#s"
        );
        assert_eq!(strip_tracking("https://x.example/p?utm_source=a"), "https://x.example/p");
        assert_eq!(strip_tracking("https://x.example/p?id=4"), "https://x.example/p?id=4");
        assert_eq!(
            strip_tracking("https://www.google.com/url?q=https%3A%2F%2Fy.example%2Fa%3Fgclid%3D1%26k%3D2&sa=D"),
            "https://y.example/a?k=2"
        );
        assert_eq!(
            strip_tracking("https://eur01.safelinks.protection.outlook.com/?url=https%3A%2F%2Fz.example%2F&data=x"),
            "https://z.example/"
        );
    }

    #[test]
    fn removed_ranges() {
        let t = tracking("https://x.example/p?utm_a=1&id=4&fbclid=2#f");
        assert_eq!(t.removed.iter().map(|r| &t.target[r.clone()]).collect::<Vec<_>>(), ["utm_a=1&", "&fbclid=2"]);
        assert_eq!(t.cleaned(), "https://x.example/p?id=4#f");
        let t = tracking("https://x.example/p?utm_a=1&fbclid=2");
        assert_eq!(t.removed.iter().map(|r| &t.target[r.clone()]).collect::<Vec<_>>(), ["?utm_a=1", "&fbclid=2"]);
        assert!(tracking("https://x.example/p?id=4").is_empty());
        let t = tracking("https://www.google.com/url?q=https%3A%2F%2Fy.example%2F&sa=D");
        assert_eq!(t.redirect.as_deref(), Some("www.google.com"));
        assert!(!t.is_empty());
    }

    #[test]
    fn entities() {
        assert_eq!(decode_entities("a&amp;b &#65;&#x42; &bogus; &"), "a&b AB &bogus; &");
    }

    #[test]
    fn bare_links() {
        assert_eq!(linkify("See https://a.example/x?y=1&amp;z=2."), "See <a href=\"https://a.example/x?y=1&amp;z=2\">https://a.example/x?y=1&amp;z=2</a>.");
        assert_eq!(linkify("(www.a.example/p)"), "(<a href=\"https://www.a.example/p\">www.a.example/p</a>)");
        assert_eq!(linkify("wiki https://en.example/A_(b)"), "wiki <a href=\"https://en.example/A_(b)\">https://en.example/A_(b)</a>");
        assert_eq!(linkify("mail bob@a.example!"), "mail <a href=\"mailto:bob@a.example\">bob@a.example</a>!");
        let html = "<a href=\"https://x.example\">https://x.example</a> <style>a{background:url(https://s.example/i)}</style><p title=\"https://t.example\">t</p>";
        assert_eq!(linkify(html), html);
    }
}
