//! The FILE / LINE / ROUTINE fields of an error: Postgres sends them with every error and
//! notice, and clients read them (asyncpg and pgjdbc recognise a plan invalidated by DDL by
//! RevalidateCachedQuery; postgres.js reports `routine`). The engine's code isn't Postgres's:
//! errors that name their routine (`Error::routine`) or match one of Postgres 18.6's
//! messages below get its location; others name the engine.

use crate::error::Error;

/// (SQLSTATE, message prefix, message suffix) → (routine, file, line), from PostgreSQL 18.6.
const KNOWN: &[(&str, &str, &str, &str, &str, u32)] = &[
    // The protocol's: Parse, Bind, Describe, Execute, a message's own layout, COPY, the
    // fast-path interface, startup. (Messages more than one routine raises are named by
    // theirs: `Error::routine`; untagged, a message takes its first entry: a simple query's
    // aborted transaction its own.)
    ("25P02", "current transaction is aborted", "", "exec_simple_query", "postgres.c", 1136),
    ("42601", "cannot insert multiple commands into a prepared statement", "", "exec_parse_message", "postgres.c", 1473),
    ("25P02", "current transaction is aborted", "", "exec_parse_message", "postgres.c", 1493),
    ("26000", "unnamed prepared statement does not exist", "", "exec_bind_message", "postgres.c", 1670),
    ("08P01", "bind message has ", " parameters", "exec_bind_message", "postgres.c", 1722),
    ("08P01", "bind message supplies ", "", "exec_bind_message", "postgres.c", 1728),
    ("25P02", "current transaction is aborted", "", "exec_bind_message", "postgres.c", 1745),
    ("22023", "unsupported format code: ", "", "exec_bind_message", "postgres.c", 1949),
    ("08P01", "bind message has ", " columns", "PortalSetResultFormat", "pquery.c", 640),
    ("22023", "unsupported format code: ", "", "printtup_prepare_info", "printtup.c", 290),
    ("34000", "portal \"", "\" does not exist", "exec_execute_message", "postgres.c", 2136),
    ("25P02", "current transaction is aborted", "", "exec_execute_message", "postgres.c", 2250),
    ("26000", "unnamed prepared statement does not exist", "", "exec_describe_statement_message", "postgres.c", 2668),
    ("25P02", "current transaction is aborted", "", "exec_describe_statement_message", "postgres.c", 2687),
    ("34000", "portal \"", "\" does not exist", "exec_describe_portal_message", "postgres.c", 2750),
    ("25P02", "current transaction is aborted", "", "exec_describe_portal_message", "postgres.c", 2764),
    ("08P01", "invalid CLOSE message subtype ", "", "PostgresMain", "postgres.c", 4910),
    ("08P01", "invalid DESCRIBE message subtype ", "", "PostgresMain", "postgres.c", 4947),
    ("08P01", "terminating connection because protocol synchronization was lost", "", "PostgresMain", "postgres.c", 4502),
    ("08P01", "invalid frontend message type ", "", "SocketBackend", "postgres.c", 454),
    ("08P01", "insufficient data left in message", "", "pq_copymsgbytes", "pqformat.c", 531),
    ("08P01", "invalid string in message", "", "pq_getmsgstring", "pqformat.c", 593),
    ("08P01", "invalid message format", "", "pq_getmsgend", "pqformat.c", 638),
    ("42P03", "cursor \"", "\" already exists", "CreatePortal", "portalmem.c", 185),
    ("42P18", "could not determine data type of parameter $", "", "pg_analyze_and_rewrite_varparams", "postgres.c", 733),
    ("42883", "function with OID ", " does not exist", "fetch_fp_info", "fastpath.c", 139),
    ("25P02", "current transaction is aborted", "", "HandleFunctionRequest", "fastpath.c", 206),
    ("57014", "COPY from stdin failed: ", "", "CopyGetData", "copyfromparse.c", 315),
    ("08P01", "unexpected message type ", " during COPY from stdin", "CopyGetData", "copyfromparse.c", 292),
    ("08006", "unexpected EOF on client connection with an open transaction", "", "CopyGetData", "copyfromparse.c", 301),
    ("XX000", "cache lookup failed for type ", "", "TupleDescInitEntry", "tupdesc.c", 896),
    ("XX000", "cache lookup failed for type ", "", "getBaseTypeAndTypmod", "lsyscache.c", 2852),
    ("XX000", "cache lookup failed for type ", "", "getTypeInputInfo", "lsyscache.c", 3183),
    ("XX000", "cache lookup failed for type ", "", "getTypeBinaryInputInfo", "lsyscache.c", 3249),
    ("08P01", "received unencrypted data after SSL request", "", "ProcessStartupPacket", "backend_startup.c", 626),
    ("08P01", "received unencrypted data after GSSAPI encryption request", "", "ProcessStartupPacket", "backend_startup.c", 694),
    ("0A000", "unsupported frontend protocol ", "", "ProcessStartupPacket", "backend_startup.c", 731),
    ("08P01", "invalid startup packet layout: expected terminator as last byte", "", "ProcessStartupPacket", "backend_startup.c", 835),
    ("28000", "no PostgreSQL user name specified in startup packet", "", "ProcessStartupPacket", "backend_startup.c", 852),
    ("28000", "role \"", "\" does not exist", "InitializeSessionUserId", "miscinit.c", 802),
    ("22023", "invalid value for parameter \"replication\"", "", "ProcessStartupPacket", "backend_startup.c", 791),
    ("22023", "invalid value for parameter \"client_encoding\"", "", "call_string_check_hook", "guc.c", 6936),
    ("22023", "invalid value for parameter \"", "", "parse_and_validate_value", "guc.c", 3162),
    ("55P02", "parameter \"", "\" cannot be changed without restarting the server", "set_config_with_handle", "guc.c", 3499),
    ("55P02", "parameter \"", "\" cannot be changed", "set_config_with_handle", "guc.c", 3476),
    ("42601", "--", " must be first argument", "process_postgres_switches", "postgres.c", 3860),
    ("42601", "--", " requires a value", "process_postgres_switches", "postgres.c", 3874),
    ("42601", "-c ", " requires a value", "process_postgres_switches", "postgres.c", 3879),
    ("42601", "invalid command-line argument for server process: ", "", "process_postgres_switches", "postgres.c", 4026),
    ("42883", "procedure ", " does not exist", "ParseFuncOrColumn", "parse_func.c", 620),
    ("42804", "column \"", "\" but expression is of type record", "transformAssignedExpr", "parse_target.c", 588),
    ("42804", "column \"", "", "transformAssignedExpr", "parse_target.c", 588),
    ("42P01", "relation \"", "\" does not exist", "parserOpenTable", "parse_relation.c", 1498),
    ("42703", "column \"", "\" does not exist", "errorMissingColumn", "parse_relation.c", 3854),
    ("23505", "duplicate key value violates unique constraint", "", "_bt_check_unique", "nbtinsert.c", 666),
    ("23502", "null value in column", "violates not-null constraint", "ReportNotNullViolationError", "execMain.c", 2213),
    ("23503", "insert or update on table", "", "ri_ReportViolation", "ri_triggers.c", 2772),
    ("23503", "update or delete on table", "", "ri_ReportViolation", "ri_triggers.c", 2772),
    ("23514", "new row for relation", "violates partition constraint", "ExecPartitionCheckEmitError", "execMain.c", 1964),
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
    ("42704", "unrecognized configuration parameter", "", "assignable_custom_variable_name", "guc.c", 1166),
    ("21000", "more than one row returned by a subquery", "", "ExecSetParamPlan", "nodeSubplan.c", 1164),
    ("34000", "cursor \"", "\" does not exist", "PerformPortalFetch", "portalcmds.c", 197),
    ("42803", "column \"", "must appear in the GROUP BY clause or be used in an aggregate function", "substitute_grouped_columns_mutator", "parse_agg.c", 1552),
    ("42702", "column reference \"", "\" is ambiguous", "colNameToVar", "parse_relation.c", 960),
    ("42501", "permission denied for ", "", "aclcheck_error", "aclchk.c", 2793),
    ("57014", "canceling statement due to user request", "", "ProcessInterrupts", "postgres.c", 3464),
    ("57P01", "terminating connection due to administrator command", "", "ProcessInterrupts", "postgres.c", 3354),
    ("25P03", "terminating connection due to idle-in-transaction timeout", "", "ProcessInterrupts", "postgres.c", 3485),
    ("25P04", "terminating connection due to transaction timeout", "", "ProcessInterrupts", "postgres.c", 3498),
    ("57P05", "terminating connection due to idle-session timeout", "", "ProcessInterrupts", "postgres.c", 3511),
    ("40P01", "deadlock detected", "", "DeadLockReport", "deadlock.c", 1133),
    ("28P01", "password authentication failed for user", "", "auth_failed", "auth.c", 317),
    ("3D000", "database \"", "\" does not exist", "InitPostgres", "postinit.c", 1014),
    // VACUUM (VERBOSE) and ANALYZE (VERBOSE)'s INFO messages (each names its routine).
    ("00000", "aggressively vacuuming \"", "", "heap_vacuum_rel", "vacuumlazy.c", 814),
    ("00000", "vacuuming \"", "", "heap_vacuum_rel", "vacuumlazy.c", 819),
    ("00000", "finished vacuuming \"", "", "heap_vacuum_rel", "vacuumlazy.c", 1146),
    ("00000", "table \"", "", "lazy_truncate_heap", "vacuumlazy.c", 3341),
    ("00000", "analyzing \"", "\" inheritance tree", "do_analyze_rel", "analyze.c", 316),
    ("00000", "analyzing \"", "", "do_analyze_rel", "analyze.c", 321),
    ("00000", "finished analyzing table \"", "", "do_analyze_rel", "analyze.c", 842),
    ("00000", "\"", "", "acquire_sample_rows", "analyze.c", 1345),
    ("00000", "skipping analyze of \"", "contains no child tables", "acquire_inherited_sample_rows", "analyze.c", 1429),
    ("00000", "skipping analyze of \"", "", "acquire_inherited_sample_rows", "analyze.c", 1527),
    ("00000", "clustering \"", "", "copy_table_data", "cluster.c", 966),
    ("00000", "vacuuming \"", "", "copy_table_data", "cluster.c", 971),
    ("00000", "\"", "", "copy_table_data", "cluster.c", 998),
];

