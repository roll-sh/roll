//! Repo webhooks' and review apps' credentials at rest (docs/code/http-api.md, "Review apps"):
//! their URLs (a Slack or Discord incoming-webhook URL is enough to post) and their signing
//! secrets. Each is sealed with `ROLL_SECRET_KEY`, by the sealer that keeps owners' own provider
//! keys, and bound to its row's id and field (a sealed value copied to another row or column
//! doesn't open). Without the key no webhook or review app is made or changed ([`require`]), so
//! nothing new is kept in the clear. Rows a server kept in the clear before it had the key are
//! sealed when it opens ([`seal_at_rest`]), and what they overwrote is erased from the SQLite file.
//! The secret is shown once, when it's made; answers show a URL's scheme and host only
//! ([`origin`]); only the delivery worker opens either, to send.

use crate::db::Db;
use crate::{Error, Result};
use code_review::secret::Sealer;
use code_sql::params;
use serde_json::Value;

/// A credential as its row keeps it: the column, and its `…_sealed` flag.
#[derive(Clone, PartialEq, Eq)]
pub enum Kept {
    /// Sealed to the row and field ([`Field::scope`]).
    Sealed(String),
    /// In the clear: kept by a server without `ROLL_SECRET_KEY`, and not sealed yet.
    Clear(String),
}

impl std::fmt::Debug for Kept {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Kept::Sealed(_) => "Sealed(<redacted>)",
            Kept::Clear(_) => "Clear(<redacted>)",
        })
    }
}

/// Which credential of a row a [`Kept`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    /// The signing secret (`secret`).
    Secret,
    /// Where deliveries go (`url`, `webhook_url`).
    Url,
}

impl Field {
    /// What a row's value is sealed to.
    fn scope(self, row_id: &str) -> String {
        match self {
            Field::Secret => format!("signing:{row_id}"),
            Field::Url => format!("url:{row_id}"),
        }
    }

    fn what(self) -> &'static str {
        match self {
            Field::Secret => "signing secret",
            Field::Url => "URL",
        }
    }
}

/// The sealer, or the 400 that makes or changes nothing: `what` ("webhooks", "review apps")
/// keep their URLs and secrets only sealed.
pub(crate) fn require<'a>(sealer: Option<&'a Sealer>, what: &str) -> Result<&'a Sealer> {
    sealer.ok_or_else(|| {
        Error::Invalid(format!(
            "{what} need ROLL_SECRET_KEY (at least 32 characters) on the server: their URLs and signing secrets are kept sealed with it, never in the clear"
        ))
    })
}

impl Kept {
    pub(crate) fn from_columns(text: String, sealed: i64) -> Kept {
        if sealed != 0 { Kept::Sealed(text) } else { Kept::Clear(text) }
    }

    /// The column and its `…_sealed` flag.
    pub(crate) fn columns(&self) -> (&str, i64) {
        match self {
            Kept::Sealed(s) => (s, 1),
            Kept::Clear(s) => (s, 0),
        }
    }

    /// `value` as row `row_id` keeps it in `field`.
    pub(crate) fn seal(sealer: &Sealer, field: Field, row_id: &str, value: &str) -> Kept {
        Kept::Sealed(sealer.seal(&field.scope(row_id), value))
    }

    /// The value, to send with. Errors never carry it.
    pub(crate) fn open(&self, sealer: Option<&Sealer>, field: Field, row_id: &str) -> std::result::Result<String, String> {
        match self {
            Kept::Clear(s) => Ok(s.clone()),
            Kept::Sealed(s) => {
                let what = field.what();
                let sealer = sealer.ok_or_else(|| format!("its {what} is sealed and ROLL_SECRET_KEY isn't set on the server any more"))?;
                sealer.open(&field.scope(row_id), s).map_err(|e| format!("its {what}: {e}"))
            }
        }
    }

    /// What answers show of a kept URL: its [`origin`], or `(sealed)` when this server can't open it.
    pub(crate) fn shown_url(&self, sealer: Option<&Sealer>, row_id: &str) -> String {
        self.open(sealer, Field::Url, row_id).map(|u| origin(&u)).unwrap_or_else(|_| "(sealed)".into())
    }
}

/// A URL's scheme, host and port (`https://hooks.example.com`): what answers, audit rows and
/// pings show. The path and query (where incoming-webhook URLs keep their token) never.
pub fn origin(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(u) => match u.host_str() {
            Some(_) => u.origin().ascii_serialization(),
            None => format!("{}:", u.scheme()),
        },
        Err(_) => "(not a URL)".into(),
    }
}

/// Each table and its URL column.
const TABLES: [(&str, &str); 2] = [("repo_webhooks", "url"), ("review_apps", "webhook_url")];

/// A row kept (partly) in the clear: its id, URL, secret and their flags.
type ClearRow = (String, String, i64, String, i64);

