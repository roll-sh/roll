//! Event triggers (event_trigger.c): CREATE / ALTER / DROP EVENT TRIGGER, and firing them
//! around DDL commands: `ddl_command_start` before a command, `sql_drop` and
//! `ddl_command_end` after it, `table_rewrite` before a table is rewritten, with what
//! pg_event_trigger_ddl_commands(), pg_event_trigger_dropped_objects() and
//! pg_event_trigger_table_rewrite_oid() / _reason() report.

use super::Ddl;
use crate::catalog::depend::Addr;
use crate::catalog::{Catalog, EventTrigger, quote_ident};
use crate::error::{Error, Result, code};
use crate::exec::ExecCtx;
use crate::sql::binder::ast::{self, inner, names};
use crate::types::{DataType, Value};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::cell::RefCell;
use std::sync::Arc;

/// Command tags (cmdtaglist.h): (tag, event triggers fire for it, it can rewrite a table).
const TAGS: &[(&str, bool, bool)] = &[
    ("ALTER ACCESS METHOD", true, false),
    ("ALTER AGGREGATE", true, false),
    ("ALTER CAST", true, false),
    ("ALTER COLLATION", true, false),
    ("ALTER CONSTRAINT", true, false),
    ("ALTER CONVERSION", true, false),
    ("ALTER DATABASE", false, false),
    ("ALTER DEFAULT PRIVILEGES", true, false),
    ("ALTER DOMAIN", true, false),
    ("ALTER EVENT TRIGGER", false, false),
    ("ALTER EXTENSION", true, false),
    ("ALTER FOREIGN DATA WRAPPER", true, false),
    ("ALTER FOREIGN TABLE", true, false),
    ("ALTER FUNCTION", true, false),
    ("ALTER INDEX", true, false),
    ("ALTER LANGUAGE", true, false),
    ("ALTER LARGE OBJECT", true, false),
    ("ALTER MATERIALIZED VIEW", true, true),
    ("ALTER OPERATOR", true, false),
    ("ALTER OPERATOR CLASS", true, false),
    ("ALTER OPERATOR FAMILY", true, false),
    ("ALTER POLICY", true, false),
    ("ALTER PROCEDURE", true, false),
    ("ALTER PUBLICATION", true, false),
    ("ALTER ROLE", false, false),
    ("ALTER ROUTINE", true, false),
    ("ALTER RULE", true, false),
    ("ALTER SCHEMA", true, false),
    ("ALTER SEQUENCE", true, false),
    ("ALTER SERVER", true, false),
    ("ALTER STATISTICS", true, false),
    ("ALTER SUBSCRIPTION", true, false),
    ("ALTER SYSTEM", false, false),
    ("ALTER TABLE", true, true),
    ("ALTER TABLESPACE", false, false),
    ("ALTER TEXT SEARCH CONFIGURATION", true, false),
    ("ALTER TEXT SEARCH DICTIONARY", true, false),
    ("ALTER TEXT SEARCH PARSER", true, false),
    ("ALTER TEXT SEARCH TEMPLATE", true, false),
    ("ALTER TRANSFORM", true, false),
    ("ALTER TRIGGER", true, false),
    ("ALTER TYPE", true, true),
    ("ALTER USER MAPPING", true, false),
    ("ALTER VIEW", true, false),
    ("ANALYZE", false, false),
    ("BEGIN", false, false),
    ("CALL", false, false),
    ("CHECKPOINT", false, false),
    ("CLOSE", false, false),
    ("CLOSE CURSOR", false, false),
    ("CLOSE CURSOR ALL", false, false),
    ("CLUSTER", false, false),
    ("COMMENT", true, false),
    ("COMMIT", false, false),
    ("COMMIT PREPARED", false, false),
    ("COPY", false, false),
    ("COPY FROM", false, false),
    ("CREATE ACCESS METHOD", true, false),
    ("CREATE AGGREGATE", true, false),
    ("CREATE CAST", true, false),
    ("CREATE COLLATION", true, false),
    ("CREATE CONSTRAINT", true, false),
    ("CREATE CONVERSION", true, false),
    ("CREATE DATABASE", false, false),
    ("CREATE DOMAIN", true, false),
    ("CREATE EVENT TRIGGER", false, false),
    ("CREATE EXTENSION", true, false),
    ("CREATE FOREIGN DATA WRAPPER", true, false),
    ("CREATE FOREIGN TABLE", true, false),
    ("CREATE FUNCTION", true, false),
    ("CREATE INDEX", true, false),
    ("CREATE LANGUAGE", true, false),
    ("CREATE MATERIALIZED VIEW", true, false),
    ("CREATE OPERATOR", true, false),
    ("CREATE OPERATOR CLASS", true, false),
    ("CREATE OPERATOR FAMILY", true, false),
    ("CREATE POLICY", true, false),
    ("CREATE PROCEDURE", true, false),
    ("CREATE PUBLICATION", true, false),
    ("CREATE ROLE", false, false),
    ("CREATE ROUTINE", true, false),
    ("CREATE RULE", true, false),
    ("CREATE SCHEMA", true, false),
    ("CREATE SEQUENCE", true, false),
    ("CREATE SERVER", true, false),
    ("CREATE STATISTICS", true, false),
    ("CREATE SUBSCRIPTION", true, false),
    ("CREATE TABLE", true, false),
    ("CREATE TABLE AS", true, false),
    ("CREATE TABLESPACE", false, false),
    ("CREATE TEXT SEARCH CONFIGURATION", true, false),
    ("CREATE TEXT SEARCH DICTIONARY", true, false),
    ("CREATE TEXT SEARCH PARSER", true, false),
    ("CREATE TEXT SEARCH TEMPLATE", true, false),
    ("CREATE TRANSFORM", true, false),
    ("CREATE TRIGGER", true, false),
    ("CREATE TYPE", true, false),
    ("CREATE USER MAPPING", true, false),
    ("CREATE VIEW", true, false),
    ("DEALLOCATE", false, false),
    ("DEALLOCATE ALL", false, false),
    ("DECLARE CURSOR", false, false),
    ("DELETE", false, false),
    ("DISCARD", false, false),
    ("DISCARD ALL", false, false),
    ("DISCARD PLANS", false, false),
    ("DISCARD SEQUENCES", false, false),
    ("DISCARD TEMP", false, false),
    ("DO", false, false),
    ("DROP ACCESS METHOD", true, false),
    ("DROP AGGREGATE", true, false),
    ("DROP CAST", true, false),
    ("DROP COLLATION", true, false),
    ("DROP CONSTRAINT", true, false),
    ("DROP CONVERSION", true, false),
    ("DROP DATABASE", false, false),
    ("DROP DOMAIN", true, false),
    ("DROP EVENT TRIGGER", false, false),
    ("DROP EXTENSION", true, false),
    ("DROP FOREIGN DATA WRAPPER", true, false),
    ("DROP FOREIGN TABLE", true, false),
    ("DROP FUNCTION", true, false),
    ("DROP INDEX", true, false),
    ("DROP LANGUAGE", true, false),
    ("DROP MATERIALIZED VIEW", true, false),
    ("DROP OPERATOR", true, false),
    ("DROP OPERATOR CLASS", true, false),
    ("DROP OPERATOR FAMILY", true, false),
    ("DROP OWNED", true, false),
    ("DROP POLICY", true, false),
    ("DROP PROCEDURE", true, false),
    ("DROP PUBLICATION", true, false),
    ("DROP ROLE", false, false),
    ("DROP ROUTINE", true, false),
    ("DROP RULE", true, false),
    ("DROP SCHEMA", true, false),
    ("DROP SEQUENCE", true, false),
    ("DROP SERVER", true, false),
    ("DROP STATISTICS", true, false),
    ("DROP SUBSCRIPTION", true, false),
    ("DROP TABLE", true, false),
    ("DROP TABLESPACE", false, false),
    ("DROP TEXT SEARCH CONFIGURATION", true, false),
    ("DROP TEXT SEARCH DICTIONARY", true, false),
    ("DROP TEXT SEARCH PARSER", true, false),
    ("DROP TEXT SEARCH TEMPLATE", true, false),
    ("DROP TRANSFORM", true, false),
    ("DROP TRIGGER", true, false),
    ("DROP TYPE", true, false),
    ("DROP USER MAPPING", true, false),
    ("DROP VIEW", true, false),
    ("EXECUTE", false, false),
    ("EXPLAIN", false, false),
    ("FETCH", false, false),
    ("GRANT", true, false),
    ("GRANT ROLE", false, false),
    ("IMPORT FOREIGN SCHEMA", true, false),
    ("INSERT", false, false),
    ("LISTEN", false, false),
    ("LOAD", false, false),
    ("LOCK TABLE", false, false),
    ("LOGIN", true, false),
    ("MERGE", false, false),
    ("MOVE", false, false),
    ("NOTIFY", false, false),
    ("PREPARE", false, false),
    ("PREPARE TRANSACTION", false, false),
    ("REASSIGN OWNED", false, false),
    ("REFRESH MATERIALIZED VIEW", true, false),
    ("REINDEX", true, false),
    ("RELEASE", false, false),
    ("RESET", false, false),
    ("REVOKE", true, false),
    ("REVOKE ROLE", false, false),
    ("ROLLBACK", false, false),
    ("ROLLBACK PREPARED", false, false),
    ("SAVEPOINT", false, false),
    ("SECURITY LABEL", true, false),
    ("SELECT", false, false),
    ("SELECT FOR KEY SHARE", false, false),
    ("SELECT FOR NO KEY UPDATE", false, false),
    ("SELECT FOR SHARE", false, false),
    ("SELECT FOR UPDATE", false, false),
    ("SELECT INTO", true, false),
    ("SET", false, false),
    ("SET CONSTRAINTS", false, false),
    ("SHOW", false, false),
    ("START TRANSACTION", false, false),
    ("TRUNCATE TABLE", false, false),
    ("UNLISTEN", false, false),
    ("UPDATE", false, false),
    ("VACUUM", false, false),
];

