//! The rows a backup carries besides keys (docs/secrets.md, "Backup and restore"): the
//! registrations links name, with their keys and credentials opened from the project's data key
//! (only the archive holds them, sealed to the recipient as a whole), and the project's identities
//! and syncs. One table each, read and written column by column: a column that isn't listed never
//! leaves, and a restore writes only listed columns.

use crate::crypto::Dek;
use crate::{Error, Result, Value};
use code_sql::{Conn, Value as Cell};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use std::collections::BTreeMap;

/// A row's plain columns: text and integers, never a credential.
pub type Columns = BTreeMap<String, Json>;

/// What a sealed column's ciphertext is bound to: (project, id, the row's plain columns).
type Aad = fn(&str, &str, &Columns) -> String;

/// One row of a table a backup carries.
#[derive(Debug, Serialize, Deserialize)]
pub struct Row {
    /// The table's kind: a link's (`pki`, `ssh`, `saas`, …), or `identity`, `sync`.
    pub kind: String,
    /// Its id where it was taken (a restore makes a new one and moves links to it).
    pub id: String,
    #[serde(default)]
    pub columns: Columns,
    /// Sealed columns, opened (`Value` redacts itself in `Debug`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sealed: BTreeMap<String, Value>,
    /// A CA's certificates that haven't ended, revoked ones too: its CRL stays whole.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub certificates: Vec<Columns>,
    /// A sync whose credential stayed behind (a token's backup): no signing secret, its target's
    /// URLs by host only. It's restored paused until it's made again (`syncs::NO_CREDENTIAL`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub without_credential: bool,
}

impl Row {
    pub fn text(&self, column: &str) -> &str {
        text(&self.columns, column)
    }
}

pub(crate) struct Table {
    pub kind: &'static str,
    pub table: &'static str,
    /// How warnings and `verify` name one ("CA internal").
    pub what: &'static str,
    /// Its ids: `names::new_id(prefix, len)`.
    pub prefix: &'static str,
    pub len: usize,
    pub plain: &'static [&'static str],
    pub sealed: &'static [(&'static str, Aad)],
}

fn text<'a>(c: &'a Columns, column: &str) -> &'a str {
    c.get(column).and_then(Json::as_str).unwrap_or_default()
}

/// Registrations a link names, in the order a restore makes them: CAs first.
pub(crate) const REGISTRATIONS: &[Table] = &[
    Table {
        kind: crate::pki::KIND,
        table: "cas",
        what: "CA",
        prefix: "ca",
        len: 16,
        plain: &["name", "certificate", "policy", "csr", "chain", "unconstrained", "acme_preauthorize"],
        sealed: &[("private_key", |p, id, _| crate::pki::aad(p, id))],
    },
    Table { kind: crate::ssh::KIND, table: "ssh_cas", what: "SSH CA", prefix: "sca", len: 16, plain: &["name", "public_key", "policy"], sealed: &[("private_key", |p, id, _| crate::ssh::aad(p, id))] },
    Table {
        kind: crate::saas::KIND,
        table: "saas_rotators",
        what: "SaaS rotator",
        prefix: "sas",
        len: 16,
        plain: &["name", "vendor", "target", "credential_expires_at"],
        sealed: &[("credential", |p, id, c| crate::saas::aad(p, id, text(c, "vendor"), text(c, "target")))],
    },
    Table {
        kind: crate::public_certs::KIND,
        table: "public_certs",
        what: "public certificate",
        prefix: "pcr",
        len: 16,
        plain: &["name", "directory", "domains", "dns_provider", "eab_kid", "rotate_days", "serial", "not_before", "not_after", "issued_at", "previous_serial", "previous_not_after", "asked_renewals"],
        sealed: &[
            ("dns_credential", |p, id, c| crate::public_certs::aad(p, id, &format!("dns:{}", text(c, "dns_provider")))),
            ("eab_key", |p, id, c| crate::public_certs::aad(p, id, &format!("eab:{}", text(c, "directory")))),
            ("certificate", |p, id, _| crate::public_certs::aad(p, id, "certificate")),
            // A renewal moves the certificate here as it was sealed.
            ("previous_certificate", |p, id, _| crate::public_certs::aad(p, id, "certificate")),
        ],
    },
    Table {
        kind: crate::rotators::KIND,
        table: "rotators",
        what: "rotator",
        prefix: "rot",
        len: 16,
        plain: &["name", "host"],
        sealed: &[("url", |p, id, _| crate::rotators::aad(p, id, "url")), ("secret", |p, id, _| crate::rotators::aad(p, id, "secret"))],
    },
    Table {
        kind: crate::outside::KIND,
        table: "outside_dbs",
        what: "database outside roll",
        prefix: "odb",
        len: 16,
        plain: &["name", "host", "ca", "roles", "grp", "state", "url_next_at", "gateway"],
        sealed: &[("url", |p, id, _| crate::outside::aad(p, id)), ("url_next", |p, id, _| crate::outside::next_aad(p, id))],
    },
    Table {
        kind: crate::github_app::KIND,
        table: "github_apps",
        what: "GitHub App",
        prefix: "gha",
        len: 16,
        plain: &["name", "app_id", "installation_id", "api_base", "repositories", "permissions"],
        sealed: &[("private_key", |p, id, _| crate::github_app::key_aad(p, id))],
    },
];

