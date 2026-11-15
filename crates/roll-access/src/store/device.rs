//! OAuth device authorization (RFC 8628), as `gh auth login` signs in: a device with no browser
//! (a terminal over SSH, a container) asks for a pair of codes, shows the short one, and polls
//! while a person enters it on roll in any browser and chooses what the device may do. The
//! device code is a secret (stored as its hash); the user code is short, so it lives ten minutes,
//! is used once, and wrong ones are counted per caller (`Access::device_guesses`).

use super::*;

/// How long a device request waits for its person.
pub const DEVICE_TTL_MS: i64 = 10 * 60 * 1000;
/// Live device requests at most: the endpoint is open, so the table is bounded.
pub const MAX_DEVICE_REQUESTS: usize = 10_000;
/// RFC 8628 6.1's user code alphabet: no vowels (no words), no digits a person misreads.
const ALPHABET: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ";
/// Letters in a user code: 20^8, about 34 bits.
const USER_CODE_LEN: usize = 8;

/// A device request as the person entering its code sees it.
#[derive(Clone, Debug, Serialize)]
pub struct DeviceRequest {
    pub user_code: String,
    pub client_id: String,
    pub scope: Option<String>,
    /// The network (/24, /48) it was asked from, so a person can tell it isn't theirs.
    pub network: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
}

/// A user code as typed (any case, with or without its dash or spaces), or `None`.
pub fn normalize_user_code(typed: &str) -> Option<String> {
    let code: String = typed.chars().filter(|c| !matches!(c, '-' | ' ')).map(|c| c.to_ascii_uppercase()).collect();
    (code.len() == USER_CODE_LEN && code.bytes().all(|b| ALPHABET.contains(&b))).then_some(code)
}

/// A user code as shown: `BCDF-GHJK`.
pub fn show_user_code(code: &str) -> String {
    format!("{}-{}", &code[..USER_CODE_LEN / 2], &code[USER_CODE_LEN / 2..])
}

fn new_user_code() -> String {
    let mut code = String::with_capacity(USER_CODE_LEN);
    while code.len() < USER_CODE_LEN {
        let mut b = [0u8; 16];
        getrandom::fill(&mut b).expect("the system's random source");
        // Bytes below 240 (12 x 20) only: every letter equally likely.
        code.extend(b.iter().filter(|x| **x < 240).map(|x| ALPHABET[*x as usize % ALPHABET.len()] as char).take(USER_CODE_LEN - code.len()));
    }
    code
}

/// A device request as its poll reads it.
struct Polled {
    client_id: String,
    expires_at: i64,
    polled_at: Option<i64>,
    grant_id: Option<String>,
    denied: bool,
}

impl Store {
    /// A new device request: its device code (the device's secret) and user code.
    pub fn start_device(&self, client_id: &str, scope: Option<&str>, network: Option<&str>) -> Result<(String, String)> {
        let device_code = new_secret("rld_");
        let h = hash(&device_code);
        let now = code_sql::now_ms();
        for _ in 0..5 {
            let user_code = new_user_code();
            let made = self.db.write(|c| {
                c.execute("DELETE FROM oauth_devices WHERE expires_at <= ?", [now])?;
                let live: i64 = c.query_row("SELECT COUNT(*) FROM oauth_devices", (), |r| r.get(0))?;
                if live as usize >= MAX_DEVICE_REQUESTS {
                    return Err(Error::Conflict("too many devices are signing in: try again in a few minutes".into()));
                }
                let n = c.execute(
                    "INSERT INTO oauth_devices (hash, user_code, client_id, scope, network, created_at, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT (user_code) DO NOTHING",
                    params![h, user_code, client_id, scope, network, now, now + DEVICE_TTL_MS],
                )?;
                Ok::<_, Error>(n == 1)
            })?;
            if made {
                return Ok((device_code, user_code));
            }
        }
        Err(Error::Internal("no free user code".into()))
    }

    /// A live request no one has decided, by its user code (normalized).
    pub fn device_request(&self, user_code: &str) -> Result<Option<DeviceRequest>> {
        let now = code_sql::now_ms();
        let found: Option<DeviceRequest> = self.db.read(|c| {
            c.query_row(
                "SELECT user_code, client_id, scope, network, created_at, expires_at FROM oauth_devices WHERE user_code = ? AND expires_at > ? AND grant_id IS NULL AND denied = 0",
                params![user_code, now],
                |r| Ok(DeviceRequest { user_code: r.get(0)?, client_id: r.get(1)?, scope: r.get(2)?, network: r.get(3)?, created_at: r.get(4)?, expires_at: r.get(5)? }),
            )
            .optional()
        })?;
        Ok(found)
    }

