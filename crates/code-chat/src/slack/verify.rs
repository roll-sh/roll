//! Request signing: Slack signs `v0:<X-Slack-Request-Timestamp>:<raw body>` with HMAC-SHA256
//! under the app's signing secret and sends `X-Slack-Signature: v0=<hex>`. A request more than
//! five minutes old is a replay, whatever its signature.

use crate::{MAX_SKEW_S, VerifyError};
use hmac::{Hmac, Mac};
use sha2::Sha256;

fn mac(signing_secret: &str, timestamp: &str, body: &[u8]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(signing_secret.as_bytes())
        .expect("hmac accepts any key length");
    mac.update(b"v0:");
    mac.update(timestamp.as_bytes());
    mac.update(b":");
    mac.update(body);
    mac
}

/// Check a request's `X-Slack-Request-Timestamp` and `X-Slack-Signature` against its raw body.
/// The comparison is constant-time.
pub fn verify(
    signing_secret: &str,
    timestamp: Option<&str>,
    signature: Option<&str>,
    body: &[u8],
    now_unix: i64,
) -> Result<(), VerifyError> {
    let (Some(timestamp), Some(signature)) = (timestamp, signature) else {
        return Err(VerifyError::Missing);
    };
    let timestamp = timestamp.trim();
    let ts: i64 = timestamp.parse().map_err(|_| VerifyError::Mismatch)?;
    if now_unix.abs_diff(ts) > MAX_SKEW_S as u64 {
        return Err(VerifyError::Stale);
    }
    let sig = signature
        .trim()
        .strip_prefix("v0=")
        .and_then(|h| hex::decode(h).ok())
        .ok_or(VerifyError::Mismatch)?;
    mac(signing_secret, timestamp, body)
        .verify_slice(&sig)
        .map_err(|_| VerifyError::Mismatch)
}

/// The `X-Slack-Signature` for `body` sent at `timestamp`: for tests and mock servers.
pub fn sign(signing_secret: &str, timestamp: i64, body: &[u8]) -> String {
    let tag = mac(signing_secret, &timestamp.to_string(), body).finalize();
    format!("v0={}", hex::encode(tag.into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // From Slack's "Verifying requests from Slack".
    const SECRET: &str = "8f742231b10e8888abcd99yyyzzz85a5";
    const TS: i64 = 1531420618;
    const BODY: &str = "token=xyzz0WbapA4vBCDEFasx0q6G&team_id=T1DC2JH3J&team_domain=testteamnow&channel_id=G8PSS9T3V&channel_name=foobar&user_id=U2CERLKJA&user_name=roadrunner&command=%2Fwebhook-collect&text=&response_url=https%3A%2F%2Fhooks.slack.com%2Fcommands%2FT1DC2JH3J%2F397700885554%2F96rGlfmibIGlgcZRskXaIFfN&trigger_id=398738663015.47445629121.803a0bc887a14d10d2c447fce8b6703c";
    const SIG: &str = "v0=a2114d57b48eac39b9ad189dd8316235a7b4a8d21a10bd27519666489c69b503";

    #[test]
    fn slack_known_answer() {
        assert_eq!(sign(SECRET, TS, BODY.as_bytes()), SIG);
        let ts = TS.to_string();
        assert_eq!(
            verify(SECRET, Some(&ts), Some(SIG), BODY.as_bytes(), TS + 10),
            Ok(())
        );
    }

    #[test]
    fn stale_requests_are_refused_either_way() {
        let ts = TS.to_string();
        let body = BODY.as_bytes();
        assert_eq!(verify(SECRET, Some(&ts), Some(SIG), body, TS + 300), Ok(()));
        assert_eq!(
            verify(SECRET, Some(&ts), Some(SIG), body, TS + 301),
            Err(VerifyError::Stale)
        );
        assert_eq!(
            verify(SECRET, Some(&ts), Some(SIG), body, TS - 301),
            Err(VerifyError::Stale)
        );
    }

    #[test]
    fn mismatches_and_missing_headers() {
        let ts = TS.to_string();
        let body = BODY.as_bytes();
        let now = TS;
        let tampered = format!("{BODY}&x=1");
        assert_eq!(
            verify(SECRET, Some(&ts), Some(SIG), tampered.as_bytes(), now),
            Err(VerifyError::Mismatch)
        );
        assert_eq!(
            verify("other", Some(&ts), Some(SIG), body, now),
            Err(VerifyError::Mismatch)
        );
        // The timestamp is signed: moving it breaks the signature.
        let later = (TS + 1).to_string();
        assert_eq!(
            verify(SECRET, Some(&later), Some(SIG), body, now),
            Err(VerifyError::Mismatch)
        );
        assert_eq!(
            verify(SECRET, Some(&ts), Some(&SIG[3..]), body, now),
            Err(VerifyError::Mismatch)
        );
        assert_eq!(
            verify(SECRET, Some(&ts), Some("v0=zz"), body, now),
            Err(VerifyError::Mismatch)
        );
        assert_eq!(
            verify(SECRET, Some("soon"), Some(SIG), body, now),
            Err(VerifyError::Mismatch)
        );
        assert_eq!(
            verify(SECRET, None, Some(SIG), body, now),
            Err(VerifyError::Missing)
        );
        assert_eq!(
            verify(SECRET, Some(&ts), None, body, now),
            Err(VerifyError::Missing)
        );
        assert!(!VerifyError::Mismatch.to_string().contains(SECRET));
    }
}
