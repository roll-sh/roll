//! Subscriptions (subscriptioncmds.c): CREATE / ALTER / DROP SUBSCRIPTION. What needs the
//! publisher (checking publications, listing its tables, creating, altering and dropping
//! the slot) connects to it with `replication=database` (`repl::walrcv`), through the
//! outbound allow-list; the apply workers (`repl::apply`) do the replicating.

use super::Ddl;
use crate::catalog::acl;
use crate::catalog::{SubRel, Subscription};
use crate::remote::conn::Conn;
use crate::repl::walrcv::{self, Fail};
use crate::error::{Error, Result, code};
use crate::sql::binder::ast::{def_arg_str, inner, with_pos};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;

/// pg_create_subscription.
const PG_CREATE_SUBSCRIPTION: u32 = 6304;
/// ERRCODE_S_R_E_PROHIBITED_SQL_STATEMENT_ATTEMPTED and ERRCODE_CONNECTION_FAILURE.
const PROHIBITED_STATEMENT: &str = "2F003";
const CONNECTION_FAILURE: &str = "08006";

/// libpq's connection options (PQconninfoOptions).
const CONNINFO_OPTIONS: &[&str] = &[
    "service",
    "user",
    "password",
    "passfile",
    "channel_binding",
    "connect_timeout",
    "dbname",
    "host",
    "hostaddr",
    "port",
    "client_encoding",
    "options",
    "application_name",
    "fallback_application_name",
    "keepalives",
    "keepalives_idle",
    "keepalives_interval",
    "keepalives_count",
    "tcp_user_timeout",
    "sslmode",
    "sslnegotiation",
    "sslcompression",
    "sslcert",
    "sslkey",
    "sslcertmode",
    "sslpassword",
    "sslrootcert",
    "sslcrl",
    "sslcrldir",
    "sslsni",
    "requirepeer",
    "require_auth",
    "min_protocol_version",
    "max_protocol_version",
    "ssl_min_protocol_version",
    "ssl_max_protocol_version",
    "gssencmode",
    "krbsrvname",
    "gsslib",
    "gssdelegation",
    "replication",
    "target_session_attrs",
    "load_balance_hosts",
    "scram_client_key",
    "scram_server_key",
    "oauth_issuer",
    "oauth_client_id",
    "oauth_client_secret",
    "oauth_scope",
];

/// PQconninfoParse of a `key=value ...` string: the options, or libpq's message.
fn parse_conninfo(s: &str) -> std::result::Result<Vec<(String, String)>, String> {
    if s.starts_with("postgresql://") || s.starts_with("postgres://") {
        return Ok(Vec::new());
    }
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        if b[i].is_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && b[i] != '=' && !b[i].is_whitespace() {
            i += 1;
        }
        let key: String = b[start..i].iter().collect();
        while i < b.len() && b[i].is_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] != '=' {
            return Err(format!(
                "missing \"=\" after \"{key}\" in connection info string\n"
            ));
        }
        i += 1;
        while i < b.len() && b[i].is_whitespace() {
            i += 1;
        }
        let mut val = String::new();
        if i < b.len() && b[i] == '\'' {
            i += 1;
            loop {
                if i >= b.len() {
                    return Err("unterminated quoted string in connection info string\n".into());
                }
                match b[i] {
                    '\\' if i + 1 < b.len() => {
                        val.push(b[i + 1]);
                        i += 2;
                    }
                    '\'' => {
                        i += 1;
                        break;
                    }
                    c => {
                        val.push(c);
                        i += 1;
                    }
                }
            }
        } else {
            while i < b.len() && !b[i].is_whitespace() {
                if b[i] == '\\' && i + 1 < b.len() {
                    i += 1;
                }
                val.push(b[i]);
                i += 1;
            }
        }
        if !CONNINFO_OPTIONS.contains(&key.as_str()) {
            return Err(format!("invalid connection option \"{key}\"\n"));
        }
        out.push((key, val));
    }
    Ok(out)
}

/// walrcv_check_conninfo.
fn check_conninfo(s: &str, must_use_password: bool) -> Result<Vec<(String, String)>> {
    let opts = parse_conninfo(s).map_err(|m| {
        Error::new(
            code::SYNTAX_ERROR,
            format!("invalid connection string syntax: {m}"),
        )
    })?;
    if must_use_password
        && !opts
            .iter()
            .any(|(k, v)| k == "password" && !v.is_empty())
    {
        return Err(Error::new(PROHIBITED_STATEMENT, "password is required")
            .detail("Non-superusers must provide a password in the connection string."));
    }
    Ok(opts)
}

/// A failure at the publisher as an error with `code` (an interrupt as itself).
fn failed(e: Fail, code: &'static str) -> Error {
    match e {
        Fail::Interrupt(e) => e,
        Fail::Remote { msg, .. } => Error::new(code, msg),
    }
}