fn tag_info(tag: &str) -> Option<(bool, bool)> {
    TAGS.iter().find(|t| t.0 == tag).map(|t| (t.1, t.2))
}

const EVENTS: [&str; 5] = [
    "ddl_command_start",
    "ddl_command_end",
    "sql_drop",
    "login",
    "table_rewrite",
];

/// The `EventTriggerSupportsObjectType` objects: no global objects, no event triggers.
fn supports_object(t: i32) -> bool {
    use pb::ObjectType as O;
    !matches!(
        O::try_from(t).unwrap_or(O::Undefined),
        O::ObjectDatabase | O::ObjectTablespace | O::ObjectRole | O::ObjectParameterAcl
            | O::ObjectEventTrigger
    )
}

/// The command tag of a statement event triggers fire for (`CreateCommandTag`, when the
/// command is one they support).
pub(crate) fn event_tag(node: &NodeEnum) -> Option<String> {
    use NodeEnum as N;
    let tag = match node {
        N::DropStmt(s) if !supports_object(s.remove_type) => return None,
        N::RenameStmt(s) if !supports_object(s.rename_type) => return None,
        N::AlterOwnerStmt(s) if !supports_object(s.object_type) => return None,
        N::AlterObjectSchemaStmt(s) if !supports_object(s.object_type) => return None,
        N::CommentStmt(s) if !supports_object(s.objtype) => return None,
        N::GrantStmt(s) if !supports_object(s.objtype) => return None,
        N::AlterOwnerStmt(s) => format!(
            "ALTER {}",
            super::object_type_word(
                pb::ObjectType::try_from(s.object_type).unwrap_or(pb::ObjectType::ObjectTable)
            )
        ),
        N::AlterFunctionStmt(s) => match pb::ObjectType::try_from(s.objtype) {
            Ok(pb::ObjectType::ObjectProcedure) => "ALTER PROCEDURE".into(),
            Ok(pb::ObjectType::ObjectRoutine) => "ALTER ROUTINE".into(),
            _ => "ALTER FUNCTION".into(),
        },
        N::AlterDomainStmt(_) => "ALTER DOMAIN".into(),
        N::CreateStatsStmt(_) => "CREATE STATISTICS".into(),
        N::AlterStatsStmt(_) => "ALTER STATISTICS".into(),
        N::ReindexStmt(_) => "REINDEX".into(),
        N::RuleStmt(_) => "CREATE RULE".into(),
        N::AlterDefaultPrivilegesStmt(_) => "ALTER DEFAULT PRIVILEGES".into(),
        N::SecLabelStmt(_) => "SECURITY LABEL".into(),
        N::CreateForeignTableStmt(_) => "CREATE FOREIGN TABLE".into(),
        N::ImportForeignSchemaStmt(_) => "IMPORT FOREIGN SCHEMA".into(),
        N::AlterExtensionStmt(_) | N::AlterExtensionContentsStmt(_) => "ALTER EXTENSION".into(),
        N::CreatePublicationStmt(_) => "CREATE PUBLICATION".into(),
        N::AlterPublicationStmt(_) => "ALTER PUBLICATION".into(),
        N::CreateSubscriptionStmt(_) => "CREATE SUBSCRIPTION".into(),
        N::AlterSubscriptionStmt(_) => "ALTER SUBSCRIPTION".into(),
        N::DropSubscriptionStmt(_) => "DROP SUBSCRIPTION".into(),
        N::AlterTableMoveAllStmt(_) => "ALTER TABLE".into(),
        N::AlterTsdictionaryStmt(_) => "ALTER TEXT SEARCH DICTIONARY".into(),
        N::RefreshMatViewStmt(_) => "REFRESH MATERIALIZED VIEW".into(),
        N::DropOwnedStmt(_) => "DROP OWNED".into(),
        N::ReassignOwnedStmt(_) => "REASSIGN OWNED".into(),
        N::CreateOpClassStmt(_) => "CREATE OPERATOR CLASS".into(),
        n => super::write_command(n)?,
    };
    match tag_info(&tag) {
        Some((true, _)) => Some(tag),
        _ => None,
    }
}

