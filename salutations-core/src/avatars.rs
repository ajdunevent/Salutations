//! Avatar sources besides the address book: OpenPGP photo IDs, the `Face:`
//! header, BIMI brand logos, Libravatar and Gravatar. Parsing and network
//! requests only; choosing between sources and caching happen in the app.

use gio::prelude::*;
use soup::prelude::*;

/// The largest avatar image or BIMI logo accepted, in bytes.
pub const LIMIT: usize = 1024 * 1024;

/// The SHA-256 of the trimmed, lowercased address, as Libravatar and
/// Gravatar expect it.
pub fn address_hash(email: &str) -> String {
    glib::compute_checksum_for_string(glib::ChecksumType::Sha256, email.trim().to_lowercase().as_str())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// The PNG in a `Face:` header value (base64, folded).
pub fn face_png(value: &str) -> Option<Vec<u8>> {
    let compact: String = value.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = glib::base64_decode(&compact);
    bytes.starts_with(b"\x89PNG\r\n\x1a\n").then_some(bytes)
}

/// The logo URL in a BIMI record ("v=BIMI1; l=https://…; a=…"); None for
/// another version, an empty `l=` (the domain declines BIMI) or a non-HTTPS
/// location.
pub fn bimi_location(record: &str) -> Option<String> {
    let mut version = None;
    let mut location = None;
    for part in record.split(';') {
        let Some((k, v)) = part.split_once('=') else { continue };
        match k.trim().to_ascii_lowercase().as_str() {
            "v" => version = Some(v.trim().to_string()),
            "l" => location = Some(v.trim().to_string()),
            _ => {}
        }
    }
    if version.as_deref() != Some("BIMI1") {
        return None;
    }
    // l= may list several, comma-separated; the first is used.
    let url = location?.split(',').next()?.trim().to_string();
    url.starts_with("https://").then_some(url)
}

/// Addresses whose keys carry a photo ID, from `gpg --with-colons
/// --list-keys`: each `uat` record marks the key it belongs to, and the
/// key's `uid` records give its addresses. Lowercased.
pub fn photo_addresses(colons: &str) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    let mut uids: Vec<String> = Vec::new();
    let mut has_photo = false;
    let mut flush = |uids: &mut Vec<String>, has: bool| {
        if has {
            out.extend(uids.drain(..));
        }
        uids.clear();
    };
    for line in colons.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        match fields.first().copied() {
            Some("pub") => {
                flush(&mut uids, has_photo);
                has_photo = false;
            }
            Some("uid") => {
                // Field 10 is the user ID, with ':' escaped as \x3a.
                let uid = fields.get(9).copied().unwrap_or_default().replace("\\x3a", ":");
                if let Some(email) = uid.rsplit_once('<').and_then(|(_, r)| r.split_once('>')).map(|(e, _)| e) {
                    uids.push(email.trim().to_lowercase());
                } else if uid.contains('@') {
                    uids.push(uid.trim().to_lowercase());
                }
            }
            // A revoked ("r") or expired ("e") photo is not used.
            Some("uat") if !matches!(fields.get(1).copied(), Some("r" | "e")) => has_photo = true,
            _ => {}
        }
    }
    flush(&mut uids, has_photo);
    out
}

/// The first JPEG photo ID in an exported key (OpenPGP packets, RFC 4880
/// §5.12: a User Attribute packet holding an image subpacket).
pub fn photo_jpeg(key: &[u8]) -> Option<Vec<u8>> {
    let mut packets = Packets { data: key, pos: 0 };
    while let Some((tag, body)) = packets.next() {
        if tag != 17 {
            continue;
        }
        let mut sub = 0;
        while sub < body.len() {
            let (len, used) = subpacket_length(&body[sub..])?;
            let start = sub + used;
            let end = start.checked_add(len)?.min(body.len());
            if start < end && body[start] == 1 {
                // Image header: 16-byte little-endian length, version 1,
                // encoding 1 (JPEG), then reserved bytes.
                let image = &body[start + 1..end];
                if image.len() > 16 && image[0..2] == [16, 0] && image[2] == 1 && image[3] == 1 {
                    return Some(image[16..].to_vec());
                }
            }
            sub = end;
        }
    }
    None
}