/// check_publications' warning: the publications the publisher doesn't have.
fn missing_publications_warning(missing: &[String]) -> Error {
    let list = walrcv::publications_str(missing, false);
    let msg = if missing.len() == 1 {
        format!("publication {list} does not exist on the publisher")
    } else {
        format!("publications {list} do not exist on the publisher")
    };
    crate::session::warning(code::UNDEFINED_OBJECT, msg)
}

/// The options a statement gives (parse_subscription_options).
#[derive(Default)]
struct SubOptions {
    specified: Vec<&'static str>,
    connect: bool,
    enabled: bool,
    create_slot: bool,
    /// Some(None): slot_name = NONE.
    slot_name: Option<Option<String>>,
    copy_data: bool,
    synchronous_commit: Option<String>,
    refresh: bool,
    binary: bool,
    streaming: char,
    two_phase: bool,
    disable_on_error: bool,
    password_required: bool,
    run_as_owner: bool,
    failover: bool,
    origin: String,
    /// SKIP (lsn): None for NONE.
    lsn: Option<u64>,
}

impl SubOptions {
    fn given(&self, o: &str) -> bool {
        self.specified.contains(&o)
    }
}

/// defGetBoolean.
fn def_bool(d: &pb::DefElem) -> Result<bool> {
    let Some(arg) = d.arg.as_deref() else {
        return Ok(true);
    };
    let v = match inner(arg) {
        Some(NodeEnum::Integer(i)) => match i.ival {
            0 => return Ok(false),
            1 => return Ok(true),
            _ => String::new(),
        },
        Some(NodeEnum::Boolean(b)) => return Ok(b.boolval),
        _ => def_arg_str(d).unwrap_or_default(),
    };
    match v.to_ascii_lowercase().as_str() {
        "true" | "on" | "t" | "tr" | "tru" | "yes" | "y" | "ye" | "1" => Ok(true),
        "false" | "off" | "f" | "fa" | "fal" | "fals" | "no" | "n" | "of" | "0" => Ok(false),
        _ => Err(Error::new(
            code::SYNTAX_ERROR,
            format!("{} requires a Boolean value", d.defname),
        )),
    }
}

/// ReplicationSlotValidateName.
fn validate_slot_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::new(
            code::INVALID_NAME,
            format!("replication slot name \"{name}\" is too short"),
        ));
    }
    if name.len() >= 64 {
        return Err(Error::new(
            code::NAME_TOO_LONG,
            format!("replication slot name \"{name}\" is too long"),
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(Error::new(
            code::INVALID_NAME,
            format!("replication slot name \"{name}\" contains invalid character"),
        )
        .hint("Replication slot names may only contain lower case letters, numbers, and the underscore character."));
    }
    Ok(())
}

fn parse_options(opts: &[pb::Node], supported: &[&'static str]) -> Result<SubOptions> {
    // The defaults of the options this statement takes.
    let has = |o: &str| supported.contains(&o);
    let mut o = SubOptions {
        connect: has("connect"),
        enabled: has("enabled"),
        create_slot: has("create_slot"),
        copy_data: has("copy_data"),
        refresh: has("refresh"),
        streaming: 'p',
        password_required: true,
        origin: "any".into(),
        ..Default::default()
    };
    for d in super::def_elems(opts) {
        let Some(name) = supported.iter().copied().find(|s| *s == d.defname) else {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                format!("unrecognized subscription parameter: \"{}\"", d.defname),
            ));
        };
        if o.given(name) {
            return Err(with_pos(
                Error::new(code::SYNTAX_ERROR, "conflicting or redundant options"),
                d.location,
            ));
        }
        o.specified.push(name);
        match name {
            "connect" => o.connect = def_bool(d)?,
            "enabled" => o.enabled = def_bool(d)?,
            "create_slot" => o.create_slot = def_bool(d)?,
            "slot_name" => {
                let v = def_arg_str(d).unwrap_or_default();
                o.slot_name = Some(if v == "none" {
                    None
                } else {
                    validate_slot_name(&v)?;
                    Some(v)
                });
            }
            "copy_data" => o.copy_data = def_bool(d)?,
            "synchronous_commit" => {
                let v = def_arg_str(d).unwrap_or_default();
                crate::session::settings::check_setting("synchronous_commit", &v)?;
                o.synchronous_commit = Some(v);
            }
            "refresh" => o.refresh = def_bool(d)?,
            "binary" => o.binary = def_bool(d)?,
            "streaming" => {
                o.streaming = match d.arg.as_deref().and_then(inner) {
                    None => 't',
                    Some(NodeEnum::Integer(i)) if i.ival == 0 => 'f',
                    Some(NodeEnum::Integer(i)) if i.ival == 1 => 't',
                    Some(NodeEnum::Integer(_)) => 'x',
                    _ => match def_arg_str(d).unwrap_or_default().to_ascii_lowercase().as_str()
                    {
                        "false" | "off" => 'f',
                        "true" | "on" => 't',
                        "parallel" => 'p',
                        _ => 'x',
                    },
                };
                if o.streaming == 'x' {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        format!("{} requires a Boolean value or \"parallel\"", d.defname),
                    ));
                }
            }
            "two_phase" => o.two_phase = def_bool(d)?,
            "disable_on_error" => o.disable_on_error = def_bool(d)?,
            "password_required" => o.password_required = def_bool(d)?,
            "run_as_owner" => o.run_as_owner = def_bool(d)?,
            "failover" => o.failover = def_bool(d)?,
            "origin" => {
                let v = def_arg_str(d).unwrap_or_default();
                if !v.eq_ignore_ascii_case("none") && !v.eq_ignore_ascii_case("any") {
                    return Err(Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        format!("unrecognized origin value: \"{v}\""),
                    ));
                }
                o.origin = v;
            }
            "lsn" => {
                let v = def_arg_str(d).unwrap_or_default();
                o.lsn = if v == "none" {
                    None
                } else {
                    let l = crate::types::text::parse_lsn(&v)?;
                    if l == 0 {
                        return Err(Error::new(
                            code::INVALID_PARAMETER_VALUE,
                            format!("invalid WAL location (LSN): {v}"),
                        ));
                    }
                    Some(l)
                };
            }
            _ => {}
        }
    }
    let excl = |a: &str, b: &str| {
        Err(Error::new(
            code::SYNTAX_ERROR,
            format!("{a} and {b} are mutually exclusive options"),
        ))
    };
    if supported.contains(&"connect") && !o.connect {
        if o.enabled && o.given("enabled") {
            return excl("connect = false", "enabled = true");
        }
        if o.create_slot && o.given("create_slot") {
            return excl("connect = false", "create_slot = true");
        }
        if o.copy_data && o.given("copy_data") {
            return excl("connect = false", "copy_data = true");
        }
        o.enabled = false;
        o.create_slot = false;
        o.copy_data = false;
    }
    if o.slot_name == Some(None) {
        let must = |a: &str| {
            Err(Error::new(
                code::SYNTAX_ERROR,
                format!("subscription with slot_name = NONE must also set {a}"),
            ))
        };
        if o.enabled {
            return if o.given("enabled") {
                excl("slot_name = NONE", "enabled = true")
            } else {
                must("enabled = false")
            };
        }
        if o.create_slot {
            return if o.given("create_slot") {
                excl("slot_name = NONE", "create_slot = true")
            } else {
                must("create_slot = false")
            };
        }
    }
    Ok(o)
}

