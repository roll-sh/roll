//! OpenPGP round trips and messages from pgp-decrypt.sql (made by GnuPG).

use super::options::{Expect, parse_args};
use super::{DebugGuard, Pgp, armor, decrypt, encrypt};

fn sym_ctx(args: &str, text: bool) -> Pgp {
    let mut ctx = Pgp::default();
    parse_args(&mut ctx, args.as_bytes(), &mut Expect::default()).unwrap();
    ctx.text_mode = text;
    ctx.sym_key = Some(b"foobar".to_vec());
    ctx
}

#[test]
fn gnupg_messages() {
    let msg = "\n-----BEGIN PGP MESSAGE-----\nComment: dat1.aes.sha1.mdc.s2k3.z0\n\njA0EBwMCci97v0Q6Z0Zg0kQBsVf5Oe3iC+FBzUmuMV9KxmAyOMyjCc/5i8f1Eest\nUTAsG35A1vYs02VARKzGz6xI2UHwFUirP+brPBg3Ee7muOx8pA==\n=XtrP\n-----END PGP MESSAGE-----\n";
    let data = armor::decode(msg.as_bytes()).unwrap();
    let out = decrypt::decrypt(&mut sym_ctx("", true), &data).unwrap();
    assert_eq!(out, b"Secret message.");
    // Blowfish is refused, as without OpenSSL's legacy provider.
    let msg = "\n-----BEGIN PGP MESSAGE-----\nComment: dat1.blowfish.sha1.mdc.s2k3.z0\n\njA0EBAMCfFNwxnvodX9g0jwB4n4s26/g5VmKzVab1bX1SmwY7gvgvlWdF3jKisvS\nyA6Ce1QTMK3KdL2MPfamsTUSAML8huCJMwYQFfE=\n=JcP+\n-----END PGP MESSAGE-----\n";
    let data = armor::decode(msg.as_bytes()).unwrap();
    let e = decrypt::decrypt(&mut sym_ctx("", true), &data).unwrap_err();
    assert!(format!("{e:?}").contains("Cipher cannot be initialized"), "{e:?}");
}

#[test]
fn round_trips() {
    let text = "Secret message.\nWith two lines\r\n".repeat(2000);
    for args in [
        "",
        "cipher-algo=aes256, s2k-mode=1",
        "cipher-algo=3des, disable-mdc=1",
        "compress-algo=1, compress-level=9",
        "compress-algo=2, sess-key=1, s2k-digest-algo=md5",
        "s2k-mode=0, cipher-algo=aes192, s2k-cipher-algo=aes128, sess-key=1",
        "convert-crlf=1, unicode-mode=1",
    ] {
        let mut e = sym_ctx(args, true);
        let enc = encrypt::encrypt(&mut e, text.as_bytes()).unwrap();
        let mut d = sym_ctx(if args.contains("crlf") { "convert-crlf=1" } else { "" }, true);
        let out = decrypt::decrypt(&mut d, &enc).unwrap_or_else(|e| panic!("{args}: {e:?}"));
        // (LF to CRLF and back again gives the text as it was.)
        assert!(out == text.as_bytes(), "{args}");
        assert_eq!(armor::decode(&armor::encode(&enc, &[])).unwrap(), enc);
    }
    // Binary data decrypted as text: reported after the data is read.
    let mut e = sym_ctx("", false);
    let enc = encrypt::encrypt(&mut e, b"bin").unwrap();
    let dbg = DebugGuard::enter(true);
    assert!(decrypt::decrypt(&mut sym_ctx("", true), &enc).is_err());
    assert_eq!(dbg.take(), vec!["parse_literal_data: data type=b".to_string()]);
}