fn subpacket_length(b: &[u8]) -> Option<(usize, usize)> {
    let first = *b.first()? as usize;
    match first {
        0..=191 => Some((first, 1)),
        192..=254 => Some((((first - 192) << 8) + *b.get(1)? as usize + 192, 2)),
        255 => Some((u32::from_be_bytes(b.get(1..5)?.try_into().ok()?) as usize, 5)),
        _ => None,
    }
}

struct Packets<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Packets<'a> {
    /// The next packet's tag and body; None at the end or on bad data.
    fn next(&mut self) -> Option<(u8, &'a [u8])> {
        let d = self.data;
        let header = *d.get(self.pos)?;
        if header & 0x80 == 0 {
            return None;
        }
        let (tag, len, used) = if header & 0x40 != 0 {
            // New format.
            let tag = header & 0x3f;
            let b = d.get(self.pos + 1..)?;
            let first = *b.first()? as usize;
            match first {
                0..=191 => (tag, first, 2),
                192..=223 => (tag, ((first - 192) << 8) + *b.get(1)? as usize + 192, 3),
                255 => (tag, u32::from_be_bytes(b.get(1..5)?.try_into().ok()?) as usize, 6),
                // Partial lengths are not used by key packets.
                _ => return None,
            }
        } else {
            let tag = (header >> 2) & 0x0f;
            match header & 3 {
                0 => (tag, *d.get(self.pos + 1)? as usize, 2),
                1 => (tag, u16::from_be_bytes(d.get(self.pos + 1..self.pos + 3)?.try_into().ok()?) as usize, 3),
                2 => (tag, u32::from_be_bytes(d.get(self.pos + 1..self.pos + 5)?.try_into().ok()?) as usize, 5),
                _ => return None,
            }
        };
        let start = self.pos + used;
        let end = start.checked_add(len)?;
        let body = d.get(start..end)?;
        self.pos = end;
        Some((tag, body))
    }
}

thread_local! {
    static SESSION: soup::Session = {
        // No cookie jar, so no cookies; Soup sends no referrer.
        let s = soup::Session::new();
        s.set_timeout(20);
        s.set_user_agent("Mozilla/5.0");
        s
    };
}

/// The image at `url`: Some on 200, None on 404 or a redirect (never
/// followed, so Libravatar's fallback to Gravatar stays off).
pub async fn fetch(url: &str) -> Result<Option<Vec<u8>>, glib::Error> {
    let msg = soup::Message::new("GET", url).map_err(|e| glib::Error::new(gio::IOErrorEnum::InvalidArgument, &e.to_string()))?;
    msg.add_flags(soup::MessageFlags::NO_REDIRECT);
    let session = SESSION.with(|s| s.clone());
    let stream = session.send_future(&msg, glib::Priority::LOW).await?;
    match msg.status_code() {
        200 => {}
        300..=399 | 404 => return Ok(None),
        n => return Err(glib::Error::new(gio::IOErrorEnum::Failed, &format!("HTTP status {n}"))),
    }
    let mut data = Vec::new();
    loop {
        let chunk = stream.read_bytes_future(65536, glib::Priority::LOW).await?;
        if chunk.is_empty() {
            break;
        }
        data.extend_from_slice(&chunk);
        if data.len() > LIMIT {
            return Err(glib::Error::new(gio::IOErrorEnum::Failed, "Image too large"));
        }
    }
    Ok(Some(data))
}

