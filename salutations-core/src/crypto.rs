//! OpenPGP: PGP/MIME signing, encryption, decryption and verification
//! through Camel's GnuPG context, the user's secret keys, key lookup by WKD
//! and keyserver, and keys carried in messages (Autocrypt headers and
//! `application/pgp-keys` attachments). Functions ending in `_sync` run
//! gpg and block.

use camel::prelude::*;
use camel::{GpgContext, MimeMessage, MimePart, Multipart};

pub use camel::{KeyInfo, SignStatus, Signer, Validity};

use crate::compose;

/// A secret key that can sign.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretKey {
    pub fingerprint: String,
    /// The primary user ID, such as "Name <address>".
    pub user_id: String,
}

/// The MIME type of an attached public key.
pub const KEY_MIME_TYPE: &str = "application/pgp-keys";

/// The account's public key as an attachment, armored. Runs gpg; blocks.
pub fn key_attachment_sync(ctx: &camel::GpgContext, signer: &str, email: &str) -> Result<crate::compose::Attachment, glib::Error> {
    let key = ctx.public_key_sync(signer)?;
    Ok(crate::compose::Attachment {
        name: key_file_name(email),
        mime_type: KEY_MIME_TYPE.into(),
        data: glib::Bytes::from_owned(armor_public_key(&key).into_bytes()),
    })
}

/// The name an attached key goes by, as Thunderbird names it.
pub fn key_file_name(email: &str) -> String {
    format!("OpenPGP_{email}.asc")
}

/// Secret keys in the user's keyring able to sign, without expired,
/// revoked or disabled ones. Runs gpg; blocks.
pub fn secret_keys_sync() -> Vec<SecretKey> {
    list_secret_keys_sync().unwrap_or_default()
}

/// As [`secret_keys_sync`], or why gpg could not list them: its last line
/// of error output when it failed.
pub fn list_secret_keys_sync() -> Result<Vec<SecretKey>, String> {
    let out = std::process::Command::new("gpg")
        .args(["--batch", "--with-colons", "--fixed-list-mode", "--list-secret-keys"])
        .output()
        .map_err(|e| format!("Cannot run gpg: {e}"))?;
    let keys = parse_secret_keys(&String::from_utf8_lossy(&out.stdout));
    if !out.status.success() && keys.is_empty() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let line = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("gpg failed").trim();
        return Err(line.trim_start_matches("gpg: ").to_string());
    }
    Ok(keys)
}

fn parse_secret_keys(listing: &str) -> Vec<SecretKey> {
    let mut keys = Vec::new();
    // (usable, fingerprint, first user ID) of the key being read.
    let mut current: Option<(bool, Option<String>, Option<String>)> = None;
    let finish = |c: Option<(bool, Option<String>, Option<String>)>, keys: &mut Vec<SecretKey>| {
        if let Some((true, Some(fpr), uid)) = c {
            keys.push(SecretKey { fingerprint: fpr, user_id: uid.unwrap_or_default() });
        }
    };
    for line in listing.lines() {
        let f: Vec<&str> = line.split(':').collect();
        match f.first().copied() {
            Some("sec") => {
                finish(current.take(), &mut keys);
                let validity = f.get(1).copied().unwrap_or("");
                let caps = f.get(11).copied().unwrap_or("");
                let usable = !matches!(validity, "e" | "r" | "d" | "i") && caps.contains('S') && !caps.contains('D');
                current = Some((usable, None, None));
            }
            // The first fpr and uid after "sec" are the primary key's.
            Some("fpr") => {
                if let Some((_, fpr @ None, _)) = current.as_mut() {
                    *fpr = f.get(9).map(|s| s.to_string());
                }
            }
            Some("uid") => {
                if let Some((_, _, uid @ None)) = current.as_mut() {
                    let validity = f.get(1).copied().unwrap_or("");
                    if !matches!(validity, "r" | "e") {
                        *uid = f.get(9).map(|s| unescape_colons(s));
                    }
                }
            }
            Some("ssb") => {
                // Subkeys follow; the primary key's lines are complete.
            }
            _ => {}
        }
    }
    finish(current.take(), &mut keys);
    keys
}

