//! ScyllaDB service levels: `CREATE / ALTER / DROP SERVICE LEVEL`, `ATTACH / DETACH`, and the
//! `LIST … SERVICE LEVEL(S)` forms, stored in `system.service_levels_v2` (definitions) and
//! `system.role_attributes` (`service_level` attribute of a role), as ScyllaDB 2026.3 does.
//!
//! Levels are accepted and stored; their timeout, workload type and shares only affect the
//! values reported (`LIST EFFECTIVE SERVICE LEVEL` merges a role's levels: the smallest timeout,
//! `batch` over `interactive` over unspecified, the fewest shares — as observed, although the
//! workload-prioritization docs say the most shares win). Behaviour and messages come
//! from the reference (`tests/captured_auth.rs`); permission checks are on `<all service
//! levels>` (CREATE, ALTER, DROP, DESCRIBE for the lists, AUTHORIZE for attach / detach),
//! which only superusers hold.

use std::sync::Arc;

use bytes::Bytes;
use nosql_cql::ast::{
    ListServiceLevels, LiteralKind, Permission, Property, PropertyValue, Statement, StatementKind,
};
use nosql_types::{CqlType, Dialect, Duration};

use super::authorizer::Ctx;
use super::messages as m;
use super::resource::Resource;
use super::store::{self, ServiceLevel, Snapshot, Write, p_text};
use crate::Database;
use crate::error::Result;
use crate::response::{ColumnSpec, Param, Response, ResponseBody, ResultMetadata, Rows};
use crate::session::SessionState;

const DEFAULT_SHARES: i32 = 1000;

/// Service levels, `default` included (`docs/features/workload-prioritization.rst`, "Limits").
const MAX_SERVICE_LEVELS: usize = 10;

pub async fn execute(
    db: &Arc<Database>,
    session: &SessionState,
    stmt: &Statement<'static>,
) -> Result<Response> {
    if db.dialect() != Dialect::Scylla {
        return Err(crate::hooks::unsupported(&stmt.kind));
    }
    match &stmt.kind {
        StatementKind::CreateServiceLevel(s) => {
            let props = parse_props(&s.properties, stmt)?;
            let name = s.name.name().into_owned();
            let (_g, snap) = store::lock(db).await?;
            check(db, session, Permission::Create)?;
            if name == "default" {
                return Err(m::sl_default_create());
            }
            if snap.service_levels.contains_key(&name) {
                if s.if_exists {
                    return Ok(Response::void());
                }
                return Err(m::sl_exists(&name));
            }
            // Each level takes a scheduling group: ten with `default` (and `driver`).
            if snap.service_levels.len() + 1 >= MAX_SERVICE_LEVELS {
                return Err(crate::error::Error::invalid(
                    "Can't create service level - no more scheduling groups exist",
                ));
            }
            let sl = ServiceLevel {
                shares: Some(props.shares.unwrap_or(DEFAULT_SHARES)),
                timeout: props.timeout.flatten(),
                workload_type: props.workload.flatten(),
            };
            write_level(db, &name, &sl).await?;
            store::reload(db).await?;
            Ok(Response::void())
        }
        StatementKind::AlterServiceLevel(s) => {
            let props = parse_props(&s.properties, stmt)?;
            let name = s.name.name().into_owned();
            let (_g, snap) = store::lock(db).await?;
            check(db, session, Permission::Alter)?;
            if name == "default" {
                return Err(m::sl_default_alter());
            }
            let Some(cur) = snap.service_levels.get(&name) else {
                return Err(m::sl_missing_alter(&name));
            };
            let sl = ServiceLevel {
                shares: props.shares.or(cur.shares),
                timeout: match props.timeout {
                    Some(t) => t,
                    None => cur.timeout.clone(),
                },
                workload_type: match props.workload {
                    Some(w) => w,
                    None => cur.workload_type.clone(),
                },
            };
            write_level(db, &name, &sl).await?;
            store::reload(db).await?;
            Ok(Response::void())
        }
        StatementKind::DropServiceLevel(x) => {
            let name = x.name.name().into_owned();
            let (_g, snap) = store::lock(db).await?;
            check(db, session, Permission::Drop)?;
            if name == "default" {
                return Err(m::sl_default_drop());
            }
            if !snap.service_levels.contains_key(&name) {
                if x.if_exists {
                    return Ok(Response::void());
                }
                return Err(m::sl_missing(&name));
            }
            let mut w = vec![Write::new(
                "DELETE FROM system.service_levels_v2 WHERE service_level = ?",
                vec![p_text(&name)],
            )];
            for (role, attrs) in &snap.attributes {
                if attrs.get("service_level") == Some(&name) {
                    w.push(detach(role));
                }
            }
            store::apply(db, w, None).await?;
            store::reload(db).await?;
            Ok(Response::void())
        }
        StatementKind::AttachServiceLevel(a) => {
            let (sl, role) = (a.role.name().into_owned(), a.grantee.name().into_owned());
            let (_g, snap) = store::lock(db).await?;
            check(db, session, Permission::Authorize)?;
            if sl == "default" {
                return Err(m::sl_default_attach());
            }
            if !snap.service_levels.contains_key(&sl) {
                return Err(m::sl_missing(&sl));
            }
            if !snap.exists(&role) {
                return Err(m::role_missing(Dialect::Scylla, &role));
            }
            store::apply(
                db,
                vec![Write::new(
                    "INSERT INTO system.role_attributes (role, name, value) VALUES (?, 'service_level', ?)",
                    vec![p_text(&role), p_text(&sl)],
                )],
                None,
            )
            .await?;
            store::reload(db).await?;
            Ok(Response::void())
        }
        StatementKind::DetachServiceLevel(r) => {
            let role = r.name().into_owned();
            let (_g, snap) = store::lock(db).await?;
            check(db, session, Permission::Authorize)?;
            if !snap.exists(&role) {
                return Err(m::role_missing(Dialect::Scylla, &role));
            }
            store::apply(db, vec![detach(&role)], None).await?;
            store::reload(db).await?;
            Ok(Response::void())
        }
        StatementKind::ListServiceLevels(l) => {
            let snap = store::ensure_fresh(db).await?;
            list(db, session, &snap, l)
        }
        _ => Err(crate::hooks::unsupported(&stmt.kind)),
    }
}