pub(crate) const IDENTITIES: Table = Table {
    kind: "identity",
    table: "identities",
    what: "identity",
    prefix: "idn",
    len: 12,
    plain: &["name", "issuer", "audience", "subject", "claims", "environments", "paths", "actions", "ttl_minutes", "allowed_ips", "jwks", "kind", "gateway"],
    sealed: &[],
};

/// `env_id` is read to name the environment; a restore writes the new environment's.
pub(crate) const SYNCS: Table = Table {
    kind: "sync",
    table: "syncs",
    what: "sync",
    prefix: "syn",
    len: 16,
    plain: &["env_id", "path", "kind", "target", "keys", "prefix", "state", "remote_id"],
    sealed: &[("secret", |p, id, _| crate::syncs::secret_aad(p, id))],
};

/// A CA's certificates (`ca_certs`), as its CRL and OCSP read them.
const CA_CERTS: &[&str] = &["serial", "names", "source", "not_after", "issued_at", "issued_by", "revoked_at", "revoked_by"];

/// Registrations a backup doesn't carry: their trust names the project they were made in (an AWS
/// role's ExternalId, a cloud's federated subject), or they hold a token of the server's own (a
/// gateway), or they aren't carried yet. Named in the archive so `verify` and a restore can say so.
pub(crate) const LEFT_OUT: &[(&str, &str)] = &[
    ("aws_roles", "AWS role"),
    ("aws_users", "AWS IAM user"),
    ("cloud_accounts", "cloud account"),
    ("kubernetes_clusters", "Kubernetes cluster"),
    ("snowflake_accounts", "Snowflake account"),
    ("atlas_projects", "Atlas project"),
    ("directories", "directory"),
    ("gateways", "gateway"),
];

pub(crate) fn table(kind: &str) -> Option<&'static Table> {
    REGISTRATIONS.iter().find(|t| t.kind == kind)
}

fn to_json(table: &str, column: &str, v: Cell) -> Result<Json> {
    Ok(match v {
        Cell::Null => Json::Null,
        Cell::Int(i) => Json::from(i),
        Cell::Real(r) => Json::from(r),
        Cell::Text(s) => Json::String(s),
        Cell::Blob(_) => return Err(Error::Internal(format!("{table}.{column} holds bytes, not a plain column"))),
    })
}

fn to_cell(what: &str, column: &str, v: Option<&Json>) -> Result<Cell> {
    Ok(match v {
        None | Some(Json::Null) => Cell::Null,
        Some(Json::Bool(b)) => Cell::Int(*b as i64),
        Some(Json::String(s)) => Cell::Text(s.clone()),
        Some(Json::Number(n)) => Cell::Int(n.as_i64().ok_or_else(|| Error::BadRequest(format!("a {what}'s {column} isn't a whole number")))?),
        Some(_) => return Err(Error::BadRequest(format!("a {what}'s {column} is text or a number"))),
    })
}

