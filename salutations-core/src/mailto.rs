//! `mailto:` URIs (RFC 6068). Only fields a message starts with are
//! taken; other headers, and the non-standard `attach`, are ignored, since
//! a link must not be able to attach local files or set arbitrary headers.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mailto {
    /// Comma-separated, as typed into a recipient field.
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    /// Without angle brackets.
    pub in_reply_to: Option<String>,
}

pub fn parse(uri: &str) -> Option<Mailto> {
    let rest = uri.get(..7).filter(|p| p.eq_ignore_ascii_case("mailto:")).map(|_| &uri[7..])?;
    let (to, query) = rest.split_once('?').unwrap_or((rest, ""));
    let mut m = Mailto { to: decode(to), ..Default::default() };
    let mut to = vec![m.to.clone()];
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = decode(value);
        match key.to_ascii_lowercase().as_str() {
            "to" => to.push(value),
            "cc" => m.cc = join(&m.cc, &value),
            "bcc" => m.bcc = join(&m.bcc, &value),
            "subject" => m.subject = value,
            // Line breaks are CRLF in the URI.
            "body" => m.body = value.replace("\r\n", "\n"),
            "in-reply-to" => {
                let id = value.trim().trim_start_matches('<').trim_end_matches('>').to_string();
                m.in_reply_to = (!id.is_empty()).then_some(id);
            }
            _ => {}
        }
    }
    m.to = to.iter().filter(|t| !t.trim().is_empty()).cloned().collect::<Vec<_>>().join(", ");
    Some(m)
}

fn join(a: &str, b: &str) -> String {
    match (a.trim().is_empty(), b.trim().is_empty()) {
        (true, _) => b.to_string(),
        (_, true) => a.to_string(),
        _ => format!("{a}, {b}"),
    }
}

/// Percent-decoding; "+" stays "+", as RFC 6068 has no form encoding.
fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields() {
        let m = parse("mailto:a@x.org,b@x.org?cc=c@x.org&subject=Hi%20there&body=Line%201%0D%0ALine%202&In-Reply-To=%3Cid@x%3E&attach=/etc/passwd&X-Evil=1").unwrap();
        assert_eq!(m.to, "a@x.org,b@x.org");
        assert_eq!(m.cc, "c@x.org");
        assert_eq!(m.subject, "Hi there");
        assert_eq!(m.body, "Line 1\nLine 2");
        assert_eq!(m.in_reply_to.as_deref(), Some("id@x"));
        assert_eq!(parse("MAILTO:?to=d@x.org").unwrap().to, "d@x.org");
        assert_eq!(parse("mailto:a%2Bb@x.org").unwrap().to, "a+b@x.org");
        assert!(parse("https://x.org").is_none());
        assert_eq!(decode("100%"), "100%");
    }
}