/// gpg escapes ":" and other bytes in colon listings as "\xNN".
fn unescape_colons(s: &str) -> String {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1] == b'x' {
            if let Ok(v) = u8::from_str_radix(&s[i + 2..i + 4], 16) {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The address in a user ID such as "Name <address>", lowercased.
pub fn user_id_email(uid: &str) -> Option<String> {
    let addr = match (uid.rfind('<'), uid.rfind('>')) {
        (Some(a), Some(b)) if a < b => &uid[a + 1..b],
        _ if uid.contains('@') && !uid.contains(' ') => uid,
        _ => return None,
    };
    Some(addr.trim().to_ascii_lowercase())
}

/// Whether any user ID of `key` is `address`.
pub fn key_has_address(key: &KeyInfo, address: &str) -> bool {
    key.user_ids.iter().any(|u| user_id_email(u).as_deref() == Some(&address.to_ascii_lowercase()))
}

/// The context used for every operation. Encryption trusts any key in the
/// keyring for the address; verification still reports uncertified keys,
/// since --always-trust only applies to encryption.
pub fn context(session: &crate::Session) -> GpgContext {
    context_with(Some(session))
}

/// [`context`] without a session, for tests.
pub fn context_with(session: Option<&crate::Session>) -> GpgContext {
    let ctx = GpgContext::new(session);
    ctx.set_always_trust(true);
    ctx
}

// Armor and Autocrypt

fn crc24(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xB7_04CE;
    for &byte in data {
        crc ^= (byte as u32) << 16;
        for _ in 0..8 {
            crc <<= 1;
            if crc & 0x100_0000 != 0 {
                crc ^= 0x186_4CFB;
            }
        }
    }
    crc & 0xFF_FFFF
}

/// A binary public key as an ASCII-armored block (RFC 4880 6.2).
pub fn armor_public_key(data: &[u8]) -> String {
    let b64 = glib::base64_encode(data).to_string();
    let mut out = String::from("-----BEGIN PGP PUBLIC KEY BLOCK-----\n\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
        out.push('\n');
    }
    let crc = crc24(data).to_be_bytes();
    out.push('=');
    out.push_str(&glib::base64_encode(&crc[1..]));
    out.push_str("\n-----END PGP PUBLIC KEY BLOCK-----\n");
    out
}

/// An Autocrypt header value for `address` carrying `key` (binary).
/// Folded so no line of the message exceeds 78 characters.
pub fn autocrypt_header(address: &str, key: &[u8]) -> String {
    let b64 = glib::base64_encode(key).to_string();
    let mut out = format!("addr={address}; keydata=");
    for chunk in b64.as_bytes().chunks(72) {
        out.push_str("\n ");
        out.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
    }
    out
}

/// A key found in a message for its sender.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarriedKey {
    /// The address the key is offered for, lowercased.
    pub address: String,
    pub data: Vec<u8>,
}

/// The key in an Autocrypt header value. Headers with an unknown attribute
/// not starting with "_" are ignored, as the Autocrypt specification asks.
pub fn parse_autocrypt(value: &str) -> Option<CarriedKey> {
    let mut addr = None;
    let mut keydata = None;
    for attr in value.split(';') {
        let (k, v) = attr.split_once('=')?;
        let (k, v) = (k.trim(), v.trim());
        match k {
            "addr" => addr = Some(v.to_ascii_lowercase()),
            "keydata" => keydata = Some(v.split_whitespace().collect::<String>()),
            "prefer-encrypt" => {}
            k if k.starts_with('_') => {}
            _ => return None,
        }
    }
    let data = glib::base64_decode(&keydata?);
    let address = addr?;
    (!data.is_empty()).then_some(CarriedKey { address, data })
}

/// Keys the message carries for its sender: its Autocrypt header and any
/// `application/pgp-keys` attachments (in the clear or inside `content`,
/// the decrypted body).
pub fn carried_keys(msg: &MimeMessage, content: Option<&MimePart>) -> Vec<CarriedKey> {
    let from = msg
        .from()
        .and_then(|f| f.get(0))
        .and_then(|(_, e)| e.map(|e| e.to_ascii_lowercase()))
        .unwrap_or_default();
    let mut out = Vec::new();
    if let Some(k) = msg.header("Autocrypt").and_then(|h| parse_autocrypt(&h))
        && k.address == from
    {
        out.push(k);
    }
    let mut parts = Vec::new();
    collect_key_parts(msg.upcast_ref(), &mut parts);
    if let Some(c) = content {
        collect_key_parts(c, &mut parts);
    }
    for p in parts {
        if let Some(data) = MediumExt::content(&p).map(|c| crate::render::decode_public(&c)) {
            out.push(CarriedKey { address: from.clone(), data });
        }
    }
    out
}

fn collect_key_parts(part: &MimePart, out: &mut Vec<MimePart>) {
    let ct = part.content_type();
    if ct.as_ref().is_some_and(|c| c.is("application", "pgp-keys")) {
        out.push(part.clone());
        return;
    }
    let Some(content) = MediumExt::content(part) else { return };
    if let Some(mp) = content.downcast_ref::<Multipart>() {
        // Encrypted and signed data is handled by unprotect().
        if ct.as_ref().is_some_and(|c| c.is("multipart", "encrypted")) {
            return;
        }
        for p in (0..mp.number()).filter_map(|i| mp.part(i)) {
            collect_key_parts(&p, out);
        }
    }
}

// Reading

/// What protection a shown message had.
#[derive(Debug, Clone, Default)]
pub struct Protection {
    pub encrypted: bool,
    /// Decryption failed; the message cannot be shown.
    pub decrypt_error: Option<String>,
    pub signature: Option<Signature>,
}

#[derive(Debug, Clone)]
pub struct Signature {
    pub status: SignStatus,
    pub signer: Option<Signer>,
    /// The signing key's ID or fingerprint, from gpg's report.
    pub key_id: Option<String>,
    pub error: Option<String>,
}

fn signature_of(v: &Validity) -> Option<Signature> {
    if v.sign == SignStatus::None {
        return None;
    }
    Some(Signature {
        status: v.sign,
        signer: v.signers.first().cloned(),
        key_id: v.sign_description.as_deref().and_then(key_id_in),
        error: None,
    })
}

/// The key gpg names in its report ("using RSA key 0123…").
fn key_id_in(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_ascii_hexdigit()))
        .find(|w| w.len() >= 16 && w.chars().all(|c| c.is_ascii_hexdigit()))
        .map(str::to_string)
}