/// The project's rows of `t`, sealed columns opened with `dek` (a CA's certificates that haven't
/// ended by `now` with it).
pub(crate) fn export(c: &Conn<'_>, t: &Table, project: &str, dek: &Dek, now: i64) -> Result<Vec<Row>> {
    let sealed: Vec<&str> = t.sealed.iter().map(|(s, _)| *s).collect();
    let columns = [&["id"][..], t.plain, &sealed].concat().join(", ");
    let rows: Vec<Vec<Cell>> = c.query(&format!("SELECT {columns} FROM {} WHERE project_id = ? ORDER BY id", t.table), [project], |r| {
        (0..1 + t.plain.len() + sealed.len()).map(|i| r.get::<_, Cell>(i)).collect()
    })?;
    let mut out = vec![];
    for row in rows {
        let mut cells = row.into_iter();
        let id = match cells.next() {
            Some(Cell::Text(id)) => id,
            _ => return Err(Error::Internal(format!("a {} row without an id", t.table))),
        };
        let mut r = Row { kind: t.kind.to_string(), id, columns: Columns::new(), sealed: BTreeMap::new(), certificates: vec![], without_credential: false };
        for column in t.plain {
            let v = cells.next().unwrap_or(Cell::Null);
            r.columns.insert(column.to_string(), to_json(t.table, column, v)?);
        }
        for (column, aad) in t.sealed {
            if let Some(Cell::Blob(ct)) = cells.next() {
                r.sealed.insert(column.to_string(), dek.open(&aad(project, &r.id, &r.columns), &ct)?);
            }
        }
        if t.table == "cas" {
            let certs: Vec<Vec<Cell>> = c.query(&format!("SELECT {} FROM ca_certs WHERE ca_id = ? AND not_after > ? ORDER BY serial", CA_CERTS.join(", ")), code_sql::params![r.id, now], |x| {
                (0..CA_CERTS.len()).map(|i| x.get::<_, Cell>(i)).collect()
            })?;
            for cert in certs {
                r.certificates.push(CA_CERTS.iter().zip(cert).map(|(k, v)| Ok((k.to_string(), to_json("ca_certs", k, v)?))).collect::<Result<_>>()?);
            }
        }
        out.push(r);
    }
    Ok(out)
}

/// Write `r` into `project` as row `id` of `t`, made by `by`: its listed columns, sealed ones
/// sealed again with `dek` (bound to the new project and id), and `extra` over anything listed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn import(c: &Conn<'_>, t: &Table, project: &str, id: &str, by: &str, now: i64, r: &Row, dek: &Dek, extra: &[(&str, Cell)]) -> Result<()> {
    let mut columns = vec!["id", "project_id", "created_at", "created_by"];
    let mut values = vec![Cell::Text(id.into()), Cell::Text(project.into()), Cell::Int(now), Cell::Text(by.into())];
    for (column, v) in extra {
        columns.push(*column);
        values.push(v.clone());
    }
    for column in t.plain.iter().filter(|p| !extra.iter().any(|(e, _)| e == *p)) {
        columns.push(*column);
        values.push(to_cell(t.what, column, r.columns.get(*column))?);
    }
    for (column, aad) in t.sealed {
        columns.push(*column);
        values.push(r.sealed.get(*column).map_or(Cell::Null, |v| Cell::Blob(dek.seal(&aad(project, id, &r.columns), v))));
    }
    let marks = vec!["?"; columns.len()].join(", ");
    c.execute(&format!("INSERT INTO {} ({}) VALUES ({marks})", t.table, columns.join(", ")), values)?;
    for cert in &r.certificates {
        let mut values = vec![Cell::Text(id.into()), Cell::Text(project.into())];
        for column in CA_CERTS {
            values.push(to_cell("CA certificate", column, cert.get(*column))?);
        }
        c.execute(&format!("INSERT INTO ca_certs (ca_id, project_id, {}) VALUES (?, ?, {})", CA_CERTS.join(", "), vec!["?"; CA_CERTS.len()].join(", ")), values)?;
    }
    Ok(())
}

/// The names of the project's registrations a backup doesn't carry ("AWS role deploy").
pub(crate) fn left_out(c: &Conn<'_>, project: &str) -> Result<Vec<String>> {
    let mut out = vec![];
    for (table, what) in LEFT_OUT {
        let names: Vec<String> = c.query(&format!("SELECT name FROM {table} WHERE project_id = ? ORDER BY name"), [project], |r| r.get(0))?;
        out.extend(names.into_iter().map(|n| format!("{what} {n}")));
    }
    Ok(out)
}