    /// A person allows a device request: its grant is made now (as a consent's is), and the
    /// device's next poll takes its tokens. Answers the grant's id.
    pub fn approve_device(&self, user_code: &str, client: &Client, new: &NewGrant, by: Issuer<'_>) -> Result<String> {
        check_oauth_grant(new)?;
        let grant_id = new_id("tok");
        let now = code_sql::now_ms();
        self.db.write(|c| {
            let client_id: Option<String> = c
                .query_row("SELECT client_id FROM oauth_devices WHERE user_code = ? AND expires_at > ? AND grant_id IS NULL AND denied = 0", params![user_code, now], |r| r.get(0))
                .optional()?;
            match client_id {
                Some(id) if id == client.id => {}
                Some(_) => return Err(Error::BadRequest("the device asked as another client".into())),
                None => return Err(Error::NotFound("no device waits for that code: it expired or was used".into())),
            }
            insert_oauth_grant(c, &grant_id, client, new, by, now)?;
            c.execute("UPDATE oauth_devices SET grant_id = ? WHERE user_code = ?", params![grant_id, user_code])?;
            Ok::<_, Error>(())
        })?;
        Ok(grant_id)
    }

    /// A person refuses a device request: its next poll answers `access_denied`.
    pub fn deny_device(&self, user_code: &str) -> Result<()> {
        let now = code_sql::now_ms();
        let n = self.db.write(|c| c.execute("UPDATE oauth_devices SET denied = 1 WHERE user_code = ? AND expires_at > ? AND grant_id IS NULL AND denied = 0", params![user_code, now]))?;
        if n == 0 {
            return Err(Error::NotFound("no device waits for that code: it expired or was used".into()));
        }
        Ok(())
    }

    /// The device's poll (RFC 8628 3.4): its tokens once a person allowed it (once: the request
    /// is gone after), else why not yet. Polling faster than `interval_ms` answers `slow_down`.
    pub fn poll_device(&self, device_code: &str, client_id: &str, interval_ms: i64) -> Result<(Grant, String, String)> {
        enum Poll {
            Ready(String),
            Wait(&'static str, &'static str),
        }
        let h = hash(device_code);
        let now = code_sql::now_ms();
        let (access, refresh) = (new_secret(ACCESS_PREFIX), new_secret(REFRESH_PREFIX));
        let outcome = self.db.write(|c| {
            let row = c
                .query_row("SELECT client_id, expires_at, polled_at, grant_id, denied FROM oauth_devices WHERE hash = ?", [&h], |r| {
                    Ok(Polled { client_id: r.get(0)?, expires_at: r.get(1)?, polled_at: r.get(2)?, grant_id: r.get(3)?, denied: r.get(4)? })
                })
                .optional()?;
            let Some(Polled { client_id: cid, expires_at, polled_at, grant_id, denied }) = row else { return Err(Error::OAuth("invalid_grant", "unknown or used device_code".into())) };
            if cid != client_id {
                return Err(Error::OAuth("invalid_grant", "the device_code was issued to another client".into()));
            }
            if expires_at <= now {
                c.execute("DELETE FROM oauth_devices WHERE hash = ?", [&h])?;
                return Ok(Poll::Wait("expired_token", "no one entered the code in time: start again"));
            }
            if denied {
                c.execute("DELETE FROM oauth_devices WHERE hash = ?", [&h])?;
                return Ok(Poll::Wait("access_denied", "the person refused it"));
            }
            if let Some(grant_id) = grant_id {
                c.execute("DELETE FROM oauth_devices WHERE hash = ?", [&h])?;
                insert_oauth_secrets(c, &grant_id, client_id, &access, &refresh, now)?;
                return Ok(Poll::Ready(grant_id));
            }
            c.execute("UPDATE oauth_devices SET polled_at = ? WHERE hash = ?", params![now, h])?;
            if polled_at.is_some_and(|p| now - p < interval_ms) {
                return Ok(Poll::Wait("slow_down", "polling faster than the interval: wait 5 seconds more between polls"));
            }
            Ok(Poll::Wait("authorization_pending", "the person hasn't decided yet"))
        })?;
        match outcome {
            Poll::Ready(grant_id) => Ok((self.get(&grant_id)?, access, refresh)),
            Poll::Wait(code, why) => Err(Error::OAuth(code, why.into())),
        }
    }
}