fn is(part: &MimePart, t: &str, s: &str) -> bool {
    part.content_type().is_some_and(|c| c.is(t, s))
}

/// The part to display in place of the message's content when it is
/// encrypted or signed (PGP/MIME, or inline PGP in a text/plain body), and
/// what the protection was. None when the message is not protected.
/// Runs gpg; blocks, and decrypting can ask for the passphrase.
pub fn unprotect_sync(ctx: &GpgContext, msg: &MimeMessage) -> Option<(MimePart, Protection)> {
    let top: &MimePart = msg.upcast_ref();
    // A protected part at the top, or as the first part of a
    // multipart/mixed that a mailing list wrapped with a footer.
    let candidate = if is(top, "multipart", "encrypted") || is(top, "multipart", "signed") {
        Some(top.clone())
    } else if is(top, "multipart", "mixed") {
        MediumExt::content(top)
            .and_then(|c| c.downcast::<Multipart>().ok())
            .and_then(|mp| mp.part(0))
            .filter(|p| is(p, "multipart", "encrypted") || is(p, "multipart", "signed"))
    } else if is(top, "text", "plain") || top.content_type().is_none() {
        return inline_sync(ctx, top);
    } else {
        None
    };
    let part = candidate?;
    let mut prot = Protection::default();
    if is(&part, "multipart", "encrypted") {
        prot.encrypted = true;
        match ctx.decrypt_sync(&part) {
            Ok((out, validity)) => {
                prot.signature = signature_of(&validity);
                // Signed, then encrypted: the signature is inside.
                if is(&out, "multipart", "signed") {
                    return Some(verified(ctx, out, prot));
                }
                Some((out, prot))
            }
            Err(e) => {
                prot.decrypt_error = Some(e.message().to_string());
                Some((MimePart::new(), prot))
            }
        }
    } else {
        Some(verified(ctx, part, prot))
    }
}

/// Checks a multipart/signed part; the content to show is its first part.
fn verified(ctx: &GpgContext, part: MimePart, mut prot: Protection) -> (MimePart, Protection) {
    let shown = MediumExt::content(&part)
        .and_then(|c| c.downcast::<Multipart>().ok())
        .and_then(|mp| mp.part(0))
        .unwrap_or_else(|| part.clone());
    prot.signature = Some(match ctx.verify_sync(&part) {
        Ok(v) => signature_of(&v).unwrap_or(Signature {
            status: SignStatus::Bad,
            signer: None,
            key_id: None,
            error: Some("No signature found".into()),
        }),
        Err(e) => Signature { status: SignStatus::Bad, signer: None, key_id: None, error: Some(e.message().to_string()) },
    });
    (shown, prot)
}