// ── the triggers ──

/// The event triggers firing for `event` (in name order) under session_replication_role,
/// with the command's tag `tag` passing their WHEN TAG IN filters.
fn firing(ctx: &ExecCtx, cat: &Catalog, event: &str, tag: Option<&str>) -> Vec<Arc<EventTrigger>> {
    let replica = ctx
        .sess
        .settings
        .borrow()
        .get_raw("session_replication_role")
        .as_deref()
        == Some("replica");
    let mut out: Vec<Arc<EventTrigger>> = cat
        .event_triggers
        .values()
        .filter(|e| e.event == event)
        .filter(|e| match e.enabled {
            'D' => false,
            'A' => true,
            'R' => replica,
            _ => !replica,
        })
        .filter(|e| match (&e.tags, tag) {
            (Some(t), Some(tag)) => t.iter().any(|x| x == tag),
            _ => true,
        })
        .cloned()
        .collect();
    out.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    out
}

/// Whether event triggers can fire now (the event_triggers setting).
fn enabled(ctx: &ExecCtx) -> bool {
    !matches!(
        ctx.sess
            .settings
            .borrow()
            .get_raw("event_triggers")
            .as_deref(),
        Some("off" | "false" | "no" | "0")
    )
}

/// Run the trigger functions of `trigs` for `event`.
fn run(ctx: &ExecCtx, trigs: &[Arc<EventTrigger>], event: &str, tag: &str) -> Result<()> {
    for e in trigs {
        let cat = ctx.catalog();
        let Some(f) = cat.functions.get(&e.function).cloned() else {
            continue;
        };
        crate::plpgsql::call_event_trigger(ctx, &f, event, tag)?;
    }
    Ok(())
}

/// A dropped object as pg_event_trigger_dropped_objects() reports it.
#[derive(Clone, Debug)]
pub struct Dropped {
    pub addr: Addr,
    pub original: bool,
    pub normal: bool,
    pub temporary: bool,
    pub kind: String,
    pub schema: Option<String>,
    pub name: Option<String>,
    pub identity: Option<String>,
    pub names: Vec<String>,
    pub args: Vec<String>,
}

/// A command pg_event_trigger_ddl_commands() reports.
#[derive(Clone, Debug)]
pub struct Command {
    pub addr: Addr,
    pub tag: String,
    pub kind: String,
    pub schema: Option<String>,
    pub identity: Option<String>,
}

/// What a command in progress collects for its event triggers.
#[derive(Default)]
struct State {
    dropped: Vec<Dropped>,
    commands: Vec<Command>,
    in_sql_drop: bool,
    in_end: bool,
    rewrite: Option<(u32, i32)>,
    /// The statement's command tag.
    tag: String,
}

thread_local! {
    /// The states of the commands running, innermost last.
    static STATES: RefCell<Vec<State>> = const { RefCell::new(Vec::new()) };
}

/// A command event triggers fire around (see `begin`).
pub(crate) struct Begun {
    tag: String,
    depth: usize,
    before: Arc<Catalog>,
}

/// Before a DDL command: its ddl_command_start triggers, and a state to collect what
/// sql_drop / ddl_command_end / table_rewrite triggers will see. None: no event triggers
/// concern it.
pub(crate) fn begin(ctx: &ExecCtx, node: &NodeEnum) -> Result<Option<Begun>> {
    let cat = ctx.catalog();
    if cat.event_triggers.is_empty() || !enabled(ctx) {
        return Ok(None);
    }
    let Some(tag) = event_tag(node) else {
        return Ok(None);
    };
    let start = firing(ctx, &cat, "ddl_command_start", Some(&tag));
    run(ctx, &start, "ddl_command_start", &tag)?;
    let depth = STATES.with_borrow_mut(|s| {
        s.push(State {
            tag: tag.clone(),
            ..State::default()
        });
        s.len()
    });
    Ok(Some(Begun {
        tag,
        depth,
        before: ctx.catalog(),
    }))
}

/// The command failed: forget its state.
pub(crate) fn abort(b: Begun) {
    STATES.with_borrow_mut(|s| s.truncate(b.depth - 1));
}

/// After a DDL command: its sql_drop triggers (when it dropped something), then its
/// ddl_command_end ones.
pub(crate) fn finish(ctx: &ExecCtx, b: Begun, node: &NodeEnum) -> Result<()> {
    let r = (|| {
        let cat = ctx.catalog();
        let dropped = STATES.with_borrow(|s| s.get(b.depth - 1).is_some_and(|st| !st.dropped.is_empty()));
        if dropped {
            let trigs = firing(ctx, &cat, "sql_drop", Some(&b.tag));
            if !trigs.is_empty() {
                set(b.depth, |st| st.in_sql_drop = true);
                run(ctx, &trigs, "sql_drop", &b.tag)?;
                set(b.depth, |st| st.in_sql_drop = false);
            }
        }
        let trigs = firing(ctx, &cat, "ddl_command_end", Some(&b.tag));
        if !trigs.is_empty() {
            let cmds = commands(ctx, node, &b.tag, &b.before, &cat);
            set(b.depth, |st| {
                st.commands.extend(cmds);
                st.in_end = true;
            });
            run(ctx, &trigs, "ddl_command_end", &b.tag)?;
        }
        Ok(())
    })();
    STATES.with_borrow_mut(|s| s.truncate(b.depth - 1));
    r
}