fn detach(role: &str) -> Write {
    Write::new(
        "DELETE FROM system.role_attributes WHERE role = ? AND name = 'service_level'",
        vec![p_text(role)],
    )
}

async fn write_level(db: &Arc<Database>, name: &str, sl: &ServiceLevel) -> Result<()> {
    let opt = |b: Option<Bytes>| b.map(Param::new).unwrap_or(Param::Null);
    store::apply(
        db,
        vec![Write::new(
            "INSERT INTO system.service_levels_v2 (service_level, shares, timeout, workload_type) VALUES (?, ?, ?, ?)",
            vec![
                p_text(name),
                opt(sl.shares.map(|s| Bytes::copy_from_slice(&s.to_be_bytes()))),
                opt(sl.timeout.clone()),
                opt(sl.workload_type.as_deref().map(|w| Bytes::copy_from_slice(w.as_bytes()))),
            ],
        )],
        None,
    )
    .await
}

/// The permission on `<all service levels>` (superusers only in practice).
fn check(db: &Database, session: &SessionState, p: Permission) -> Result<()> {
    let probe = session.clone();
    let cx = Ctx::new(db, &probe);
    if !cx.enforce || cx.user_is_super() {
        return Ok(());
    }
    cx.ensure(p, &Resource::ServiceLevels)
}

/// Properties of CREATE / ALTER: `None` not given, `Some(None)` set to null.
#[derive(Default)]
struct Props {
    shares: Option<i32>,
    timeout: Option<Option<Bytes>>,
    workload: Option<Option<String>>,
}