/// publicationListToArray: the names, each once.
fn publication_names(list: &[pb::Node]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for n in list {
        let name = crate::sql::binder::ast::sval(n).unwrap_or_default().to_string();
        if out.contains(&name) {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("publication name \"{name}\" used more than once"),
            ));
        }
        out.push(name);
    }
    Ok(out)
}

fn superuser_only() -> Error {
    Error::new(
        code::INSUFFICIENT_PRIVILEGE,
        "password_required=false is superuser-only",
    )
    .hint("Subscriptions with the password_required option set to false may only be created or modified by the superuser.")
}

impl Ddl<'_, '_> {
    /// walrcv_connect to subscription `name`'s publisher; the error is libpq's message.
    fn publisher(&self, conninfo: &str, name: &str) -> std::result::Result<Conn, String> {
        walrcv::connect(conninfo, name, &crate::remote::env(self.ctx.sess))
    }

    /// A connection for subscription `x` must have used a password (password_required, an
    /// owner who isn't superuser).
    fn must_use_password(&self, x: &Subscription) -> bool {
        x.password_required && !self.cat.is_superuser(x.owner)
    }

    /// The local table a published `nsp.rel` replicates into (RangeVarGetRelid and
    /// CheckSubscriptionRelkind).
    fn subscription_target(&self, nsp: &str, rel: &str) -> Result<u32> {
        match self.cat.lookup(Some(nsp), rel, &[])? {
            Some(crate::catalog::Relation::Table(t)) => Ok(t.oid),
            Some(r) => {
                let what = match r {
                    crate::catalog::Relation::Sequence(_) => "sequences",
                    crate::catalog::Relation::Index(_) => "indexes",
                    _ => "views",
                };
                Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("cannot use relation \"{nsp}.{rel}\" as logical replication target"),
                )
                .detail(format!("This operation is not supported for {what}.")))
            }
            None => Err(Error::new(code::UNDEFINED_TABLE, format!("relation \"{nsp}.{rel}\" does not exist"))),
        }
    }

    /// The published tables as pg_subscription_rel rows in `state`, checking `validate`'s
    /// publications exist there first.
    fn published_rels(&self, c: &mut Conn, x_pubs: &[String], validate: &[String], state: char) -> Result<Vec<SubRel>> {
        let intr = crate::remote::interrupts(self.ctx);
        if !validate.is_empty() {
            let missing = walrcv::missing_publications(c, validate, &intr).map_err(|e| failed(e, CONNECTION_FAILURE))?;
            if !missing.is_empty() {
                self.ctx.notice(missing_publications_warning(&missing));
            }
        }
        let mut out: Vec<SubRel> = Vec::new();
        for (nsp, rel) in walrcv::fetch_table_list(c, x_pubs, &intr).map_err(|e| failed(e, CONNECTION_FAILURE))? {
            let relid = self.subscription_target(&nsp, &rel)?;
            if !out.iter().any(|r| r.relid == relid) {
                out.push(SubRel { relid, state, lsn: 0 });
            }
        }
        Ok(out)
    }

    /// AlterSubscription_refresh: tables the publications have now are added (to be copied
    /// unless `copy_data` is off), those they don't any more removed.
    fn refresh_subscription(&mut self, x: &Subscription, copy_data: bool, validate: &[String]) -> Result<()> {
        let mut c = self.publisher(&x.conninfo, &x.name).map_err(|m| {
            Error::new(CONNECTION_FAILURE, format!("subscription \"{}\" could not connect to the publisher: {m}", x.name))
        })?;
        walrcv::password_check(&c, self.must_use_password(x))?;
        let now = self.published_rels(&mut c, &x.publications, validate, if copy_data { 'i' } else { 'r' })?;
        let mut rels = self.cat.sub_rels.get(&x.oid).cloned().unwrap_or_default();
        // A removed table still being synchronized: its worker stops, its synchronization
        // slot (unless SYNCDONE: dropped already) and origin go.
        let removed: Vec<SubRel> =
            rels.iter().filter(|r| r.state != 'r' && !now.iter().any(|n| n.relid == r.relid)).cloned().collect();
        let workers = self.ctx.sess.cluster.logical_workers();
        for r in &removed {
            if let Some(w) = &workers {
                w.stop_sync(x.oid, r.relid);
            }
            if let Some(repl) = self.ctx.sess.db.repl() {
                let _ = repl.origins.drop_origin(&format!("pg_{}_{}", x.oid, r.relid), true);
            }
        }
        let slots: Vec<String> = removed
            .iter()
            .filter(|r| r.state != 's')
            .map(|r| format!("pg_{}_sync_{}_{}", x.oid, r.relid, crate::repl::SYSTEM_IDENTIFIER))
            .collect();
        self.drop_sync_slots(&mut c, &slots)?;
        rels.retain(|r| now.iter().any(|n| n.relid == r.relid));
        for n in now {
            if !rels.iter().any(|r| r.relid == n.relid) {
                rels.push(n);
            }
        }
        self.cat.sub_rels.insert(x.oid, rels);
        Ok(())
    }

    /// walrcv_alter_slot (ALTER SUBSCRIPTION ... SET (failover / two_phase)).
    fn alter_subscription_slot(&self, x: &Subscription, failover: Option<bool>, two_phase: Option<bool>) -> Result<()> {
        let slot = x.slot_name.clone().unwrap_or_default();
        let mut c = self.publisher(&x.conninfo, &x.name).map_err(|m| {
            Error::new(CONNECTION_FAILURE, format!("subscription \"{}\" could not connect to the publisher: {m}", x.name))
        })?;
        walrcv::password_check(&c, self.must_use_password(x))?;
        let mut opts = Vec::new();
        if let Some(f) = failover {
            opts.push(format!("FAILOVER {f}"));
        }
        if let Some(t) = two_phase {
            opts.push(format!("TWO_PHASE {t}"));
        }
        let cmd = format!("ALTER_REPLICATION_SLOT {} ( {} );", crate::catalog::quote_ident(&slot), opts.join(", "));
        let intr = crate::remote::interrupts(self.ctx);
        walrcv::exec(&mut c, &cmd, &intr).map_err(|e| match e {
            Fail::Remote { msg, .. } => {
                Error::new(code::PROTOCOL_VIOLATION, format!("could not alter replication slot \"{slot}\": {msg}"))
            }
            Fail::Interrupt(e) => e,
        })?;
        Ok(())
    }

    fn database_oid(&self) -> u32 {
        let db = &self.ctx.sess.database;
        self.ctx
            .sess
            .cluster
            .databases()
            .into_iter()
            .find(|d| &d.1 == db)
            .map_or(0, |d| d.0)
    }

    pub(super) fn subscription_named(&self, name: &str) -> Result<Subscription> {
        let db = self.database_oid();
        self.cat
            .subscriptions
            .values()
            .find(|s| s.name == name && s.dbid == db)
            .cloned()
            .ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("subscription \"{name}\" does not exist"),
                )
            })
    }

    /// CREATE SUBSCRIPTION.
    pub(super) fn create_subscription(&mut self, s: &pb::CreateSubscriptionStmt) -> Result<String> {
        let o = parse_options(
            &s.options,
            &[
                "connect",
                "enabled",
                "create_slot",
                "slot_name",
                "copy_data",
                "synchronous_commit",
                "binary",
                "streaming",
                "two_phase",
                "disable_on_error",
                "password_required",
                "run_as_owner",
                "failover",
                "origin",
            ],
        )?;
        if o.create_slot {
            super::prevent_in_block("CREATE SUBSCRIPTION ... WITH (create_slot = true)")?;
        }
        let owner = self.me();
        if !self
            .cat
            .has_privs_of_role(owner, PG_CREATE_SUBSCRIPTION, self.db_owner())
        {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                "permission denied to create subscription",
            )
            .detail("Only roles with privileges of the \"pg_create_subscription\" role may create subscriptions."));
        }
        self.require_database_priv(acl::CREATE)?;
        let superuser = self.is_superuser();
        if !o.password_required && !superuser {
            return Err(superuser_only());
        }
        let db = self.database_oid();
        if self
            .cat
            .subscriptions
            .values()
            .any(|x| x.name == s.subname && x.dbid == db)
        {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("subscription \"{}\" already exists", s.subname),
            ));
        }
        let slot_name = match &o.slot_name {
            Some(n) => n.clone(),
            None => Some(s.subname.clone()),
        };
        let synccommit = o.synchronous_commit.clone().unwrap_or_else(|| "off".into());
        check_conninfo(&s.conninfo, o.password_required && !superuser)?;
        let publications = publication_names(&s.publication)?;
        let mut rels = Vec::new();
        let mut twophase = if o.two_phase { 'p' } else { 'd' };
        if o.connect {
            let mut c = self.publisher(&s.conninfo, &s.subname).map_err(|m| {
                Error::new(
                    CONNECTION_FAILURE,
                    format!("subscription \"{}\" could not connect to the publisher: {m}", s.subname),
                )
            })?;
            walrcv::password_check(&c, o.password_required && !superuser)?;
            rels = self.published_rels(&mut c, &publications, &publications, if o.copy_data { 'i' } else { 'r' })?;
            if o.create_slot {
                let slot = slot_name.clone().unwrap_or_default();
                // (two_phase is enabled once every table is ready: now, when none is copied.)
                let two = o.two_phase && !o.copy_data && !rels.is_empty();
                let intr = crate::remote::interrupts(self.ctx);
                walrcv::create_slot(&mut c, &slot, false, two, o.failover, "nothing", &intr)
                    .map_err(|e| failed(e, code::PROTOCOL_VIOLATION))?;
                if two {
                    twophase = 'e';
                }
                self.notice(format!("created replication slot \"{slot}\" on publisher"));
            }
        }
        let oid = self.cat.alloc_oid();
        if !rels.is_empty() {
            self.cat.sub_rels.insert(oid, rels);
        }
        self.cat.subscriptions.insert(
            oid,
            Subscription {
                oid,
                dbid: db,
                skiplsn: 0,
                name: s.subname.clone(),
                owner,
                enabled: o.enabled,
                binary: o.binary,
                stream: o.streaming,
                twophase,
                disable_on_err: o.disable_on_error,
                password_required: o.password_required,
                run_as_owner: o.run_as_owner,
                failover: o.failover,
                conninfo: s.conninfo.clone(),
                slot_name,
                synccommit,
                publications,
                origin: o.origin.clone(),
            },
        );
        self.save()?;
        // The subscription's replication origin, its progress starting over.
        if let Some(r) = self.ctx.sess.db.repl() {
            let name = format!("pg_{oid}");
            let _ = r.origins.drop_origin(&name, true);
            r.origins.create(&name)?;
        }
        if let Some(w) = self.ctx.sess.cluster.logical_workers() {
            w.wake();
        }
        if !o.connect {
            self.ctx.notice(
                crate::session::warning(code::WARNING, "subscription was created, but is not connected")
                    .hint("To initiate replication, you must manually create the replication slot, enable the subscription, and refresh the subscription."),
            );
        }
        Ok("CREATE SUBSCRIPTION".into())
    }

    /// The owner checks every ALTER SUBSCRIPTION makes.
    fn alter_subscription_checks(&self, x: &Subscription) -> Result<()> {
        self.require_owner(x.owner, "subscription", &x.name)?;
        self.require_database_priv(acl::CREATE)?;
        if !x.password_required && !self.is_superuser() {
            return Err(superuser_only());
        }
        Ok(())
    }

    /// ALTER SUBSCRIPTION.
    pub(super) fn alter_subscription(&mut self, s: &pb::AlterSubscriptionStmt) -> Result<String> {
        use pb::AlterSubscriptionType as K;
        const TAG: &str = "ALTER SUBSCRIPTION";
        let mut x = self.subscription_named(&s.subname)?;
        self.alter_subscription_checks(&x)?;
        // (Table synchronization workers updating its tables' states hold it in AccessShareLock.)
        self.lock_object(crate::catalog::depend::SUBSCRIPTION, x.oid, crate::txn::locks::LockMode::AccessExclusive)?;
        let refresh_disabled = |what: &str| {
            Error::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "ALTER SUBSCRIPTION with refresh is not allowed for disabled subscriptions",
            )
            .hint(format!("Use {what} instead."))
        };
        let mut refresh: Option<(bool, Vec<String>)> = None;
        let mut alter_slot: (Option<bool>, Option<bool>) = (None, None);
        match K::try_from(s.kind).unwrap_or(K::Undefined) {
            K::AlterSubscriptionOptions => {
                let o = parse_options(
                    &s.options,
                    &[
                        "slot_name",
                        "synchronous_commit",
                        "binary",
                        "streaming",
                        "two_phase",
                        "disable_on_error",
                        "password_required",
                        "run_as_owner",
                        "failover",
                        "origin",
                    ],
                )?;
                if let Some(n) = &o.slot_name {
                    if x.enabled && n.is_none() {
                        return Err(Error::new(
                            code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                            "cannot set slot_name = NONE for enabled subscription",
                        ));
                    }
                    x.slot_name = n.clone();
                }
                if let Some(v) = &o.synchronous_commit {
                    x.synccommit = v.clone();
                }
                if o.given("binary") {
                    x.binary = o.binary;
                }
                if o.given("streaming") {
                    x.stream = o.streaming;
                }
                if o.given("disable_on_error") {
                    x.disable_on_err = o.disable_on_error;
                }
                if o.given("password_required") {
                    if !o.password_required && !self.is_superuser() {
                        return Err(superuser_only());
                    }
                    x.password_required = o.password_required;
                }
                if o.given("run_as_owner") {
                    x.run_as_owner = o.run_as_owner;
                }
                if o.given("two_phase") {
                    let update = !o.two_phase;
                    self.check_alter_sub_option(&x, "two_phase", update)?;
                    if update && o.given("slot_name") {
                        return Err(Error::new(
                            code::SYNTAX_ERROR,
                            "slot_name and two_phase cannot be altered at the same time",
                        ));
                    }
                    x.twophase = if o.two_phase { 'p' } else { 'd' };
                    if update {
                        alter_slot.1 = Some(false);
                    }
                }
                if o.given("failover") {
                    self.check_alter_sub_option(&x, "failover", true)?;
                    x.failover = o.failover;
                    alter_slot.0 = Some(o.failover);
                }
                if o.given("origin") {
                    x.origin = o.origin.clone();
                }
            }
            K::AlterSubscriptionEnabled => {
                let o = parse_options(&s.options, &["enabled"])?;
                if x.slot_name.is_none() && o.enabled {
                    return Err(Error::new(
                        code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "cannot enable subscription that does not have a slot name",
                    ));
                }
                x.enabled = o.enabled;
            }
            K::AlterSubscriptionConnection => {
                let owner_super = self.cat.is_superuser(x.owner);
                check_conninfo(&s.conninfo, x.password_required && !owner_super)?;
                x.conninfo = s.conninfo.clone();
            }
            K::AlterSubscriptionSetPublication => {
                let o = parse_options(&s.options, &["copy_data", "refresh"])?;
                x.publications = publication_names(&s.publication)?;
                if o.refresh {
                    if !x.enabled {
                        return Err(refresh_disabled(
                            "ALTER SUBSCRIPTION ... SET PUBLICATION ... WITH (refresh = false)",
                        ));
                    }
                    self.two_phase_copy(&x, o.copy_data, "ALTER SUBSCRIPTION ... SET PUBLICATION with refresh = false, or with copy_data = false, or use DROP/CREATE SUBSCRIPTION.")?;
                    super::prevent_in_block("ALTER SUBSCRIPTION with refresh")?;
                    refresh = Some((o.copy_data, x.publications.clone()));
                }
            }
            k @ (K::AlterSubscriptionAddPublication | K::AlterSubscriptionDropPublication) => {
                let add = k == K::AlterSubscriptionAddPublication;
                let o = parse_options(&s.options, &["refresh", "copy_data"])?;
                x.publications = self.merge_publications(&x, &s.publication, add)?;
                if o.refresh {
                    if !x.enabled {
                        return Err(refresh_disabled(if add {
                            "ALTER SUBSCRIPTION ... ADD PUBLICATION ... WITH (refresh = false)"
                        } else {
                            "ALTER SUBSCRIPTION ... DROP PUBLICATION ... WITH (refresh = false)"
                        }));
                    }
                    self.two_phase_copy(&x, o.copy_data, "ALTER SUBSCRIPTION ... SET PUBLICATION with refresh = false, or with copy_data = false, or use DROP/CREATE SUBSCRIPTION.")?;
                    super::prevent_in_block("ALTER SUBSCRIPTION with refresh")?;
                    refresh = Some((o.copy_data, if add { publication_names(&s.publication)? } else { Vec::new() }));
                }
            }
            K::AlterSubscriptionRefresh => {
                if !x.enabled {
                    return Err(Error::new(
                        code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "ALTER SUBSCRIPTION ... REFRESH is not allowed for disabled subscriptions",
                    ));
                }
                let o = parse_options(&s.options, &["copy_data"])?;
                self.two_phase_copy(&x, o.copy_data, "Use ALTER SUBSCRIPTION ... REFRESH with copy_data = false, or use DROP/CREATE SUBSCRIPTION.")?;
                super::prevent_in_block("ALTER SUBSCRIPTION ... REFRESH")?;
                refresh = Some((o.copy_data, Vec::new()));
            }
            K::AlterSubscriptionSkip => {
                let o = parse_options(&s.options, &["lsn"])?;
                x.skiplsn = o.lsn.unwrap_or(0);
            }
            K::Undefined => {}
        }
        if alter_slot != (None, None) {
            self.alter_subscription_slot(&x, alter_slot.0, alter_slot.1)?;
        }
        if let Some((copy_data, validate)) = &refresh {
            self.refresh_subscription(&x, *copy_data, validate)?;
        }
        self.cat.subscriptions.insert(x.oid, x);
        self.save()?;
        if let Some(w) = self.ctx.sess.cluster.logical_workers() {
            w.wake();
        }
        Ok(TAG.into())
    }

    /// CheckAlterSubOption for failover and two_phase.
    fn check_alter_sub_option(&self, x: &Subscription, option: &str, slot_update: bool) -> Result<()> {
        if x.enabled {
            return Err(Error::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("cannot set option \"{option}\" for enabled subscription"),
            ));
        }
        if slot_update {
            if x.slot_name.is_none() {
                return Err(Error::new(
                    code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!(
                        "cannot set option \"{option}\" for a subscription that does not have a slot name"
                    ),
                ));
            }
            super::prevent_in_block(&format!("ALTER SUBSCRIPTION ... SET ({option})"))?;
        }
        Ok(())
    }

    /// A refresh copying data can't run once two_phase is enabled.
    fn two_phase_copy(&self, x: &Subscription, copy_data: bool, hint: &str) -> Result<()> {
        if x.twophase == 'e' && copy_data {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                "ALTER SUBSCRIPTION with refresh and copy_data is not allowed when two_phase is enabled",
            )
            .hint(hint.to_string()));
        }
        Ok(())
    }

    /// merge_publications.
    fn merge_publications(&self, x: &Subscription, list: &[pb::Node], add: bool) -> Result<Vec<String>> {
        let mut old = x.publications.clone();
        let mut seen: Vec<String> = Vec::new();
        for n in list {
            let name = crate::sql::binder::ast::sval(n).unwrap_or_default().to_string();
            if seen.contains(&name) {
                return Err(Error::new(
                    code::DUPLICATE_OBJECT,
                    format!("publication name \"{name}\" used more than once"),
                ));
            }
            seen.push(name.clone());
            let found = old.contains(&name);
            if add {
                if found {
                    return Err(Error::new(
                        code::DUPLICATE_OBJECT,
                        format!(
                            "publication \"{name}\" is already in subscription \"{}\"",
                            x.name
                        ),
                    ));
                }
                old.push(name);
            } else if found {
                old.retain(|p| *p != name);
            } else {
                return Err(Error::new(
                    code::INVALID_OBJECT_DEFINITION,
                    format!(
                        "publication \"{name}\" is not in subscription \"{}\"",
                        x.name
                    ),
                ));
            }
        }
        if old.is_empty() {
            return Err(Error::new(
                code::INVALID_OBJECT_DEFINITION,
                "cannot drop all the publications from a subscription",
            ));
        }
        Ok(old)
    }

    /// DROP SUBSCRIPTION.
    pub(super) fn drop_subscription(&mut self, s: &pb::DropSubscriptionStmt) -> Result<String> {
        const TAG: &str = "DROP SUBSCRIPTION";
        let x = match self.subscription_named(&s.subname) {
            Ok(x) => x,
            Err(_) if s.missing_ok => {
                self.notice(format!(
                    "subscription \"{}\" does not exist, skipping",
                    s.subname
                ));
                return Ok(TAG.into());
            }
            Err(e) => return Err(e),
        };
        self.require_owner(x.owner, "subscription", &x.name)?;
        self.lock_object(crate::catalog::depend::SUBSCRIPTION, x.oid, crate::txn::locks::LockMode::AccessExclusive)?;
        if x.slot_name.is_some() {
            super::prevent_in_block("DROP SUBSCRIPTION")?;
        }
        // logicalrep_worker_stop: the worker ends first (it holds the slot and the origin).
        let workers = self.ctx.sess.cluster.logical_workers();
        if let Some(w) = &workers {
            w.stop(x.oid, true);
        }
        // The table synchronization slots of tables still being synchronized (not SYNCDONE or
        // READY: the worker dropped its slot then), and the subscription's slot.
        let syncing: Vec<String> = self
            .cat
            .sub_rels
            .get(&x.oid)
            .into_iter()
            .flatten()
            .filter(|r| !matches!(r.state, 'r' | 's'))
            .map(|r| format!("pg_{}_sync_{}_{}", x.oid, r.relid, crate::repl::SYSTEM_IDENTIFIER))
            .collect();
        if x.slot_name.is_some() || !syncing.is_empty() {
            let r = self.drop_remote_slots(&x, x.slot_name.as_deref(), &syncing);
            if r.is_err()
                && let Some(w) = &workers
            {
                w.release(x.oid);
            }
            r?;
        }
        if let Some(r) = self.ctx.sess.db.repl() {
            let _ = r.origins.drop_origin(&format!("pg_{}", x.oid), true);
            // (And the table synchronization origins of tables not ready yet.)
            for rel in self.cat.sub_rels.get(&x.oid).into_iter().flatten() {
                let _ = r.origins.drop_origin(&format!("pg_{}_{}", x.oid, rel.relid), true);
            }
        }
        self.cat.subscriptions.remove(&x.oid);
        self.cat.sub_rels.remove(&x.oid);
        self.cat.drop_comments(x.oid);
        self.save()?;
        Ok(TAG.into())
    }

    /// ReplicationSlotDropAtPubNode for DROP SUBSCRIPTION: table synchronization slots
    /// (missing ones are a LOG line), then the subscription's `slot`. Without a slot, a
    /// publisher it can't reach leaves the synchronization slots behind.
    fn drop_remote_slots(&self, x: &Subscription, slot: Option<&str>, syncing: &[String]) -> Result<()> {
        let conn = self.publisher(&x.conninfo, &x.name);
        let Some(slot) = slot else {
            let Ok(mut c) = conn else { return Ok(()) };
            walrcv::password_check(&c, self.must_use_password(x))?;
            return self.drop_sync_slots(&mut c, syncing);
        };
        let mut c = conn.map_err(|m| {
            Error::new(
                CONNECTION_FAILURE,
                format!("could not connect to publisher when attempting to drop replication slot \"{slot}\": {m}"),
            )
            .hint("Use ALTER SUBSCRIPTION ... DISABLE to disable the subscription, and then use ALTER SUBSCRIPTION ... SET (slot_name = NONE) to disassociate it from the slot.")
        })?;
        walrcv::password_check(&c, self.must_use_password(x))?;
        self.drop_sync_slots(&mut c, syncing)?;
        let intr = crate::remote::interrupts(self.ctx);
        match walrcv::drop_slot(&mut c, slot, &intr) {
            Ok(()) => {
                self.notice(format!("dropped replication slot \"{slot}\" on publisher"));
                Ok(())
            }
            Err(Fail::Interrupt(e)) => Err(e),
            Err(Fail::Remote { msg, .. }) => Err(Error::new(
                CONNECTION_FAILURE,
                format!("could not drop replication slot \"{slot}\" on publisher: {msg}"),
            )),
        }
    }

    /// Table synchronization slots dropped (missing_ok): a NOTICE for each dropped, a LOG
    /// line for one the publisher doesn't have.
    fn drop_sync_slots(&self, c: &mut Conn, slots: &[String]) -> Result<()> {
        let intr = crate::remote::interrupts(self.ctx);
        for s in slots {
            match walrcv::drop_slot(c, s, &intr) {
                Ok(()) => self.notice(format!("dropped replication slot \"{s}\" on publisher")),
                Err(Fail::Interrupt(e)) => return Err(e),
                Err(Fail::Remote { msg, sqlstate }) if sqlstate.as_deref() == Some("42704") => {
                    tracing::info!("could not drop replication slot \"{s}\" on publisher: {msg}");
                }
                Err(Fail::Remote { msg, .. }) => {
                    return Err(Error::new(
                        CONNECTION_FAILURE,
                        format!("could not drop replication slot \"{s}\" on publisher: {msg}"),
                    ));
                }
            }
        }
        Ok(())
    }

    /// ALTER SUBSCRIPTION ... RENAME TO (AlterObjectRename_internal).
    pub(super) fn rename_subscription(&mut self, old: &str, new: &str) -> Result<String> {
        let mut x = self.subscription_named(old)?;
        if !self.is_superuser() {
            self.require_owner(x.owner, "subscription", old)?;
            self.require_database_priv(acl::CREATE)?;
            if !x.password_required {
                return Err(superuser_only());
            }
        }
        if self.subscription_named(new).is_ok() {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("subscription \"{new}\" already exists"),
            ));
        }
        x.name = new.to_string();
        self.cat.subscriptions.insert(x.oid, x);
        self.save()?;
        Ok("ALTER SUBSCRIPTION".into())
    }

    /// ALTER SUBSCRIPTION ... OWNER TO (AlterSubscriptionOwner_internal).
    pub(super) fn owner_subscription(&mut self, name: &str, new: u32) -> Result<()> {
        let mut x = self.subscription_named(name)?;
        if x.owner == new {
            return Ok(());
        }
        self.require_owner(x.owner, "subscription", name)?;
        if !x.password_required && !self.is_superuser() {
            return Err(superuser_only());
        }
        if !self.cat.can_set_role(self.me(), new) {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("must be able to SET ROLE \"{}\"", self.role_name(new)),
            ));
        }
        self.require_database_priv(acl::CREATE)?;
        x.owner = new;
        self.cat.subscriptions.insert(x.oid, x);
        Ok(())
    }
}