fn set(depth: usize, f: impl FnOnce(&mut State)) {
    STATES.with_borrow_mut(|s| {
        if let Some(st) = s.get_mut(depth - 1) {
            f(st);
        }
    });
}

/// `EventTriggerSQLDropAddObject` for the objects a DROP (or a command dropping parts)
/// deletes, as `drop_objects` found them: (object, DEPFLAG_* flags) in deletion order.
pub(crate) fn note_dropped(cat: &Catalog, targets: &[(Addr, u16)]) {
    use crate::catalog::depend::{flag, *};
    if STATES.with_borrow(|s| s.is_empty()) {
        return;
    }
    let mut list = Vec::new();
    for &(addr, flags) in targets {
        let (class, oid, sub) = addr;
        if class == EVENT_TRIGGER || class == 1260 || class == 1262 || class == 1213 {
            continue;
        }
        let Ok(id) = crate::catalog::identify::identify(cat, addr) else {
            continue;
        };
        // The schema the object is in (for triggers / policies / defaults / constraints:
        // their table's).
        let rel_schema = |rel: u32| {
            cat.tables
                .get(&rel)
                .map(|t| t.schema)
                .or_else(|| cat.views.get(&rel).map(|v| v.schema))
                .or_else(|| cat.sequences.get(&rel).map(|q| q.schema))
                .or_else(|| {
                    cat.indexes
                        .get(&rel)
                        .and_then(|ix| cat.index_table(ix.table).map(|t| t.schema))
                })
        };
        let owner_table = match class {
            TRIGGER => cat.triggers.get(&oid).map(|t| t.table),
            POLICY => cat.policies.get(&oid).map(|p| p.table),
            REWRITE => cat.rules.get(&oid).map(|r| r.table),
            ATTRDEF => cat
                .tables
                .values()
                .find(|t| t.columns.iter().any(|c| c.def_oid == oid && (c.default.is_some() || c.generated.is_some())))
                .map(|t| t.oid),
            CONSTRAINT => cat
                .tables
                .values()
                .find(|t| t.constraints.iter().any(|c| c.oid == oid))
                .map(|t| t.oid),
            REL => Some(oid),
            _ => None,
        };
        let real_schema: Option<String> = match class {
            NAMESPACE => None,
            _ => owner_table
                .and_then(rel_schema)
                .map(|s| cat.schema_name(s).to_string())
                .or_else(|| id.schema.clone())
                // (Foreign keys' internal triggers: their table's, first in the address.)
                .or_else(|| {
                    (matches!(class, TRIGGER | POLICY | REWRITE | CONSTRAINT | ATTRDEF))
                        .then(|| id.names.as_ref().filter(|n| n.len() >= 3)?.first().cloned())
                        .flatten()
                }),
        };
        let temporary = match class {
            NAMESPACE => cat.schema_name(oid).starts_with("pg_temp"),
            _ => real_schema.as_deref().is_some_and(|s| s.starts_with("pg_temp")),
        };
        let schema = if temporary && class != NAMESPACE {
            Some("pg_temp".to_string())
        } else {
            real_schema.clone()
        };
        // Only objects with names unique in their schema (and not sub-objects) are named.
        let name = match class {
            NAMESPACE => Some(cat.schema_name(oid).to_string()),
            REL | TYPE if sub == 0 => id.name.clone().or_else(|| {
                id.names.as_ref().and_then(|n| n.last().cloned())
            }),
            CONVERSION | STATEXT | TSCONFIG | TSDICT | TSPARSER | TSTEMPLATE => id.name.clone(),
            _ => None,
        };
        let mut identity = id.identity.clone();
        let mut names = id.names.clone().unwrap_or_default();
        if temporary
            && class != NAMESPACE
            && let Some(real) = &real_schema
        {
            // A temporary object is named by its schema's alias.
            (identity, names) = temp_alias(identity, names, real);
        }
        // TYPE identities read from the type's name in this layout.
        let name = if class == TYPE {
            cat.user_type_name(oid).map(|(_, n, array)| if array { cat.array_typname(oid, n) } else { n.to_string() })
        } else {
            name
        };
        list.push(Dropped {
            addr,
            original: flags & flag::ORIGINAL != 0,
            normal: flags & (flag::NORMAL | flag::REVERSE) != 0,
            temporary,
            kind: id.kind.clone(),
            schema,
            name,
            identity,
            names,
            args: id.args.clone().unwrap_or_default(),
        });
    }
    STATES.with_borrow_mut(|s| {
        if let Some(st) = s.last_mut() {
            // (Each one goes at the head of the list.)
            for d in list {
                st.dropped.insert(0, d);
            }
        }
    });
}

/// As a session starts: its login event triggers.
pub(crate) fn login(ctx: &ExecCtx) -> Result<()> {
    let cat = ctx.catalog();
    if !enabled(ctx) {
        return Ok(());
    }
    let trigs = firing(ctx, &cat, "login", None);
    run(ctx, &trigs, "login", "login")
}

/// Before ALTER TABLE (or ALTER TYPE) rewrites table `oid`: its table_rewrite triggers
/// (`reason`: AT_REWRITE_* bits).
pub(crate) fn table_rewrite(ctx: &ExecCtx, oid: u32, reason: i32) -> Result<()> {
    let cat = ctx.catalog();
    if cat.event_triggers.is_empty() || !enabled(ctx) {
        return Ok(());
    }
    let Some(tag) = STATES.with_borrow(|s| s.last().map(|st| st.tag.clone())) else {
        return Ok(());
    };
    let trigs = firing(ctx, &cat, "table_rewrite", Some(&tag));
    if trigs.is_empty() {
        return Ok(());
    }
    let depth = STATES.with_borrow(|s| s.len());
    set(depth, |st| st.rewrite = Some((oid, reason)));
    let r = run(ctx, &trigs, "table_rewrite", &tag);
    set(depth, |st| st.rewrite = None);
    r
}

