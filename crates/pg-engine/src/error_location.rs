//! The FILE / LINE / ROUTINE fields of an error: Postgres sends them with every error and
//! notice, and clients read them (asyncpg and pgjdbc recognise a plan invalidated by DDL by
//! RevalidateCachedQuery; postgres.js reports `routine`). The engine's code isn't Postgres's:
//! errors that name their routine (`Error::routine`) or match one of Postgres 18.6's
//! messages below get its location; others name the engine.

use crate::error::Error;

/// (SQLSTATE, message prefix, message suffix) → (routine, file, line), from PostgreSQL 18.6.
const KNOWN: &[(&str, &str, &str, &str, &str, u32)] = &[
    ("42804", "column \"", "\" but expression is of type record", "transformAssignedExpr", "parse_target.c", 588),
    ("42804", "column \"", "", "transformAssignedExpr", "parse_target.c", 588),
    ("42P01", "relation \"", "\" does not exist", "parserOpenTable", "parse_relation.c", 1498),
    ("42703", "column \"", "\" does not exist", "errorMissingColumn", "parse_relation.c", 3854),
    ("23505", "duplicate key value violates unique constraint", "", "_bt_check_unique", "nbtinsert.c", 666),
    ("23502", "null value in column", "violates not-null constraint", "ReportNotNullViolationError", "execMain.c", 2213),
    ("23503", "insert or update on table", "", "ri_ReportViolation", "ri_triggers.c", 2772),
    ("23503", "update or delete on table", "", "ri_ReportViolation", "ri_triggers.c", 2772),
    ("23514", "new row for relation", "violates check constraint", "ExecConstraints", "execMain.c", 2076),
    ("22001", "value too long for type character varying", "", "varchar", "varchar.c", 637),
    ("22P02", "invalid input syntax for type integer", "", "pg_strtoint32_safe", "numutils.c", 615),
    ("22P02", "invalid input syntax for type bigint", "", "pg_strtoint64_safe", "numutils.c", 876),
    ("22P02", "invalid input syntax for type numeric", "", "numeric_in", "numeric.c", 803),
    ("22P02", "invalid input syntax for type uuid", "", "string_to_uuid", "uuid.c", 191),
    ("22P02", "invalid input syntax for type boolean", "", "boolin", "bool.c", 148),
    ("22007", "invalid input syntax for type date", "", "DateTimeParseError", "datetime.c", 4262),
    ("22007", "invalid input syntax for type timestamp", "", "DateTimeParseError", "datetime.c", 4262),
    ("42883", "function ", " does not exist", "ParseFuncOrColumn", "parse_func.c", 629),
    ("42601", "syntax error at", "", "scanner_yyerror", "scan.l", 1236),
    ("42P07", "relation \"", "\" already exists", "heap_create_with_catalog", "heap.c", 1194),
    ("42P05", "prepared statement \"", "\" already exists", "StorePreparedStatement", "prepare.c", 412),
    ("26000", "prepared statement \"", "\" does not exist", "FetchPreparedStatement", "prepare.c", 451),
    ("25P02", "current transaction is aborted", "", "exec_simple_query", "postgres.c", 1136),
    ("42704", "unrecognized configuration parameter", "", "assignable_custom_variable_name", "guc.c", 1166),
    ("21000", "more than one row returned by a subquery", "", "ExecSetParamPlan", "nodeSubplan.c", 1164),
    ("34000", "cursor \"", "\" does not exist", "PerformPortalFetch", "portalcmds.c", 197),
    ("42803", "column \"", "must appear in the GROUP BY clause or be used in an aggregate function", "substitute_grouped_columns_mutator", "parse_agg.c", 1552),
    ("42702", "column reference \"", "\" is ambiguous", "colNameToVar", "parse_relation.c", 960),
    ("42501", "permission denied for ", "", "aclcheck_error", "aclchk.c", 2793),
    ("57014", "canceling statement due to user request", "", "ProcessInterrupts", "postgres.c", 3464),
    ("57P01", "terminating connection due to administrator command", "", "ProcessInterrupts", "postgres.c", 3354),
    ("40P01", "deadlock detected", "", "DeadLockReport", "deadlock.c", 1133),
    ("28P01", "password authentication failed for user", "", "auth_failed", "auth.c", 317),
    ("3D000", "database \"", "\" does not exist", "InitPostgres", "postinit.c", 1014),
];

/// (file, line, routine) for an error.
pub fn location(e: &Error) -> (&'static str, String, &'static str) {
    if let Some(r) = e.objects.as_ref().and_then(|o| o.routine) {
        if let Some(k) = KNOWN.iter().find(|k| k.3 == r) {
            return (k.4, k.5.to_string(), r);
        }
        return match r {
            "RevalidateCachedQuery" => ("plancache.c", "866".into(), r),
            "exec_stmt_raise" => ("pl_exec.c", "3909".into(), r),
            _ => ("pg-engine", "0".into(), r),
        };
    }
    let m = e.message.as_str();
    match KNOWN
        .iter()
        .find(|k| k.0 == e.code && m.starts_with(k.1) && m.ends_with(k.2))
    {
        Some(k) => (k.4, k.5.to_string(), k.3),
        None => ("pg-engine", "0".into(), ""),
    }
}
