//! PGP/MIME round trips against a throwaway GnuPG home. Needs gpg; run with
//! `cargo test -p salutations-core --test gpg -- --ignored`.

use salutations_core::camel::{self, prelude::*};
use salutations_core::compose::{self, Format, Outgoing};
use salutations_core::crypto::{self, SignStatus};
use salutations_core::Address;

fn gpg(home: &std::path::Path, args: &[&str]) {
    let ok = std::process::Command::new("gpg")
        .env("GNUPGHOME", home)
        .args(["--batch", "--pinentry-mode", "loopback", "--passphrase", ""])
        .args(args)
        .status()
        .expect("gpg runs")
        .success();
    assert!(ok, "gpg {args:?}");
}

#[test]
#[ignore]
fn sign_encrypt_round_trip() {
    let home = std::env::temp_dir().join(format!("salutations-gpg-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::set_permissions(&home, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    // Camel's gpg context runs gpg with the environment's GNUPGHOME.
    unsafe { std::env::set_var("GNUPGHOME", &home) };
    gpg(&home, &["--quick-gen-key", "Alice <alice@example.org>", "ed25519", "sign", "never"]);
    let keys = crypto::secret_keys_sync();
    let fpr = keys.first().expect("a key").fingerprint.clone();
    gpg(&home, &["--quick-add-key", &fpr, "cv25519", "encr", "never"]);
    gpg(&home, &["--quick-gen-key", "Bob <bob@example.org>", "default", "default", "never"]);

    camel::init(&home, false).unwrap();
    let ctx = crypto::context_with(None);
    assert!(crypto::missing_keys_sync(&ctx, &["bob@example.org".into(), "carol@example.org".into()]) == vec!["carol@example.org".to_string()]);

    let out = Outgoing {
        from: Address { name: Some("Alice".into()), email: "alice@example.org".into() },
        to: "bob@example.org".into(),
        subject: "Secret".into(),
        body: "Hello *Bob*".into(),
        format: Format::Markdown,
        message_id: compose::new_message_id("alice@example.org"),
        ..Default::default()
    };
    let msg = out.headers(false).unwrap();
    crypto::protect_sync(&ctx, &msg, &out.content_part(), Some(&fpr), Some(&["bob@example.org".to_string()])).unwrap();
    let ct = msg.content_type().unwrap().simple().unwrap();
    assert_eq!(ct.as_str(), "multipart/encrypted");

    // Through bytes, as the recipient would get it.
    let stream = gio::MemoryOutputStream::new_resizable();
    msg.write_to_output_stream_sync(&stream, None::<&gio::Cancellable>).unwrap();
    stream.close(None::<&gio::Cancellable>).unwrap();
    let bytes = stream.steal_as_bytes();
    let parsed = camel::MimeMessage::new();
    parsed
        .construct_from_input_stream_sync(&gio::MemoryInputStream::from_bytes(&bytes), None::<&gio::Cancellable>)
        .unwrap();
    let (shown, prot) = crypto::unprotect_sync(&ctx, &parsed).expect("protected");
    assert!(prot.encrypted, "{prot:?}");
    let sig = prot.signature.expect("signed");
    assert!(matches!(sig.status, SignStatus::Good | SignStatus::Unknown), "{sig:?}");
    assert_eq!(sig.signer.and_then(|s| s.email).as_deref(), Some("alice@example.org"));
    let text = salutations_core::render::reply_text_of(&shown);
    assert!(text.contains("Hello *Bob*"), "{text}");

    let msg = out.headers(false).unwrap();
    crypto::protect_sync(&ctx, &msg, &out.content_part(), Some(&fpr), None).unwrap();
    let (_, prot) = crypto::unprotect_sync(&ctx, &msg).expect("signed");
    assert!(!prot.encrypted);
    assert!(matches!(prot.signature.unwrap().status, SignStatus::Good | SignStatus::Unknown));

    let key = ctx.public_key_sync(&fpr).unwrap();
    let info = ctx.key_data_info_sync(&key).unwrap();
    assert_eq!(info[0].fingerprint, fpr);
    let _ = std::process::Command::new("gpgconf").env("GNUPGHOME", &home).args(["--kill", "all"]).status();
    let _ = std::fs::remove_dir_all(&home);
}