fn protocol(msg: String) -> Error {
    Error::new("39P03", msg)
}

/// pg_event_trigger_dropped_objects()'s rows.
pub(crate) fn dropped_objects() -> Result<Vec<Vec<Value>>> {
    let list = STATES.with_borrow(|s| {
        s.last()
            .filter(|st| st.in_sql_drop)
            .map(|st| st.dropped.clone())
    });
    let Some(list) = list else {
        return Err(protocol(
            "pg_event_trigger_dropped_objects() can only be called in a sql_drop event trigger function"
                .into(),
        ));
    };
    let t = |s: &Option<String>| s.as_ref().map_or(Value::Null, |s| Value::Text(Arc::from(s.as_str())));
    let arr = |v: &[String]| {
        Value::Array(Arc::new(crate::types::array::ArrayValue::from_vec(
            DataType::Text,
            v.iter().map(|x| Value::Text(Arc::from(x.as_str()))).collect(),
        )))
    };
    Ok(list
        .iter()
        .map(|d| {
            vec![
                Value::Oid(d.addr.0),
                Value::Oid(d.addr.1),
                Value::Int4(d.addr.2),
                Value::Bool(d.original),
                Value::Bool(d.normal),
                Value::Bool(d.temporary),
                Value::Text(Arc::from(d.kind.as_str())),
                t(&d.schema),
                t(&d.name),
                t(&d.identity),
                arr(&d.names),
                arr(&d.args),
            ]
        })
        .collect())
}

/// pg_event_trigger_ddl_commands()'s rows.
pub(crate) fn ddl_commands() -> Result<Vec<Vec<Value>>> {
    let list = STATES.with_borrow(|s| s.last().filter(|st| st.in_end).map(|st| st.commands.clone()));
    let Some(list) = list else {
        return Err(protocol(
            "pg_event_trigger_ddl_commands() can only be called in an event trigger function".into(),
        ));
    };
    let t = |s: &Option<String>| s.as_ref().map_or(Value::Null, |s| Value::Text(Arc::from(s.as_str())));
    Ok(list
        .iter()
        .map(|c| {
            vec![
                Value::Oid(c.addr.0),
                Value::Oid(c.addr.1),
                Value::Int4(c.addr.2),
                Value::Text(Arc::from(c.tag.as_str())),
                Value::Text(Arc::from(c.kind.as_str())),
                t(&c.schema),
                t(&c.identity),
                Value::Bool(false),
                Value::Null,
            ]
        })
        .collect())
}

/// pg_event_trigger_table_rewrite_oid() / _reason().
pub(crate) fn table_rewrite_info(func: &str) -> Result<(u32, i32)> {
    STATES
        .with_borrow(|s| s.last().and_then(|st| st.rewrite))
        .ok_or_else(|| {
            protocol(format!(
                "{func} can only be called in a table_rewrite event trigger function"
            ))
        })
}

// ── what a command did, for ddl_command_end ──

fn command(cat: &Catalog, addr: Addr, tag: &str) -> Option<Command> {
    let id = crate::catalog::identify::identify(cat, addr).ok()?;
    let mut identity = id.identity;
    let mut schema = id.schema;
    if let Some(real) = schema.clone().filter(|s| s.starts_with("pg_temp")) {
        (identity, _) = temp_alias(identity, Vec::new(), &real);
        schema = Some("pg_temp".into());
    } else if let Some(i) = &identity
        && let Some(at) = i.find("pg_temp_")
        && let Some(dot) = i[at..].find('.')
        && i[at + 8..at + dot].bytes().all(|b| b.is_ascii_digit())
    {
        // A trigger or policy of a temporary table: "x on pg_temp.t".
        identity = Some(format!("{}pg_temp{}", &i[..at], &i[at + dot..]));
    }
    Some(Command {
        addr,
        tag: tag.to_string(),
        kind: id.kind,
        schema,
        identity,
    })
}

/// An identity and address names with the session's temporary schema shown as `pg_temp`.
fn temp_alias(
    identity: Option<String>,
    mut names: Vec<String>,
    real: &str,
) -> (Option<String>, Vec<String>) {
    let q = quote_ident(real);
    let identity = identity.map(|i| i.replace(&format!("{q}."), "pg_temp."));
    for n in &mut names {
        if n == real {
            *n = "pg_temp".into();
        } else if let Some(rest) = n.strip_prefix(&format!("{q}.")) {
            *n = format!("pg_temp.{rest}");
        }
    }
    (identity, names)
}

/// The relation a RangeVar names in `cat`.
fn rel_oid(ctx: &ExecCtx, cat: &Catalog, rv: &pb::RangeVar) -> Option<u32> {
    let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
    cat.find(schema, &rv.relname, &ctx.search_path).map(|r| r.oid())
}