/// Seal every URL and signing secret kept in the clear, once the server has a key, and cut the
/// URLs a server wrote whole elsewhere before this down to their [`origin`] ([`Mentions`]). It's
/// one write that erases what it overwrote ([`code_sql::Db::write_erasing`]): on SQLite nothing of
/// the old values stays in the file's free pages or its `-wal`; on Postgres the old row versions
/// stay as dead tuples until autovacuum reuses their pages, and backups and WAL archives taken
/// before still hold them. Every node may run this at once: a row is replaced only if it's still
/// what was read, and sealed values are never touched again. Returns how many rows it rewrote.
pub(crate) fn seal_at_rest(db: &Db, sealer: Option<&Sealer>) -> Result<usize> {
    let Some(sealer) = sealer else { return Ok(0) };
    let clear_rows = |c: &code_sql::Conn<'_>| -> code_sql::Result<bool> {
        for (table, _) in TABLES {
            let n: i64 = c.query_row(&format!("SELECT COUNT(*) FROM {table} WHERE url_sealed = 0 OR secret_sealed = 0"), (), |r| r.get(0))?;
            if n > 0 {
                return Ok(true);
            }
        }
        Ok(false)
    };
    // Most opens have nothing to do: look before taking the writer.
    let todo = db.sql().read(|c| Ok::<_, code_sql::Error>(clear_rows(c)? || !Mentions::read(c, false)?.is_empty()))?;
    if !todo {
        return Ok(0);
    }
    let n = db.sql().write_erasing(|c| {
        let mut n = 0;
        // Rows in the clear are from before this release: their deliveries may name them too.
        let deliveries = clear_rows(c)?;
        for (table, url_col) in TABLES {
            let clear: Vec<ClearRow> = c.query(
                &format!("SELECT id, {url_col}, url_sealed, secret, secret_sealed FROM {table} WHERE url_sealed = 0 OR secret_sealed = 0"),
                (),
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )?;
            for (id, url, url_sealed, secret, secret_sealed) in clear {
                let sealed = |field: Field, value: &str, flag: i64| match flag {
                    0 => Kept::seal(sealer, field, &id, value),
                    _ => Kept::Sealed(value.to_string()),
                };
                let (u, s) = (sealed(Field::Url, &url, url_sealed), sealed(Field::Secret, &secret, secret_sealed));
                n += c.execute(
                    &format!(
                        "UPDATE {table} SET {url_col} = ?, url_sealed = 1, secret = ?, secret_sealed = 1 \
                         WHERE id = ? AND {url_col} = ? AND url_sealed = ? AND secret = ? AND secret_sealed = ?"
                    ),
                    params![u.columns().0, s.columns().0, id, url, url_sealed, secret, secret_sealed],
                )?;
            }
        }
        let m = Mentions::read(c, deliveries)?;
        for (id, detail) in m.audit {
            n += c.execute("UPDATE audit SET detail = ? WHERE id = ?", params![detail, id])?;
        }
        for (id, payload, last_error) in m.deliveries {
            n += c.execute("UPDATE review_deliveries SET payload = ?, last_error = ? WHERE id = ?", params![payload, last_error, id])?;
        }
        Ok::<_, Error>(n)
    })?;
    if n > 0 {
        tracing::info!(rows = n, "sealed webhooks' and review apps' URLs and signing secrets kept in the clear");
    }
    Ok(n)
}

/// Delivery URLs a server wrote whole into other rows before it wrote only their [`origin`]:
/// `webhook_create` audit rows (`detail.url`), pings (`payload.data.webhook.url`) and failed attempts
/// (reqwest's `… for url (<url>)` in `last_error`). Each row with the text it gets instead.
#[derive(Default)]
struct Mentions {
    audit: Vec<(i64, String)>,
    /// id, payload, last_error.
    deliveries: Vec<(String, String, Option<String>)>,
}

impl Mentions {
    fn is_empty(&self) -> bool {
        self.audit.is_empty() && self.deliveries.is_empty()
    }

    /// Audit rows always (there are few); deliveries only with `deliveries` (once: none are
    /// written whole after this release).
    fn read(c: &code_sql::Conn<'_>, deliveries: bool) -> code_sql::Result<Mentions> {
        let mut m = Mentions::default();
        for (id, detail) in c.query("SELECT id, detail FROM audit WHERE action = 'webhook_create'", (), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
            if let Some(cut) = cut_json(&detail, &["url"]) {
                m.audit.push((id, cut));
            }
        }
        if deliveries {
            let rows = c.query(
                "SELECT id, payload, last_error FROM review_deliveries WHERE (event = 'ping' AND substr(app_id, 1, 3) = 'wh_') OR last_error LIKE '% for url (%'",
                (),
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?)),
            )?;
            for (id, payload, last_error) in rows {
                let p = cut_json(&payload, &["data", "webhook", "url"]);
                let e = last_error.as_deref().and_then(cut_reqwest);
                if p.is_some() || e.is_some() {
                    m.deliveries.push((id, p.unwrap_or(payload), e.or(last_error)));
                }
            }
        }
        Ok(m)
    }
}

/// `text` (JSON) with the URL at `path` cut to its origin, if it was more.
fn cut_json(text: &str, path: &[&str]) -> Option<String> {
    let mut v: Value = serde_json::from_str(text).ok()?;
    let Some(Value::String(url)) = path.iter().try_fold(&mut v, |v, k| v.get_mut(*k)) else { return None };
    let shown = origin(url);
    if *url == shown {
        return None;
    }
    *url = shown;
    Some(v.to_string())
}

/// A reqwest error's text (`… for url (<url>)`) with the URL cut to its origin, if it was more.
fn cut_reqwest(text: &str) -> Option<String> {
    const MARK: &str = " for url (";
    let at = text.find(MARK)?;
    let url = text[at + MARK.len()..].strip_suffix(')')?;
    let shown = origin(url);
    (url != shown).then(|| format!("{}{MARK}{shown})", &text[..at]))
}
