//! Sender verification from the `Authentication-Results` header (RFC 8601)
//! that the user's provider adds on delivery. Only the topmost header is
//! read: headers further down were added before the provider, possibly by
//! the sender. A sender can still forge a topmost header when the provider
//! adds none, which is why only failures are acted on.

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Verification {
    /// The DMARC result ("pass", "fail", "none" and so on), lowercased.
    pub dmarc: Option<String>,
    /// The From domain DMARC was evaluated for (`header.from`).
    pub from_domain: Option<String>,
    /// Domains whose DKIM signature passed (`header.d`).
    pub signed_by: Vec<String>,
    /// The envelope sender's domain SPF checked (`smtp.mailfrom`).
    pub mailed_by: Option<String>,
}

impl Verification {
    /// The domain of `email` when DMARC passed for exactly that domain:
    /// the message demonstrably comes from it.
    pub fn verified_domain(&self, email: &str) -> Option<String> {
        let domain = email.rsplit_once('@')?.1.trim().to_ascii_lowercase();
        (self.dmarc.as_deref() == Some("pass") && self.from_domain.as_deref().is_some_and(|f| f.eq_ignore_ascii_case(&domain)))
            .then_some(domain)
    }

    /// DMARC failed: the From domain did not send this message as far as
    /// the provider could tell.
    pub fn failed(&self) -> bool {
        self.dmarc.as_deref() == Some("fail")
    }
}

/// Removes RFC 5322 comments, which may nest.
fn strip_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0u32;
    let mut quoted = false;
    let mut escaped = false;
    for c in s.chars() {
        if escaped {
            escaped = false;
            if depth == 0 {
                out.push(c);
            }
            continue;
        }
        match c {
            '\\' => {
                escaped = true;
                if depth == 0 {
                    out.push(c);
                }
            }
            '"' if depth == 0 => {
                quoted = !quoted;
                out.push(c);
            }
            '(' if !quoted => depth += 1,
            ')' if !quoted && depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

fn domain_of(value: &str) -> String {
    let v = value.trim_matches('"');
    v.rsplit('@').next().unwrap_or(v).to_ascii_lowercase()
}

/// Parses one `Authentication-Results` value. Unknown methods and
/// properties are ignored.
pub fn parse(header: &str) -> Verification {
    let mut v = Verification::default();
    let text = strip_comments(&header.replace(['\r', '\n'], " "));
    // The first part is the authserv-id, which says who did the checks.
    for resinfo in text.split(';').skip(1) {
        let mut words = resinfo.split_whitespace();
        let Some((method, result)) = words.next().and_then(|w| w.split_once('=')) else { continue };
        let method = method.to_ascii_lowercase();
        let result = result.to_ascii_lowercase();
        let props: Vec<(String, String)> = words
            .filter_map(|w| w.split_once('='))
            .map(|(k, val)| (k.to_ascii_lowercase(), val.to_string()))
            .collect();
        let prop = |name: &str| props.iter().find(|(k, _)| k == name).map(|(_, val)| val.clone());
        match method.as_str() {
            "dmarc" if v.dmarc.is_none() => {
                v.dmarc = Some(result);
                v.from_domain = prop("header.from").map(|d| domain_of(&d));
            }
            "dkim" if result == "pass" => {
                if let Some(d) = prop("header.d").or_else(|| prop("header.i")).map(|d| domain_of(&d))
                    && !v.signed_by.contains(&d)
                {
                    v.signed_by.push(d);
                }
            }
            "spf" if result == "pass" && v.mailed_by.is_none() => {
                v.mailed_by = prop("smtp.mailfrom").map(|d| domain_of(&d));
            }
            _ => {}
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gmail_style() {
        let h = "mx.google.com;\r\n       dkim=pass header.i=@news.example.com header.s=s1 header.b=abc;\r\n       spf=pass (google.com: domain of bounce@mail.example.com designates 1.2.3.4 as permitted sender) smtp.mailfrom=bounce@mail.example.com;\r\n       dmarc=pass (p=REJECT sp=REJECT dis=NONE) header.from=example.com";
        let v = parse(h);
        assert_eq!(v.dmarc.as_deref(), Some("pass"));
        assert_eq!(v.from_domain.as_deref(), Some("example.com"));
        assert_eq!(v.signed_by, vec!["news.example.com".to_string()]);
        assert_eq!(v.mailed_by.as_deref(), Some("mail.example.com"));
        assert!(!v.failed());
    }

    #[test]
    fn failure() {
        let v = parse("mx.example.net; dkim=fail header.d=chase.com; spf=softfail smtp.mailfrom=x@evil.example; dmarc=FAIL (p=reject) header.from=chase.com");
        assert!(v.failed());
        assert_eq!(v.from_domain.as_deref(), Some("chase.com"));
        assert!(v.signed_by.is_empty());
        assert_eq!(v.mailed_by, None);
    }

    #[test]
    fn nothing_known() {
        assert_eq!(parse("mx.example.net; none"), Verification::default());
        assert_eq!(parse("mx.example.net 1; arc=pass (x (nested) y) smtp.remote-ip=1.2.3.4"), Verification::default());
    }
}