/// The commands a statement ran, as `EventTriggerCollectSimpleCommand` & co. record them.
fn commands(ctx: &ExecCtx, node: &NodeEnum, tag: &str, before: &Catalog, after: &Catalog) -> Vec<Command> {
    use crate::catalog::depend::*;
    use NodeEnum as N;
    let mut out = Vec::new();
    let new_seqs = |owner: Option<u32>| -> Vec<u32> {
        let mut v: Vec<u32> = after
            .sequences
            .values()
            .filter(|q| !before.sequences.contains_key(&q.oid))
            .filter(|q| owner.is_none_or(|o| q.owned_by.map(|x| x.0) == Some(o)))
            .map(|q| q.oid)
            .collect();
        v.sort_unstable();
        v
    };
    let create_table = |out: &mut Vec<Command>, t: u32| {
        let seqs = new_seqs(Some(t));
        for &q in &seqs {
            out.extend(command(after, (REL, q, 0), "CREATE SEQUENCE"));
        }
        out.extend(command(after, (REL, t, 0), "CREATE TABLE"));
        let is_partition = after.tables.get(&t).is_some_and(|x| x.partbound.is_some());
        if !is_partition && let Some(tb) = after.tables.get(&t) {
            let mut ixs: Vec<u32> = tb
                .indexes
                .iter()
                .chain(&tb.pkey_index)
                .copied()
                .filter(|i| !before.indexes.contains_key(i))
                .collect();
            // Only its constraints' indexes (a CREATE SCHEMA's later CREATE INDEX is its own).
            ixs.retain(|i| {
                Some(*i) == tb.pkey_index
                    || tb.constraints.iter().any(|c| {
                        use crate::catalog::ConstraintKind as K;
                        matches!(c.kind, K::PrimaryKey { index } | K::Unique { index } | K::Exclusion { index, .. } if index == *i)
                    })
            });
            ixs.sort_unstable();
            ixs.dedup();
            for i in ixs {
                out.extend(command(after, (REL, i, 0), "CREATE INDEX"));
            }
        }
        for &q in &seqs {
            out.extend(command(after, (REL, q, 0), "ALTER SEQUENCE"));
        }
        if after.tables.get(&t).is_some_and(|x| {
            x.constraints
                .iter()
                .any(|c| matches!(c.kind, crate::catalog::ConstraintKind::ForeignKey { .. }))
        }) {
            out.extend(command(after, (REL, t, 0), "ALTER TABLE"));
        }
    };
    match node {
        N::DropStmt(_) | N::DropOwnedStmt(_) => {}
        N::CreateStmt(s) => {
            if let Some(t) = s.relation.as_ref().and_then(|rv| rel_oid(ctx, after, rv)) {
                create_table(&mut out, t);
            }
        }
        N::CreateSchemaStmt(s) => {
            if let Some(sc) = after.schema_by_name(&s.schemaname) {
                out.extend(command(after, (NAMESPACE, sc.oid, 0), "CREATE SCHEMA"));
            }
            // transformCreateSchemaStmt runs them grouped: sequences, tables, views,
            // indexes, triggers, grants.
            let rank = |e: &&pb::Node| match inner(e) {
                Some(N::CreateSeqStmt(_)) => 0,
                Some(N::CreateStmt(_)) => 1,
                Some(N::ViewStmt(_)) => 2,
                Some(N::IndexStmt(_)) => 3,
                Some(N::CreateTrigStmt(_)) => 4,
                Some(N::GrantStmt(_)) => 5,
                _ => 6,
            };
            let mut elts: Vec<&pb::Node> = s.schema_elts.iter().collect();
            elts.sort_by_key(rank);
            for e in elts {
                match inner(e) {
                    Some(N::CreateStmt(c)) => {
                        let rel = c.relation.as_ref().map(|rv| {
                            let mut rv = rv.clone();
                            if rv.schemaname.is_empty() {
                                rv.schemaname = s.schemaname.clone();
                            }
                            rv
                        });
                        if let Some(t) = rel.and_then(|rv| rel_oid(ctx, after, &rv)) {
                            create_table(&mut out, t);
                        }
                    }
                    Some(n) => {
                        if let Some(sc) = after.schema_by_name(&s.schemaname) {
                            let tag = super::write_command(n).unwrap_or_default();
                            let found = new_object_in(before, after, sc.oid, n);
                            if let Some(a) = found {
                                out.extend(command(after, a, &tag));
                            }
                        }
                    }
                    None => {}
                }
            }
        }
        N::IndexStmt(s) => {
            if let Some(t) = s.relation.as_ref().and_then(|rv| rel_oid(ctx, after, rv)) {
                let mut ixs: Vec<u32> = after
                    .indexes
                    .values()
                    .filter(|ix| ix.table == t && !before.indexes.contains_key(&ix.oid))
                    .map(|ix| ix.oid)
                    .collect();
                ixs.sort_unstable();
                if let Some(i) = ixs.first() {
                    out.extend(command(after, (REL, *i, 0), "CREATE INDEX"));
                }
            }
        }
        N::AlterTableStmt(s) => {
            if let Some(t) = s.relation.as_ref().and_then(|rv| rel_oid(ctx, after, rv)) {
                // A type change of an identity column alters its sequence too.
                let identity_seq = after.tables.get(&t).and_then(|tb| {
                    s.cmds.iter().find_map(|c| match inner(c) {
                        Some(N::AlterTableCmd(c))
                            if c.subtype == pb::AlterTableType::AtAlterColumnType as i32 =>
                        {
                            let col = tb.columns.iter().find(|x| x.name == c.name)?;
                            col.identity.as_ref().and(col.seq)
                        }
                        _ => None,
                    })
                });
                let drops_identity = s.cmds.iter().any(|c| {
                    matches!(inner(c), Some(N::AlterTableCmd(c)) if c.subtype == pb::AlterTableType::AtDropIdentity as i32)
                });
                if let (Some(q), false) = (identity_seq, drops_identity) {
                    out.extend(command(after, (REL, q, 0), "ALTER SEQUENCE"));
                }
                out.extend(command(after, (REL, t, 0), tag));
            }
        }
        N::CreateFunctionStmt(s) => {
            // A new function, or the one CREATE OR REPLACE changed.
            let fname = names(&s.funcname).pop().unwrap_or_default();
            let f = after
                .functions
                .values()
                .filter(|f| f.name == fname)
                .find(|f| before.functions.get(&f.oid).is_none_or(|b| !Arc::ptr_eq(b, f)));
            if let Some(f) = f {
                out.extend(command(after, (PROC, f.oid, 0), tag));
            }
        }
        N::ReindexStmt(s) => {
            for i in reindexed(ctx, after, s) {
                out.extend(command(after, (REL, i, 0), "REINDEX"));
            }
        }
        N::CreateOpClassStmt(s) if s.opfamilyname.is_empty() => {
            if let Some(a) = new_of(before, after, OPFAMILY) {
                out.extend(command(after, a, "CREATE OPERATOR FAMILY"));
            }
            if let Some(a) = new_of(before, after, OPCLASS) {
                out.extend(command(after, a, "CREATE OPERATOR CLASS"));
            }
        }
        N::CreatePolicyStmt(s) => {
            if let Some(t) = s.table.as_ref().and_then(|rv| rel_oid(ctx, after, rv))
                && let Some(p) = after
                    .policies
                    .values()
                    .find(|p| p.table == t && p.name == s.policy_name)
            {
                out.extend(command(after, (POLICY, p.oid, 0), tag));
            }
        }
        N::AlterPolicyStmt(s) => {
            if let Some(t) = s.table.as_ref().and_then(|rv| rel_oid(ctx, after, rv))
                && let Some(p) = after
                    .policies
                    .values()
                    .find(|p| p.table == t && p.name == s.policy_name)
            {
                out.extend(command(after, (POLICY, p.oid, 0), tag));
            }
        }
        _ => {
            // Whatever it made: the new object (the first, by oid).
            if let Some(a) = any_new(before, after) {
                out.extend(command(after, a, tag));
            }
        }
    }
    out
}

