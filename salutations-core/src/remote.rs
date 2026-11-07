//! Remote content of messages whose remote content is allowed: images,
//! stylesheets and fonts. Salutations fetches them itself, without cookies,
//! into a size-limited cache, and the web view loads them from
//! `x-salutations-remote:` URIs; it never reaches the network itself.
//! Reopening a message therefore sends nothing, and content seen before
//! shows offline. Addresses inside fetched stylesheets are rewritten the
//! same way when served.

use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::SystemTime;

use gio::prelude::*;
use regex::{Captures, Regex};
use soup::prelude::*;

pub const SCHEME: &str = "x-salutations-remote";

/// Total size the cache is trimmed to, least recently used first.
const CACHE_LIMIT: u64 = 50 * 1024 * 1024;
/// A larger image or font is not loaded.
const IMAGE_LIMIT: usize = 10 * 1024 * 1024;
/// A larger stylesheet is not loaded.
const CSS_LIMIT: usize = 2 * 1024 * 1024;

fn cache_dir() -> PathBuf {
    crate::cache_dir().join("remote")
}

static ATTR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b(src|background)(\s*=\s*)(?:"((?:https?:)?//[^"]*)"|'((?:https?:)?//[^']*)'|((?:https?:)?//[^\s>"']+))"#)
        .expect("valid image attribute pattern")
});
static SRCSET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\bsrcset(\s*=\s*)(?:"([^"]*)"|'([^']*)')"#).expect("valid srcset pattern")
});
static CSS_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)url\(\s*(?:"([^"]*)"|'([^']*)'|([^)\s'"]+))\s*\)"#).expect("valid CSS url pattern")
});
/// `@import "…"` without `url()`.
static CSS_IMPORT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)@import\s+(?:"([^"]*)"|'([^']*)')"#).expect("valid CSS import pattern"));
static LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<link\b[^>]*>").expect("valid link pattern"));
static HREF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\bhref(\s*=\s*)(?:"((?:https?:)?//[^"]*)"|'((?:https?:)?//[^']*)'|((?:https?:)?//[^\s>"']+))"#)
        .expect("valid href pattern")
});
static STYLESHEET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\brel\s*=\s*["']?[^"'>]*\bstylesheet\b"#).expect("valid rel pattern"));

fn local_uri(raw: &str) -> String {
    let url = crate::links::decode_entities(raw.trim());
    let url = if url.starts_with("//") { format!("https:{url}") } else { url };
    format!("{SCHEME}:{}", glib::Uri::escape_string(&url, None, false))
}

/// The remote address behind a rewritten URI.
pub fn remote_url(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix(SCHEME)?.strip_prefix(':')?;
    glib::uri_unescape_string(rest, None::<&str>).map(|s| s.to_string())
}

/// Points image sources (`src`, `background`, `srcset`), stylesheet links
/// and CSS `url()` and `@import` at [`SCHEME`].
pub fn rewrite(html: &str) -> String {
    let html = ATTR.replace_all(html, |c: &Captures| {
        let url = c.get(3).or(c.get(4)).or(c.get(5)).map_or("", |m| m.as_str());
        format!("{}{}\"{}\"", &c[1], &c[2], local_uri(url))
    });
    let html = SRCSET.replace_all(&html, |c: &Captures| {
        let list = c.get(2).or(c.get(3)).map_or("", |m| m.as_str());
        let items: Vec<String> = list
            .split(',')
            .map(|item| {
                let item = item.trim();
                let (url, rest) = item.split_once(char::is_whitespace).unwrap_or((item, ""));
                if url.starts_with("http://") || url.starts_with("https://") || url.starts_with("//") {
                    format!("{} {}", local_uri(url), rest.trim()).trim_end().to_string()
                } else {
                    item.to_string()
                }
            })
            .collect();
        format!("srcset{}\"{}\"", &c[1], items.join(", "))
    });
    let html = LINK.replace_all(&html, |c: &Captures| {
        let tag = &c[0];
        if !STYLESHEET.is_match(tag) {
            return tag.to_string();
        }
        HREF.replace(tag, |h: &Captures| {
            let url = h.get(2).or(h.get(3)).or(h.get(4)).map_or("", |m| m.as_str());
            format!("href{}\"{}\"", &h[1], local_uri(url))
        })
        .into_owned()
    });
    // Relative addresses in a message have nothing to resolve against.
    rewrite_css(&html, None)
}

/// `url()` and `@import` addresses in `css` pointed at [`SCHEME`], relative
/// ones resolved against `base`, the stylesheet's own address.
pub fn rewrite_css(css: &str, base: Option<&str>) -> String {
    let local = |raw: &str| -> Option<String> {
        let raw = crate::links::decode_entities(raw.trim());
        let absolute = if raw.starts_with("http://") || raw.starts_with("https://") || raw.starts_with("//") {
            raw
        } else {
            // data:, cid:, fragments (SVG references) and the scheme itself
            // stay as they are.
            let lower = raw.to_ascii_lowercase();
            if raw.is_empty() || raw.starts_with('#') || lower.starts_with("data:") || lower.starts_with("cid:") || lower.starts_with(SCHEME) {
                return None;
            }
            glib::Uri::resolve_relative(base, &raw, glib::UriFlags::NONE).ok()?.to_string()
        };
        Some(local_uri(&absolute))
    };
    let css = CSS_URL.replace_all(css, |c: &Captures| {
        let url = c.get(1).or(c.get(2)).or(c.get(3)).map_or("", |m| m.as_str());
        match local(url) {
            Some(l) => format!("url(\"{l}\")"),
            None => c[0].to_string(),
        }
    });
    CSS_IMPORT
        .replace_all(&css, |c: &Captures| {
            let url = c.get(1).or(c.get(2)).map_or("", |m| m.as_str());
            match local(url) {
                Some(l) => format!("@import url(\"{l}\")"),
                None => c[0].to_string(),
            }
        })
        .into_owned()
}

/// How much of a remote address is removed before fetching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strip {
    /// Known tracking parameters and those carrying the user's address.
    All,
    /// Only parameters carrying the user's address.
    Address,
    Nothing,
}