/// Inline PGP in a text/plain body: decrypted or verified, never sent.
fn inline_sync(ctx: &GpgContext, part: &MimePart) -> Option<(MimePart, Protection)> {
    let text = crate::render::plain_text(part);
    let encrypted = text.contains("-----BEGIN PGP MESSAGE-----");
    let signed = text.contains("-----BEGIN PGP SIGNED MESSAGE-----");
    if !encrypted && !signed {
        return None;
    }
    // Camel recognises inline PGP by these private content types.
    let wrapped = MimePart::new();
    let kind = if encrypted { "application/x-inlinepgp-encrypted" } else { "application/x-inlinepgp-signed" };
    MimePartExt::set_content(&wrapped, text.as_bytes(), Some(kind));
    let mut prot = Protection { encrypted, ..Default::default() };
    if encrypted {
        match ctx.decrypt_sync(&wrapped) {
            Ok((out, v)) => {
                prot.signature = signature_of(&v);
                Some((out, prot))
            }
            Err(e) => {
                prot.decrypt_error = Some(e.message().to_string());
                Some((MimePart::new(), prot))
            }
        }
    } else {
        let shown = MimePart::new();
        MimePartExt::set_content(&shown, strip_clearsign(&text).as_bytes(), Some("text/plain; charset=utf-8"));
        prot.signature = Some(match ctx.verify_sync(&wrapped) {
            Ok(v) => signature_of(&v)?,
            Err(e) => Signature { status: SignStatus::Bad, signer: None, key_id: None, error: Some(e.message().to_string()) },
        });
        Some((shown, prot))
    }
}

/// The text of a clearsigned block without its armor lines, signature and
/// dash-escaping.
fn strip_clearsign(text: &str) -> String {
    let mut out = Vec::new();
    let mut state = 0; // 0 before, 1 headers, 2 text, 3 signature
    for line in text.lines() {
        match state {
            0 if line.starts_with("-----BEGIN PGP SIGNED MESSAGE-----") => state = 1,
            0 => out.push(line.to_string()),
            1 if line.trim().is_empty() => state = 2,
            1 => {}
            2 if line.starts_with("-----BEGIN PGP SIGNATURE-----") => state = 3,
            2 => out.push(line.strip_prefix("- ").unwrap_or(line).to_string()),
            3 if line.starts_with("-----END PGP SIGNATURE-----") => state = 0,
            _ => {}
        }
    }
    out.join("\n")
}

// Sending

/// Signs and/or encrypts the message's content in place. `signer` is a
/// fingerprint; `encrypt_to` lists recipient addresses (or key
/// fingerprints), and the signer's key is added so the sender can read
/// Sent and Drafts. Runs gpg; blocks, and signing can ask for the
/// passphrase.
pub fn protect_sync(
    ctx: &GpgContext,
    msg: &MimeMessage,
    content: &MimePart,
    signer: Option<&str>,
    encrypt_to: Option<&[String]>,
) -> Result<(), glib::Error> {
    let mut part = content.clone();
    if let Some(s) = signer {
        part = ctx.sign_sync(s, &part)?;
    }
    if let Some(to) = encrypt_to {
        let mut recipients = resolve_keys_sync(ctx, to).map_err(|missing| {
            glib::Error::new(gio::IOErrorEnum::NotFound, &format!("No key for {}", missing.join(", ")))
        })?;
        if let Some(s) = signer {
            recipients.push(s.to_string());
        }
        part = ctx.encrypt_sync(&recipients, &part)?;
    }
    compose::set_body(msg, &part);
    Ok(())
}

/// The key each address encrypts to: the newest key in the keyring with
/// that address, so a key imported to replace an older one is used. The
/// addresses without one are the error. Runs gpg; blocks.
pub fn resolve_keys_sync(ctx: &GpgContext, addresses: &[String]) -> Result<Vec<String>, Vec<String>> {
    let mut found = Vec::new();
    let mut missing = Vec::new();
    for a in addresses {
        // A fingerprint or key ID names its key already.
        if !a.contains('@') {
            found.push(a.clone());
            continue;
        }
        let newest = ctx
            .public_key_info_sync(a)
            .unwrap_or_default()
            .into_iter()
            // Trust 2 is "never": the user disabled the key.
            .filter(|k| k.trust != 2 && key_has_address(k, a))
            .max_by_key(|k| k.created);
        match newest {
            Some(k) => found.push(k.fingerprint),
            None => missing.push(a.clone()),
        }
    }
    if missing.is_empty() { Ok(found) } else { Err(missing) }
}

/// Recipients without a usable key in the keyring. Runs gpg; blocks.
pub fn missing_keys_sync(ctx: &GpgContext, addresses: &[String]) -> Vec<String> {
    resolve_keys_sync(ctx, addresses).err().unwrap_or_default()
}

// Looking keys up

