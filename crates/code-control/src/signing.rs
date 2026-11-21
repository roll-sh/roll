//! Repo webhooks' and review apps' signing secrets at rest (docs/code/http-api.md, "Review
//! apps"). With `ROLL_SECRET_KEY` they're sealed by the sealer that keeps owners' own provider
//! keys, bound to their row's id (a sealed secret copied to another row doesn't open), and
//! secrets a server kept in the clear before it had the key are sealed when it opens
//! ([`seal_at_rest`]). Without the key they stay in the clear, as they always were. Either way the
//! secret is shown once, when it's made, and only the delivery worker opens it, to sign.

use crate::Result;
use crate::db::Db;
use code_review::secret::Sealer;
use code_sql::params;

/// A signing secret as its row keeps it: the `secret` column, and `secret_sealed`.
#[derive(Clone, PartialEq, Eq)]
pub enum Kept {
    /// Sealed to the row ([`scope`]).
    Sealed(String),
    /// In the clear: made on a server without `ROLL_SECRET_KEY`.
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

/// What a row's secret is sealed to.
fn scope(row_id: &str) -> String {
    format!("signing:{row_id}")
}

impl Kept {
    pub(crate) fn from_columns(text: String, sealed: i64) -> Kept {
        if sealed != 0 { Kept::Sealed(text) } else { Kept::Clear(text) }
    }

    /// The `secret` column and `secret_sealed`.
    pub(crate) fn columns(&self) -> (&str, i64) {
        match self {
            Kept::Sealed(s) => (s, 1),
            Kept::Clear(s) => (s, 0),
        }
    }

    /// `secret` as row `row_id` keeps it: sealed when the server has a key.
    pub(crate) fn keep(sealer: Option<&Sealer>, row_id: &str, secret: &str) -> Kept {
        match sealer {
            Some(s) => Kept::Sealed(s.seal(&scope(row_id), secret)),
            None => Kept::Clear(secret.to_string()),
        }
    }

    /// The secret, to sign with.
    pub(crate) fn open(&self, sealer: Option<&Sealer>, row_id: &str) -> std::result::Result<String, String> {
        match self {
            Kept::Clear(s) => Ok(s.clone()),
            Kept::Sealed(s) => {
                let sealer = sealer.ok_or("its signing secret is sealed and ROLL_SECRET_KEY isn't set on the server any more")?;
                sealer.open(&scope(row_id), s).map_err(|e| format!("its signing secret: {e}"))
            }
        }
    }
}

/// Tables whose `secret` is a [`Kept`] signing secret.
const TABLES: [&str; 2] = ["repo_webhooks", "review_apps"];

/// Seal every signing secret kept in the clear, once the server has a key. Every node may run
/// this at once: a row is replaced only if it's still the clear secret that was read (another
/// node, or a new secret, may have got there first), and sealed rows are never touched again.
/// Returns how many it sealed.
pub(crate) fn seal_at_rest(db: &Db, sealer: Option<&Sealer>) -> Result<usize> {
    let Some(sealer) = sealer else { return Ok(0) };
    let mut n = 0;
    for table in TABLES {
        let clear: Vec<(String, String)> =
            db.sql().query(&format!("SELECT id, secret FROM {table} WHERE secret_sealed = 0"), (), |r| Ok((r.get(0)?, r.get(1)?)))?;
        for (id, secret) in clear {
            let Kept::Sealed(sealed) = Kept::keep(Some(sealer), &id, &secret) else { unreachable!("a sealer seals") };
            n += db.sql().execute(
                &format!("UPDATE {table} SET secret = ?, secret_sealed = 1 WHERE id = ? AND secret_sealed = 0 AND secret = ?"),
                params![sealed, id, secret],
            )?;
        }
    }
    if n > 0 {
        tracing::info!(secrets = n, "sealed webhooks' and review apps' signing secrets kept in the clear");
    }
    Ok(n)
}

