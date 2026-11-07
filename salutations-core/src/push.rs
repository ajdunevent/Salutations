//! Whether an IMAP server can push new mail (IDLE or NOTIFY), asked with a
//! connection of its own before login. Camel keeps its server
//! capabilities inside the IMAPX provider, out of reach of applications.

use gio::prelude::*;

/// Some(true) or Some(false) from the server's CAPABILITY answer; None if
/// it could not be asked. Servers usually list IDLE before login.
pub async fn supported(host: &str, port: u16, tls: Option<bool>) -> Option<bool> {
    let client = gio::SocketClient::new();
    client.set_timeout(10);
    client.set_tls(tls == Some(true));
    let conn = client.connect_to_host_future(host, port).await.ok()?;
    let (mut input, mut output): (gio::InputStream, gio::OutputStream) =
        (conn.input_stream(), conn.output_stream());
    let mut reader = gio::DataInputStream::new(&input);
    let greeting = line(&reader).await?;
    let caps = bracket_caps(&greeting);
    if tls == Some(false) {
        send(&output, "s1 STARTTLS").await?;
        tagged(&reader, "s1").await?;
        let identity = gio::NetworkAddress::new(host, port);
        let tls_conn = gio::TlsClientConnection::new(&conn, Some(&identity)).ok()?;
        tls_conn.handshake_future(glib::Priority::DEFAULT).await.ok()?;
        input = tls_conn.input_stream();
        output = tls_conn.output_stream();
        reader = gio::DataInputStream::new(&input);
        // Capabilities can change once the connection is encrypted, so
        // the greeting's list is not used.
        let result = ask(&reader, &output).await;
        let _ = send(&output, "s3 LOGOUT").await;
        let _ = tls_conn.close_future(glib::Priority::DEFAULT).await;
        return result;
    }
    let result = match caps {
        Some(c) => Some(has_push(&c)),
        None => ask(&reader, &output).await,
    };
    let _ = send(&output, "s3 LOGOUT").await;
    let _ = conn.close_future(glib::Priority::DEFAULT).await;
    result
}

async fn ask(reader: &gio::DataInputStream, output: &gio::OutputStream) -> Option<bool> {
    send(output, "s2 CAPABILITY").await?;
    let mut caps = String::new();
    loop {
        let l = line(reader).await?;
        if let Some(rest) = l.strip_prefix("* CAPABILITY ") {
            caps.push_str(rest);
            caps.push(' ');
        }
        if l.starts_with("s2 ") {
            return Some(has_push(&caps));
        }
    }
}

fn has_push(caps: &str) -> bool {
    caps.split_whitespace().any(|c| c.eq_ignore_ascii_case("IDLE") || c.eq_ignore_ascii_case("NOTIFY"))
}

/// The capability list a greeting carries as `[CAPABILITY ...]`.
fn bracket_caps(greeting: &str) -> Option<String> {
    let start = greeting.find("[CAPABILITY ")? + "[CAPABILITY ".len();
    let end = greeting[start..].find(']')? + start;
    Some(greeting[start..end].to_string())
}

async fn line(reader: &gio::DataInputStream) -> Option<String> {
    let bytes = reader.read_line_future(glib::Priority::DEFAULT).await.ok()??;
    if bytes.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).trim_end().to_string())
}

async fn tagged(reader: &gio::DataInputStream, tag: &str) -> Option<()> {
    loop {
        let l = line(reader).await?;
        if let Some(rest) = l.strip_prefix(tag) {
            return rest.trim_start().starts_with("OK").then_some(());
        }
    }
}

async fn send(output: &gio::OutputStream, command: &str) -> Option<()> {
    output
        .write_all_future(format!("{command}\r\n").into_bytes(), glib::Priority::DEFAULT)
        .await
        .ok()?;
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities() {
        assert!(has_push("IMAP4rev1 SASL-IR IDLE LITERAL+"));
        assert!(!has_push("IMAP4rev1 SASL-IR LITERAL+"));
        assert_eq!(
            bracket_caps("* OK [CAPABILITY IMAP4rev1 IDLE] Dovecot ready.").as_deref(),
            Some("IMAP4rev1 IDLE")
        );
        assert_eq!(bracket_caps("* OK Gimap ready"), None);
    }
}