/// What [`strip`] removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stripped {
    pub url: String,
    /// Known tracking parameters were removed.
    pub tracking: bool,
    /// Parameters carrying the user's address were removed.
    pub address: bool,
}

/// `url` without the query parameters `level` removes. A parameter carries
/// `address` when its value is the address, contains it plainly or
/// percent-encoded, as base64, or as an MD5, SHA-1 or SHA-256 hash, or is
/// exactly the address's local part.
pub fn strip(url: &str, level: Strip, address: Option<&str>) -> Stripped {
    let unchanged = Stripped { url: url.to_string(), tracking: false, address: false };
    if level == Strip::Nothing {
        return unchanged;
    }
    let main_end = url.find('#').unwrap_or(url.len());
    let Some(q) = url[..main_end].find('?') else { return unchanged };
    let forms = address.map(address_forms).unwrap_or_default();
    let local = address.and_then(|a| a.split('@').next()).map(str::to_ascii_lowercase);
    let (mut tracking, mut carried) = (false, false);
    let kept: Vec<&str> = url[q + 1..main_end]
        .split('&')
        .filter(|p| {
            if p.is_empty() {
                return false;
            }
            if level == Strip::All && crate::links::is_tracking(p) {
                tracking = true;
                return false;
            }
            let value = p.split_once('=').map_or("", |(_, v)| v);
            let decoded = glib::uri_unescape_string(value.replace('+', " "), None::<&str>)
                .map(|s| s.to_string())
                .unwrap_or_else(|| value.to_string());
            let lower = decoded.to_ascii_lowercase();
            let carries = forms.iter().any(|f| f.matches(&decoded, &lower))
                || local.as_deref().is_some_and(|l| !l.is_empty() && lower == l);
            if carries {
                carried = true;
            }
            !carries
        })
        .collect();
    if !tracking && !carried {
        return unchanged;
    }
    let mut out = url[..q].to_string();
    if !kept.is_empty() {
        out.push('?');
        out.push_str(&kept.join("&"));
    }
    out.push_str(&url[main_end..]);
    Stripped { url: out, tracking, address: carried }
}

enum Form {
    /// Compared without regard to case: the address and its hashes.
    Caseless(String),
    /// Base64, where case matters.
    Exact(String),
}

impl Form {
    fn matches(&self, value: &str, lower: &str) -> bool {
        match self {
            Form::Caseless(f) => lower.contains(f.as_str()),
            Form::Exact(f) => value.contains(f.as_str()),
        }
    }
}