/// Libravatar's server for `domain`: its `_avatars-sec._tcp` SRV record
/// (lowest priority, then highest weight), else the public service.
pub async fn libravatar_base(domain: &str) -> String {
    let resolver = gio::Resolver::default();
    let found = resolver.lookup_service_future("avatars-sec", "tcp", domain).await.ok();
    found
        .and_then(|mut targets| {
            targets.sort_by_key(|t| {
                let mut t = t.clone();
                (t.priority(), std::cmp::Reverse(t.weight()))
            });
            targets.into_iter().next()
        })
        .map(|mut t| match t.port() {
            443 => format!("https://{}", t.hostname()),
            p => format!("https://{}:{p}", t.hostname()),
        })
        .unwrap_or_else(|| "https://seccdn.libravatar.org".into())
}

/// The BIMI logo URL for `domain`: its `default._bimi` TXT record, else
/// the organizational domain's. Ok(None) when neither has one; an error
/// when DNS could not answer, so a passing failure is not remembered.
pub async fn bimi_url(domain: &str, organizational: Option<&str>) -> Result<Option<String>, glib::Error> {
    let resolver = gio::Resolver::default();
    let mut names = vec![domain.to_string()];
    if let Some(o) = organizational.filter(|o| *o != domain) {
        names.push(o.to_string());
    }
    for name in names {
        let records = match resolver.lookup_records_future(&format!("default._bimi.{name}"), gio::ResolverRecordType::Txt).await {
            Ok(r) => r,
            Err(e) if e.matches(gio::ResolverError::NotFound) => continue,
            Err(e) => return Err(e),
        };
        for r in records {
            // Each TXT record is a tuple of strings, joined.
            let Some((parts,)) = r.get::<(Vec<String>,)>() else { continue };
            if let Some(url) = bimi_location(&parts.concat()) {
                return Ok(Some(url));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes() {
        assert_eq!(address_hash(" Test@Example.com "), address_hash("test@example.com"));
        assert_eq!(address_hash("test@example.com").len(), 64);
    }

    #[test]
    fn bimi() {
        assert_eq!(bimi_location("v=BIMI1; l=https://x.example/logo.svg; a="), Some("https://x.example/logo.svg".into()));
        assert_eq!(bimi_location("v=BIMI1; l=; a="), None);
        assert_eq!(bimi_location("v=BIMI1; l=http://x.example/l.svg"), None);
        assert_eq!(bimi_location("v=BIMI2; l=https://x.example/l.svg"), None);
    }

    #[test]
    fn colons() {
        let out = "pub:u:255:22:AAAA:1:::u:::scESC::::::23::0:\n\
                   uid:u::::1::H::Ann <Ann@Example.org>::::::::::0:\n\
                   uat:u::::1::H::1 3486::::::::::0:\n\
                   pub:u:255:22:BBBB:1:::u:::scESC::::::23::0:\n\
                   uid:u::::1::H::Bob <bob@example.org>::::::::::0:\n";
        let set = photo_addresses(out);
        assert!(set.contains("ann@example.org"));
        assert!(!set.contains("bob@example.org"));
    }

    #[test]
    fn photo_packet() {
        // A new-format User Attribute packet with one JPEG image subpacket.
        let jpeg = [0xff, 0xd8, 0xff, 0xd9];
        let mut image = vec![1u8, 16, 0, 1, 1];
        image.extend([0u8; 12]);
        image.extend(jpeg);
        let mut body = vec![image.len() as u8];
        body.extend(&image);
        let mut packet = vec![0xc0 | 17, body.len() as u8];
        packet.extend(&body);
        // A user ID packet first, which is skipped.
        let mut key = vec![0xc0 | 13, 3, b'a', b'b', b'c'];
        key.extend(packet);
        assert_eq!(photo_jpeg(&key), Some(jpeg.to_vec()));
    }

    #[test]
    fn face() {
        assert!(face_png("iVBORw0KGgo=").is_some());
        assert!(face_png("aGVsbG8=").is_none());
    }
}