/// The indexes a REINDEX rebuilds (leaf indexes: a partitioned one's partitions').
fn reindexed(ctx: &ExecCtx, cat: &Catalog, s: &pb::ReindexStmt) -> Vec<u32> {
    use pb::ReindexObjectType as K;
    let leaves = |ix: u32| -> Vec<u32> {
        let mut out = Vec::new();
        let mut stack = vec![ix];
        while let Some(i) = stack.pop() {
            let kids: Vec<u32> = cat.indexes.values().filter(|c| c.parent == i).map(|c| c.oid).collect();
            if kids.is_empty() {
                if cat.indexes.get(&i).is_some_and(|x| cat.tables.get(&x.table).is_none_or(|t| t.partkey.is_none())) {
                    out.push(i);
                }
            } else {
                stack.extend(kids);
            }
        }
        out.sort_unstable();
        out
    };
    let table_indexes = |t: u32| -> Vec<u32> {
        let mut out = Vec::new();
        let mut tables = vec![t];
        while let Some(x) = tables.pop() {
            if let Some(tb) = cat.tables.get(&x) {
                if tb.partkey.is_none() {
                    let mut ixs: Vec<u32> = tb.indexes.iter().chain(&tb.pkey_index).copied().collect();
                    ixs.sort_unstable();
                    ixs.dedup();
                    out.extend(ixs);
                }
                tables.extend(cat.children(x));
            }
        }
        out
    };
    match K::try_from(s.kind).unwrap_or(K::Undefined) {
        K::ReindexObjectIndex => s
            .relation
            .as_ref()
            .and_then(|rv| rel_oid(ctx, cat, rv))
            .map(leaves)
            .unwrap_or_default(),
        K::ReindexObjectTable => s
            .relation
            .as_ref()
            .and_then(|rv| rel_oid(ctx, cat, rv))
            .map(table_indexes)
            .unwrap_or_default(),
        K::ReindexObjectSchema => {
            let Some(sc) = cat.schema_by_name(&s.name) else {
                return Vec::new();
            };
            let mut out = Vec::new();
            for t in cat.tables.values().filter(|t| t.schema == sc.oid) {
                out.extend(table_indexes(t.oid));
            }
            out
        }
        _ => Vec::new(),
    }
}

/// The first object of class `class` the statement made.
fn new_of(before: &Catalog, after: &Catalog, class: u32) -> Option<Addr> {
    use crate::catalog::depend::*;
    let oids: Vec<u32> = match class {
        OPFAMILY => after.opfamilies.keys().filter(|o| !before.opfamilies.contains_key(o)).copied().collect(),
        OPCLASS => after.opclasses.keys().filter(|o| !before.opclasses.contains_key(o)).copied().collect(),
        _ => Vec::new(),
    };
    oids.into_iter().min().map(|o| (class, o, 0))
}

/// The object a schema element made in schema `schema`.
fn new_object_in(before: &Catalog, after: &Catalog, schema: u32, n: &NodeEnum) -> Option<Addr> {
    use crate::catalog::depend::*;
    match n {
        NodeEnum::IndexStmt(s) => after
            .indexes
            .values()
            .find(|ix| ix.name == s.idxname && !before.indexes.contains_key(&ix.oid))
            .map(|ix| (REL, ix.oid, 0)),
        NodeEnum::ViewStmt(_) | NodeEnum::CreateSeqStmt(_) => {
            let mut v: Vec<u32> = after
                .views
                .values()
                .filter(|x| x.schema == schema)
                .map(|x| x.oid)
                .chain(after.sequences.values().filter(|x| x.schema == schema).map(|x| x.oid))
                .filter(|o| !before.views.contains_key(o) && !before.sequences.contains_key(o))
                .collect();
            v.sort_unstable();
            v.first().map(|o| (REL, *o, 0))
        }
        _ => any_new(before, after),
    }
}

/// The first new object a statement made, by oid.
fn any_new(before: &Catalog, after: &Catalog) -> Option<Addr> {
    use crate::catalog::depend::*;
    let mut found: Vec<Addr> = Vec::new();
    let mut add = |class: u32, oids: Vec<u32>| found.extend(oids.into_iter().map(|o| (class, o, 0)));
    add(REL, after.tables.keys().filter(|o| !before.tables.contains_key(o)).copied().collect());
    add(REL, after.views.keys().filter(|o| !before.views.contains_key(o)).copied().collect());
    add(REL, after.sequences.keys().filter(|o| !before.sequences.contains_key(o)).copied().collect());
    add(REL, after.indexes.keys().filter(|o| !before.indexes.contains_key(o)).copied().collect());
    add(PROC, after.functions.keys().filter(|o| !before.functions.contains_key(o)).copied().collect());
    add(NAMESPACE, after.schemas.keys().filter(|o| !before.schemas.contains_key(o)).copied().collect());
    add(TRIGGER, after.triggers.keys().filter(|o| !before.triggers.contains_key(o)).copied().collect());
    add(POLICY, after.policies.keys().filter(|o| !before.policies.contains_key(o)).copied().collect());
    add(REWRITE, after.rules.keys().filter(|o| !before.rules.contains_key(o)).copied().collect());
    add(TYPE, after.enums.keys().chain(after.composites.keys()).chain(after.domains.keys()).chain(after.ranges.keys())
        .filter(|o| !before.enums.contains_key(o) && !before.composites.contains_key(o) && !before.domains.contains_key(o) && !before.ranges.contains_key(o)).copied().collect());
    add(OPERATOR, after.operators.keys().filter(|o| !before.operators.contains_key(o)).copied().collect());
    add(STATEXT, after.statistics.keys().filter(|o| !before.statistics.contains_key(o)).copied().collect());
    found.into_iter().min_by_key(|a| a.1)
}

// ── DDL ──