/// (file, line, routine) for an error.
pub fn location(e: &Error) -> (&'static str, String, &'static str) {
    let m = e.message.as_str();
    // (The suffix may be followed by a name: `... violates check constraint "c"`.)
    let matches = |k: &&(&str, &str, &str, &str, &str, u32)| {
        k.0 == e.code
            && m.starts_with(k.1)
            && (m.ends_with(k.2) || (k.2.starts_with(" violates") || k.2.starts_with("violates")) && m.contains(k.2))
    };
    if let Some(r) = e.objects.as_ref().and_then(|o| o.routine) {
        // (The routine's message that this is, else its first.)
        let mut of = KNOWN.iter().filter(|k| k.3 == r);
        if let Some(k) = of.clone().find(matches).or_else(|| of.next()) {
            return (k.4, k.5.to_string(), r);
        }
        return match r {
            "RevalidateCachedQuery" => ("plancache.c", "866".into(), r),
            "exec_stmt_raise" => ("pl_exec.c", "3909".into(), r),
            _ => ("pg-engine", "0".into(), r),
        };
    }
    // (The INFO messages only by their routine: their prefixes are too general.)
    match KNOWN.iter().filter(|k| k.0 != "00000").find(matches) {
        Some(k) => (k.4, k.5.to_string(), k.3),
        None => ("pg-engine", "0".into(), ""),
    }
}