fn parse_props(props: &[Property<'_>], stmt: &Statement<'_>) -> Result<Props> {
    let mut out = Props::default();
    for p in props {
        let name = p.name.name().into_owned();
        let (kind, text) = match &p.value {
            PropertyValue::Simple(l) => (Some(l.kind), l.text.to_string()),
            PropertyValue::Map(_) => (None, String::new()),
        };
        match name.as_str() {
            "shares" => {
                if kind.is_none() {
                    return Err(m::sl_value_not_string(&name));
                }
                let n = stoi(&text).ok_or_else(|| m::sl_invalid_integer(&text))?;
                if !(1..=1000).contains(&n) {
                    return Err(m::sl_shares_range(&n.to_string()));
                }
                out.shares = Some(n as i32);
            }
            "timeout" => {
                if kind.is_none() {
                    return Err(m::sl_value_not_string(&name));
                }
                if kind == Some(LiteralKind::String) && text == "null" {
                    out.timeout = Some(None);
                    continue;
                }
                let d = Duration::parse(&text, Dialect::Scylla)
                    .map_err(|_| m::sl_invalid_duration(&stmt.to_string(), &text))?;
                if d.is_negative() {
                    return Err(m::sl_timeout_negative());
                }
                if d.months != 0 || d.days != 0 {
                    return Err(m::sl_timeout_days());
                }
                if d.nanos % 1_000_000 != 0 {
                    return Err(m::sl_timeout_granularity());
                }
                out.timeout = Some(Some(Bytes::from(d.encode())));
            }
            "workload_type" => {
                if kind.is_none() {
                    return Err(m::sl_value_not_string(&name));
                }
                out.workload = Some(match text.as_str() {
                    "null" if kind == Some(LiteralKind::String) => None,
                    "interactive" | "batch" => Some(text),
                    _ => return Err(m::sl_workload(&text)),
                });
            }
            _ => return Err(m::sl_unknown_property(&name)),
        }
    }
    Ok(out)
}

/// C's `stoi`: an optional sign and the leading digits (`'1.5'` is 1).
fn stoi(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let end = digits
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(digits.len());
    if end == 0 {
        return None;
    }
    let n: i64 = digits[..end].parse().ok()?;
    Some(if neg { -n } else { n })
}

fn col(table: &str, name: &str, ty: CqlType) -> ColumnSpec {
    ColumnSpec::new("QOS", table, name, ty)
}

fn text(s: &str) -> Option<Bytes> {
    Some(Bytes::copy_from_slice(s.as_bytes()))
}

fn rows(columns: Vec<ColumnSpec>, rows: Vec<Vec<Option<Bytes>>>) -> Response {
    Response::new(ResponseBody::Rows(Rows {
        metadata: ResultMetadata { columns },
        rows,
        ..Default::default()
    }))
}

fn level_row(name: &str, sl: &ServiceLevel) -> Vec<Option<Bytes>> {
    vec![
        text(name),
        sl.timeout.clone(),
        sl.workload_type.as_deref().and_then(text),
        Some(Bytes::copy_from_slice(
            &sl.shares.unwrap_or(DEFAULT_SHARES).to_be_bytes(),
        )),
    ]
}

fn list(
    db: &Database,
    session: &SessionState,
    snap: &Snapshot,
    l: &ListServiceLevels<'_>,
) -> Result<Response> {
    let level_cols = || {
        vec![
            col("service_levels", "service_level", CqlType::Text),
            col("service_levels", "timeout", CqlType::Duration),
            col("service_levels", "workload_type", CqlType::Text),
            col("service_levels", "shares", CqlType::Int),
        ]
    };
    let attach_cols = || {
        vec![
            col("service_levels_attachments", "role", CqlType::Text),
            col("service_levels_attachments", "service_level", CqlType::Text),
        ]
    };
    match l {
        ListServiceLevels::All => {
            check(db, session, Permission::Describe)?;
            let total: i64 = snap
                .service_levels
                .values()
                .map(|s| s.shares.unwrap_or(DEFAULT_SHARES) as i64)
                .sum();
            let mut cols = level_cols();
            cols.push(col(
                "service_levels",
                "percentage of all service level shares",
                CqlType::Text,
            ));
            let out = snap
                .service_levels
                .iter()
                .map(|(n, sl)| {
                    let mut r = level_row(n, sl);
                    let share = sl.shares.unwrap_or(DEFAULT_SHARES) as f64;
                    r.push(text(&format!(
                        "{:.2}%",
                        share * 100.0 / total.max(1) as f64
                    )));
                    r
                })
                .collect();
            Ok(rows(cols, out))
        }
        ListServiceLevels::One(name) => {
            check(db, session, Permission::Describe)?;
            let name = name.name();
            let out = snap
                .service_levels
                .get(name.as_ref())
                .map(|sl| level_row(&name, sl))
                .into_iter()
                .collect();
            Ok(rows(level_cols(), out))
        }
        ListServiceLevels::AttachedOf(role) => {
            check(db, session, Permission::Describe)?;
            let role = role.name();
            let out = snap
                .attributes
                .get(role.as_ref())
                .and_then(|a| a.get("service_level"))
                .map(|sl| vec![text(&role), text(sl)])
                .into_iter()
                .collect();
            Ok(rows(attach_cols(), out))
        }
        ListServiceLevels::AllAttached => {
            check(db, session, Permission::Describe)?;
            // The reference lists them in hash-map order (neither token nor name order);
            // roll sorts by role.
            let out = snap
                .attributes
                .iter()
                .filter_map(|(r, a)| a.get("service_level").map(|sl| vec![text(r), text(sl)]))
                .collect();
            Ok(rows(attach_cols(), out))
        }
        ListServiceLevels::EffectiveOf(role) => {
            let role = role.name().into_owned();
            if !snap.exists(&role) {
                return Err(m::role_missing(Dialect::Scylla, &role));
            }
            effective(snap, &role)
        }
    }
}

/// The scheduling group `role`'s requests run in, as ScyllaDB labels its metrics
/// (`scheduling_group_name`): `sl:<level>` of the level whose shares the role's effective
/// service level takes, `sl:default` without one.
pub async fn scheduling_group(db: &Arc<Database>, role: &str) -> String {
    // Service levels attached a moment ago count (the auth cache may be older).
    let snap = match super::store::ensure_fresh(db).await {
        Ok(s) => s,
        Err(_) => super::snapshot(db),
    };
    let mut order: Vec<String> = vec![role.to_string()];
    order.extend(snap.roles_of(role, true).into_iter().filter(|r| r != role));
    let mut best: Option<(&String, i32)> = None;
    for (name, sl) in order
        .iter()
        .filter_map(|r| snap.attributes.get(r)?.get("service_level"))
        .filter_map(|n| snap.service_levels.get_key_value(n))
    {
        let shares = sl.shares.unwrap_or(DEFAULT_SHARES);
        if best.is_none_or(|(_, s)| shares < s) {
            best = Some((name, shares));
        }
    }
    format!("sl:{}", best.map_or("default", |(n, _)| n.as_str()))
}

/// `LIST EFFECTIVE SERVICE LEVEL OF role`: the role's own level first, then those of the roles
/// granted to it (by name); ties keep the earlier level.
fn effective(snap: &Snapshot, role: &str) -> Result<Response> {
    let mut order: Vec<String> = vec![role.to_string()];
    order.extend(snap.roles_of(role, true).into_iter().filter(|r| r != role));
    let levels: Vec<(&String, &ServiceLevel)> = order
        .iter()
        .filter_map(|r| snap.attributes.get(r)?.get("service_level"))
        .filter_map(|n| snap.service_levels.get_key_value(n))
        .collect();
    let Some(first) = levels.first() else {
        return Err(m::sl_none_assigned(role));
    };
    let rank = |w: &Option<String>| match w.as_deref() {
        Some("batch") => 2,
        Some("interactive") => 1,
        _ => 0,
    };
    let (mut wl, mut to, mut sh) = (*first, *first, *first);
    for l in &levels[1..] {
        if rank(&l.1.workload_type) > rank(&wl.1.workload_type) {
            wl = *l;
        }
        // An unset timeout takes the next level's (even unset); otherwise the smaller wins.
        let dur = |s: &ServiceLevel| s.timeout.as_deref().and_then(Duration::decode);
        match (dur(to.1), dur(l.1)) {
            (None, _) => to = *l,
            (Some(cur), Some(next)) if next.nanos < cur.nanos => to = *l,
            _ => {}
        }
        // The fewest shares win (observed; ties keep the earlier level).
        if l.1.shares.unwrap_or(DEFAULT_SHARES) < sh.1.shares.unwrap_or(DEFAULT_SHARES) {
            sh = *l;
        }
    }
    let timeout =
        to.1.timeout
            .as_deref()
            .and_then(Duration::decode)
            .map(|d| d.to_text());
    let cols = ["service_level_option", "effective_service_level", "value"]
        .iter()
        .map(|c| col("effective_service_level", c, CqlType::Text))
        .collect();
    let out = vec![
        vec![
            text("workload_type"),
            text(wl.0),
            text(wl.1.workload_type.as_deref().unwrap_or("unspecified")),
        ],
        vec![
            text("timeout"),
            text(to.0),
            timeout.as_deref().and_then(text),
        ],
        vec![
            text("shares"),
            text(sh.0),
            text(&sh.1.shares.unwrap_or(DEFAULT_SHARES).to_string()),
        ],
    ];
    Ok(rows(cols, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stoi_like_scylla() {
        assert_eq!(stoi("7"), Some(7));
        assert_eq!(stoi("1.5"), Some(1));
        assert_eq!(stoi("-1"), Some(-1));
        assert_eq!(stoi("x"), None);
        assert_eq!(stoi("null"), None);
    }
}