impl Ddl<'_, '_> {
    pub(super) fn create_event_trigger(&mut self, s: &pb::CreateEventTrigStmt) -> Result<String> {
        if !self.is_superuser() {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied to create event trigger \"{}\"", s.trigname),
            )
            .hint("Must be superuser to create an event trigger."));
        }
        if !EVENTS.contains(&s.eventname.as_str()) {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                format!("unrecognized event name \"{}\"", s.eventname),
            ));
        }
        let mut tags: Option<Vec<String>> = None;
        for w in &s.whenclause {
            let Some(NodeEnum::DefElem(d)) = inner(w) else {
                continue;
            };
            if d.defname != "tag" {
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    format!("unrecognized filter variable \"{}\"", d.defname),
                ));
            }
            if tags.is_some() {
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    format!("filter variable \"{}\" specified more than once", d.defname),
                ));
            }
            let list = match d.arg.as_deref().and_then(inner) {
                Some(NodeEnum::List(l)) => names(&l.items),
                _ => Vec::new(),
            };
            tags = Some(list);
        }
        if let Some(list) = &tags {
            match s.eventname.as_str() {
                "login" => {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "tag filtering is not supported for login event triggers",
                    ));
                }
                ev => {
                    for t in list {
                        let up = t.to_uppercase();
                        let Some((ok, rewrite_ok)) = tag_info(&up) else {
                            return Err(Error::new(
                                code::SYNTAX_ERROR,
                                format!("filter value \"{t}\" not recognized for filter variable \"tag\""),
                            ));
                        };
                        let fine = if ev == "table_rewrite" { rewrite_ok } else { ok };
                        if !fine {
                            return Err(Error::new(
                                code::FEATURE_NOT_SUPPORTED,
                                format!("event triggers are not supported for {t}"),
                            ));
                        }
                    }
                }
            }
        }
        let name = crate::catalog::truncate_ident(&s.trigname);
        if self.cat.event_triggers.values().any(|e| e.name == name) {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("event trigger \"{name}\" already exists"),
            ));
        }
        // The function: no arguments, returning event_trigger.
        let parts = names(&s.funcname);
        let fname = parts.last().cloned().unwrap_or_default();
        let path: Vec<u32> = if parts.len() > 1 {
            vec![self.cat.schema_oid(&parts[parts.len() - 2])?]
        } else {
            self.search_path()
                .iter()
                .filter_map(|n| self.cat.schema_by_name(n).map(|s| s.oid))
                .collect()
        };
        let func = path.iter().find_map(|sch| {
            self.cat
                .functions
                .values()
                .find(|f| f.schema == *sch && f.name == fname && f.args.is_empty())
                .cloned()
        });
        let builtin = crate::sql::func::registry()
            .funcs
            .get(fname.as_str())
            .is_some_and(|v| v.iter().any(|d| d.args.is_empty()));
        let must_return = || {
            Error::new(
                code::INVALID_OBJECT_DEFINITION,
                format!("function {} must return type event_trigger", parts.join(".")),
            )
        };
        let func = match func {
            Some(f) if crate::plpgsql::is_event_trigger_type(&f.ret) => f,
            Some(_) => return Err(must_return()),
            None if builtin => return Err(must_return()),
            None => {
                return Err(Error::new(
                    code::UNDEFINED_FUNCTION,
                    format!("function {}() does not exist", parts.join(".")),
                ));
            }
        };
        let oid = self.cat.alloc_oid();
        self.cat.event_triggers.insert(
            oid,
            Arc::new(EventTrigger {
                oid,
                name,
                event: s.eventname.clone(),
                owner: self.me(),
                function: func.oid,
                enabled: 'O',
                tags: tags.map(|t| t.iter().map(|x| x.to_uppercase()).collect()),
            }),
        );
        self.save()?;
        Ok("CREATE EVENT TRIGGER".into())
    }

    fn event_trigger_named(&self, name: &str) -> Result<Arc<EventTrigger>> {
        self.cat
            .event_triggers
            .values()
            .find(|e| e.name == name)
            .cloned()
            .ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("event trigger \"{name}\" does not exist"),
                )
            })
    }

    pub(super) fn alter_event_trigger(&mut self, s: &pb::AlterEventTrigStmt) -> Result<String> {
        let e = self.event_trigger_named(&s.trigname)?;
        self.require_owner(e.owner, "event trigger", &quote_ident(&e.name))?;
        let mut e = (*e).clone();
        e.enabled = s.tgenabled.chars().next().unwrap_or('O');
        self.cat.event_triggers.insert(e.oid, Arc::new(e));
        self.save()?;
        Ok("ALTER EVENT TRIGGER".into())
    }

    pub(super) fn rename_event_trigger(&mut self, s: &pb::RenameStmt) -> Result<String> {
        let old = s.object.as_deref().and_then(ast::sval).unwrap_or_default().to_string();
        let e = self.event_trigger_named(&old)?;
        let new = crate::catalog::truncate_ident(&s.newname);
        if self.cat.event_triggers.values().any(|x| x.name == new) {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("event trigger \"{new}\" already exists"),
            ));
        }
        self.require_owner(e.owner, "event trigger", &quote_ident(&e.name))?;
        let mut e = (*e).clone();
        e.name = new;
        self.cat.event_triggers.insert(e.oid, Arc::new(e));
        self.save()?;
        Ok("ALTER EVENT TRIGGER".into())
    }

    pub(super) fn event_trigger_owner(&mut self, name: &str, new: u32) -> Result<String> {
        let e = self.event_trigger_named(name)?;
        self.require_owner(e.owner, "event trigger", &quote_ident(&e.name))?;
        if !self.cat.is_superuser(new) {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied to change owner of event trigger \"{}\"", e.name),
            )
            .hint("The owner of an event trigger must be a superuser."));
        }
        let mut e = (*e).clone();
        e.owner = new;
        self.cat.event_triggers.insert(e.oid, Arc::new(e));
        self.save()?;
        Ok("ALTER EVENT TRIGGER".into())
    }

    /// The event trigger a DROP EVENT TRIGGER names. None: missing, IF EXISTS.
    pub(super) fn event_trigger_to_drop(&mut self, name: &str, missing_ok: bool) -> Result<Option<u32>> {
        match self.cat.event_triggers.values().find(|e| e.name == name).cloned() {
            Some(e) => {
                self.require_owner(e.owner, "event trigger", &quote_ident(&e.name))?;
                Ok(Some(e.oid))
            }
            None if missing_ok => {
                self.notice(format!("event trigger \"{name}\" does not exist, skipping"));
                Ok(None)
            }
            None => Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("event trigger \"{name}\" does not exist"),
            )),
        }
    }
}