fn address_forms(address: &str) -> Vec<Form> {
    let a = address.trim().to_ascii_lowercase();
    let mut forms = vec![Form::Caseless(a.clone())];
    for kind in [glib::ChecksumType::Md5, glib::ChecksumType::Sha1, glib::ChecksumType::Sha256] {
        if let Some(h) = glib::compute_checksum_for_string(kind, a.as_str()) {
            forms.push(Form::Caseless(h.to_string()));
        }
    }
    let b64 = glib::base64_encode(a.as_bytes()).to_string();
    // Without padding, as a prefix of longer encoded data would show it,
    // and in the URL-safe alphabet.
    let bare = b64.trim_end_matches('=').to_string();
    forms.push(Form::Exact(bare.replace('+', "-").replace('/', "_")));
    forms.push(Form::Exact(bare));
    forms
}

fn key(url: &str) -> String {
    glib::compute_checksum_for_string(glib::ChecksumType::Sha256, url)
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// The cached image and its MIME type; marks it as just used.
fn cached(url: &str) -> Option<(Vec<u8>, String)> {
    let base = cache_dir().join(key(url));
    let data = std::fs::read(&base).ok()?;
    let mime = std::fs::read_to_string(base.with_extension("type")).ok()?;
    if let Ok(f) = std::fs::File::options().write(true).open(&base) {
        let _ = f.set_modified(SystemTime::now());
    }
    Some((data, mime))
}

fn store(url: &str, data: &[u8], mime: &str) {
    let dir = cache_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let base = dir.join(key(url));
    if std::fs::write(&base, data).is_ok() {
        let _ = std::fs::write(base.with_extension("type"), mime);
    }
    trim(&dir);
}

/// Deletes the least recently used images until the cache fits its limit.
fn trim(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_none())
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some((m.modified().ok()?, m.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= CACHE_LIMIT {
        return;
    }
    files.sort_by_key(|f| f.0);
    for (_, len, path) in files {
        if total <= CACHE_LIMIT {
            break;
        }
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("type"));
        total = total.saturating_sub(len);
    }
}

thread_local! {
    static SESSION: soup::Session = {
        // No cookie jar is added, so no cookies are sent or kept.
        let s = soup::Session::new();
        s.set_timeout(30);
        s.set_user_agent("Mozilla/5.0");
        s
    };
}

fn error(msg: &str) -> glib::Error {
    glib::Error::new(gio::IOErrorEnum::Failed, msg)
}

/// What a remote load may be: an image, a stylesheet or a font. Fonts are
/// often served as application/octet-stream, accepted for font file names.
fn acceptable(mime: &str, url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase();
    let font_file = [".woff", ".woff2", ".ttf", ".otf", ".eot"].iter().any(|e| path.ends_with(e));
    mime.starts_with("image/")
        || mime == "text/css"
        || mime.starts_with("font/")
        || mime.starts_with("application/font-")
        || mime.starts_with("application/x-font-")
        || mime == "application/vnd.ms-fontobject"
        || (font_file && mime == "application/octet-stream")
}

/// The content at `url` from the cache, else from the network into the
/// cache: an image, stylesheet or font, and its MIME type.
pub async fn fetch(url: &str) -> Result<(Vec<u8>, String), glib::Error> {
    if let Some(hit) = cached(url) {
        return Ok(hit);
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(error("Not a web address"));
    }
    let msg = soup::Message::new("GET", url).map_err(|e| error(&e.to_string()))?;
    let session = SESSION.with(|s| s.clone());
    let stream = session.send_future(&msg, glib::Priority::DEFAULT).await?;
    if msg.status_code() != 200 {
        return Err(error(&format!("HTTP status {}", msg.status_code())));
    }
    let mime = msg
        .response_headers()
        .and_then(|h| h.content_type())
        .map(|(t, _)| t.to_ascii_lowercase())
        .unwrap_or_default();
    if !acceptable(&mime, url) {
        return Err(error("Not an image, stylesheet or font"));
    }
    let limit = if mime == "text/css" { CSS_LIMIT } else { IMAGE_LIMIT };
    let mut data = Vec::new();
    loop {
        let chunk = stream.read_bytes_future(65536, glib::Priority::DEFAULT).await?;
        if chunk.is_empty() {
            break;
        }
        data.extend_from_slice(&chunk);
        if data.len() > limit {
            return Err(error("Too large"));
        }
    }
    store(url, &data, &mime);
    Ok((data, mime))
}

/// The body at `url` if the server answers 200, up to `limit` bytes.
/// Nothing is cached.
pub async fn fetch_bytes(url: &str, limit: usize) -> Result<Vec<u8>, glib::Error> {
    let msg = soup::Message::new("GET", url).map_err(|e| error(&e.to_string()))?;
    let session = SESSION.with(|s| s.clone());
    let stream = session.send_future(&msg, glib::Priority::DEFAULT).await?;
    if msg.status_code() != 200 {
        return Err(error(&format!("HTTP status {}", msg.status_code())));
    }
    let mut data = Vec::new();
    loop {
        let chunk = stream.read_bytes_future(65536, glib::Priority::DEFAULT).await?;
        if chunk.is_empty() {
            break;
        }
        data.extend_from_slice(&chunk);
        if data.len() > limit {
            return Err(error("Response too large"));
        }
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(url: &str) -> String {
        local_uri(url)
    }

    #[test]
    fn rewriting() {
        let html = concat!(
            r#"<img src="https://a.example/x.png?a=1&amp;b=2"><td background='//b.example/bg.gif'>"#,
            r#"<img srcset="https://c.example/1x.png 1x, https://c.example/2x.png 2x" src=cid:logo>"#,
            r#"<div style="background:url(https://d.example/d.jpg)"></div>"#,
            r#"<style>@import url("https://e.example/s.css"); @font-face { src: url(https://f.example/f.woff) }</style>"#
        );
        let out = rewrite(html);
        assert!(out.contains(&format!("src=\"{}\"", local("https://a.example/x.png?a=1&b=2"))));
        assert!(out.contains(&format!("background=\"{}\"", local("https://b.example/bg.gif"))));
        assert!(out.contains(&format!("{} 1x, {} 2x", local("https://c.example/1x.png"), local("https://c.example/2x.png"))));
        assert!(out.contains("src=cid:logo"));
        assert!(out.contains(&format!("url(\"{}\")", local("https://d.example/d.jpg"))));
        assert!(out.contains(&format!("@import url(\"{}\")", local("https://e.example/s.css"))));
        assert!(out.contains(&format!("url(\"{}\")", local("https://f.example/f.woff"))));
        assert_eq!(remote_url(&local("https://a.example/x.png?a=1&b=2")).as_deref(), Some("https://a.example/x.png?a=1&b=2"));
        let links = rewrite(r#"<link rel="stylesheet" href="https://g.example/a.css"><link rel=icon href="https://g.example/i.png">"#);
        assert!(links.contains(&format!("href=\"{}\"", local("https://g.example/a.css"))));
        assert!(links.contains(r#"href="https://g.example/i.png""#));
    }

    #[test]
    fn stylesheets() {
        let css = r#"@import "more.css"; a { background: url(../img/bg.png) } b { fill: url(#grad) } c { src: url(data:font/woff2;base64,AA) }"#;
        let out = rewrite_css(css, Some("https://h.example/css/main.css"));
        assert!(out.contains(&format!("@import url(\"{}\")", local("https://h.example/css/more.css"))));
        assert!(out.contains(&format!("url(\"{}\")", local("https://h.example/img/bg.png"))));
        assert!(out.contains("url(#grad)"));
        assert!(out.contains("url(data:font/woff2;base64,AA)"));
    }

    #[test]
    fn stripping() {
        let me = Some("Aj@Example.com");
        let s = strip("https://t.example/p.gif?id=5&utm_source=x&to=aj%40example.com#f", Strip::All, me);
        assert_eq!(s, Stripped { url: "https://t.example/p.gif?id=5#f".into(), tracking: true, address: true });
        let s = strip("https://t.example/p.gif?utm_source=x&sent_to=aj", Strip::Address, me);
        assert_eq!(s.url, "https://t.example/p.gif?utm_source=x");
        assert!(!s.tracking && s.address);
        let md5 = glib::compute_checksum_for_string(glib::ChecksumType::Md5, "aj@example.com").unwrap().to_uppercase();
        assert_eq!(strip(&format!("https://t.example/o?h={md5}"), Strip::Address, me).url, "https://t.example/o");
        let b64 = glib::base64_encode(b"aj@example.com").to_string();
        let b64 = glib::Uri::escape_string(&b64, None, false);
        assert_eq!(strip(&format!("https://t.example/o?e={b64}&k=1"), Strip::Address, me).url, "https://t.example/o?k=1");
        let plain = "https://t.example/a.png?w=600";
        assert_eq!(strip(plain, Strip::All, me).url, plain);
        assert_eq!(strip("https://t.example/a.png?utm_x=1", Strip::Nothing, me).url, "https://t.example/a.png?utm_x=1");
    }
}