/// The Web Key Directory hash of a local part: z-base-32 of its SHA-1.
fn wkd_hash(local: &str) -> String {
    const ALPHABET: &[u8; 32] = b"ybndrfg8ejkmcpqxot1uwisza345h769";
    let mut sum = glib::Checksum::new(glib::ChecksumType::Sha1).expect("SHA-1 is available");
    sum.update(local.to_lowercase().as_bytes());
    let digest = sum.digest();
    let mut out = String::new();
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for byte in digest {
        buffer = (buffer << 8) | byte as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// The WKD addresses to try for `email`: the advanced method, then the
/// direct one.
pub fn wkd_urls(email: &str) -> Vec<String> {
    let Some((local, domain)) = email.rsplit_once('@') else { return Vec::new() };
    let domain = domain.to_ascii_lowercase();
    let hash = wkd_hash(local);
    let l = glib::Uri::escape_string(local, None, false);
    vec![
        format!("https://openpgpkey.{domain}/.well-known/openpgpkey/{domain}/hu/{hash}?l={l}"),
        format!("https://{domain}/.well-known/openpgpkey/hu/{hash}?l={l}"),
    ]
}

/// The HKP lookup address on `keyserver` ("hkps://host" or a URL) for
/// `email`.
pub fn keyserver_url(keyserver: &str, email: &str) -> String {
    let base = keyserver.trim().trim_end_matches('/');
    let base = base
        .strip_prefix("hkps://")
        .map(|h| format!("https://{h}"))
        .or_else(|| base.strip_prefix("hkp://").map(|h| format!("http://{h}:11371")))
        .unwrap_or_else(|| base.to_string());
    let q = glib::Uri::escape_string(email, None, false);
    format!("{base}/pks/lookup?op=get&options=mr&search={q}")
}

/// The public key for `email` from its WKD, else from `keyserver`; None
/// when neither has one.
pub async fn lookup(email: &str, keyserver: &str) -> Option<Vec<u8>> {
    for url in wkd_urls(email).into_iter().chain(std::iter::once(keyserver_url(keyserver, email))) {
        if let Ok(data) = crate::remote::fetch_bytes(&url, 512 * 1024).await
            && !data.is_empty()
        {
            return Some(data);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wkd() {
        // The example from the WKD draft.
        assert_eq!(wkd_hash("Joe.Doe"), "iy9q119eutrkn8s1mk4r39qejnbu3n5q");
        assert_eq!(
            wkd_urls("Joe.Doe@Example.ORG")[1],
            "https://example.org/.well-known/openpgpkey/hu/iy9q119eutrkn8s1mk4r39qejnbu3n5q?l=Joe.Doe"
        );
    }

    #[test]
    fn crc() {
        // RFC 4880's CRC-24 of no data is its initial value.
        assert_eq!(crc24(b""), 0xB704CE);
        assert!(armor_public_key(b"abc").contains("\nYWJj\n="));
    }

    #[test]
    fn autocrypt_round_trip() {
        let h = autocrypt_header("a@example.org", &[1, 2, 3, 250]);
        let k = parse_autocrypt(&h).expect("parses");
        assert_eq!(k.address, "a@example.org");
        assert_eq!(k.data, vec![1, 2, 3, 250]);
        assert!(parse_autocrypt("addr=a@b.c; type=2; keydata=AAAA").is_none());
        assert!(parse_autocrypt("addr=a@b.c; _x=1; keydata=AAAA").is_some());
    }

    #[test]
    fn secret_key_listing() {
        let listing = "sec:u:255:22:AAAA:1:::u:::scESC:::+:::23::0:\nfpr:::::::::FPR1:\ngrp:::::::::G:\nuid:u::::1::H::Alice \\x3a A <alice@example.org>::::::::::0:\nssb:u:255:18:B:1::::::e:::+:::23:\nfpr:::::::::SUBFPR:\nsec:e:255:22:CCCC:1:2::u:::scESC:::+:::23::0:\nfpr:::::::::FPR2:\nuid:e::::1::H::Old <old@example.org>::::::::::0:\n";
        let keys = parse_secret_keys(listing);
        assert_eq!(keys, vec![SecretKey { fingerprint: "FPR1".into(), user_id: "Alice : A <alice@example.org>".into() }]);
        assert_eq!(user_id_email(&keys[0].user_id).as_deref(), Some("alice@example.org"));
    }

    #[test]
    fn clearsign() {
        let t = "-----BEGIN PGP SIGNED MESSAGE-----\nHash: SHA256\n\nHello\n- -- dashed\n-----BEGIN PGP SIGNATURE-----\nabc\n-----END PGP SIGNATURE-----\n";
        assert_eq!(strip_clearsign(t), "Hello\n-- dashed");
    }

    #[test]
    fn key_ids() {
        assert_eq!(key_id_in("gpg: using EDDSA key 0123456789ABCDEF0123").as_deref(), Some("0123456789ABCDEF0123"));
    }
}
