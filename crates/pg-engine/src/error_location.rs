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
    ("22P04", "extra data after last expected column", "", "CopyFromTextLikeOneRow", "copyfromparse.c", 960),
    ("22P04", "missing data for column \"", "", "CopyFromTextLikeOneRow", "copyfromparse.c", 974),
    ("22P04", "unterminated CSV quoted field", "", "CopyReadAttributesCSV", "copyfromparse.c", 1919),
    ("22P04", "COPY file signature not recognized", "", "ReceiveCopyBinaryHeader", "copyfromparse.c", 198),
    ("22023", "COPY format \"", "\" not recognized", "ProcessCopyOptions", "copy.c", 574),
    ("22P02", "skipped more than REJECT_LIMIT ", "", "CopyFrom", "copyfrom.c", 1169),
    ("55000", "cannot perform COPY FREEZE because the table was not created or truncated in the current subtransaction", "", "CopyFrom", "copyfrom.c", 904),
    ("XX000", "cache lookup failed for type ", "", "TupleDescInitEntry", "tupdesc.c", 896),
    ("XX000", "cache lookup failed for type ", "", "getBaseTypeAndTypmod", "lsyscache.c", 2852),
    ("XX000", "cache lookup failed for type ", "", "getTypeInputInfo", "lsyscache.c", 3183),
    ("XX000", "cache lookup failed for type ", "", "getTypeBinaryInputInfo", "lsyscache.c", 3249),
    ("XX000", "cache lookup failed for type ", "", "typeOrDomainTypeRelid", "parse_type.c", 699),
    ("XX000", "cache lookup failed for type ", "", "format_type_extended", "format_type.c", 137),
    ("42704", "type with OID ", " does not exist", "lookup_type_cache", "typcache.c", 471),
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
    ("42804", "column \"", "\" cannot be cast automatically to type ", "ATPrepAlterColumnType", "tablecmds.c", 14529),
    ("42804", "column \"", "\" but expression is of type record", "transformAssignedExpr", "parse_target.c", 588),
    ("42804", "column \"", "", "transformAssignedExpr", "parse_target.c", 588),
    ("42P01", "relation \"", "\" does not exist", "parserOpenTable", "parse_relation.c", 1498),
    ("42703", "column \"", "\" does not exist", "errorMissingColumn", "parse_relation.c", 3854),
    ("42703", "column ", " does not exist", "errorMissingColumn", "parse_relation.c", 3854),
    ("23514", "value for domain ", " violates check constraint", "ExecEvalConstraintCheck", "execExprInterp.c", 4497),
    ("23502", "domain ", " does not allow null values", "ExecEvalConstraintNotNull", "execExprInterp.c", 4482),
    ("23P01", "conflicting key value violates exclusion constraint", "", "check_exclusion_or_unique_constraint", "execIndexing.c", 928),
    ("23514", "no partition of relation ", " found for row", "ExecFindPartition", "execPartition.c", 328),
    ("428C9", "cannot insert a non-DEFAULT value into column ", "", "rewriteTargetListIU", "rewriteHandler.c", 942),
    ("22001", "value too long for type character(", "", "bpchar", "varchar.c", 310),
    ("42883", "operator does not exist: ", "", "op_error", "parse_oper.c", 635),
    ("P0004", "", "", "exec_stmt_assert", "pl_exec.c", 3965),
    ("2BP01", "cannot drop ", " because other objects depend on it", "reportDependentObjects", "dependency.c", 1148),
    ("42P01", "table \"", "\" does not exist", "DropErrorMsgNonExistent", "tablecmds.c", 1500),
    ("0A000", "cannot alter type of a column used by a view or rule", "", "RememberAllDependentForRebuilding", "tablecmds.c", 15163),
    ("57014", "canceling statement due to statement timeout", "", "ProcessInterrupts", "postgres.c", 3444),
    ("21000", "ON CONFLICT DO UPDATE command cannot affect row a second time", "", "ExecOnConflictUpdate", "nodeModifyTable.c", 2804),
    ("42601", "subquery must return only one column", "", "transformSubLink", "parse_expr.c", 1922),
    ("22P02", "invalid input syntax for type json", "", "json_errsave_error", "jsonfuncs.c", 659),
    ("22P02", "invalid input value for enum ", "", "enum_in", "enum.c", 129),
    ("42P01", "relation \"", "\" does not exist", "regclassin", "regproc.c", 911),
    ("23505", "duplicate key value violates unique constraint", "", "_bt_check_unique", "nbtinsert.c", 666),
    ("23502", "null value in column", "violates not-null constraint", "ReportNotNullViolationError", "execMain.c", 2213),
    ("23503", "insert or update on table", "", "ri_ReportViolation", "ri_triggers.c", 2772),
    ("23503", "update or delete on table", "", "ri_ReportViolation", "ri_triggers.c", 2799),
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
    ("42601", "syntax error at end of input", "", "scanner_yyerror", "scan.l", 1228),
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
    // Transaction blocks and the transaction parameters' check hooks.
    ("25P01", "SET LOCAL can only be used in transaction blocks", "", "CheckTransactionBlock", "xact.c", 3745),
    ("25P01", "SET TRANSACTION can only be used in transaction blocks", "", "CheckTransactionBlock", "xact.c", 3745),
    ("25P01", "SAVEPOINT can only be used in transaction blocks", "", "DefineSavepoint", "xact.c", 4420),
    ("25P01", "there is no transaction in progress", "", "EndTransactionBlock", "xact.c", 4073),
    ("25P01", "there is no transaction in progress", "", "UserAbortTransactionBlock", "xact.c", 4275),
    ("25001", "", " cannot run inside a transaction block", "PreventInTransactionBlock", "xact.c", 3654),
    ("25001", "SET TRANSACTION ISOLATION LEVEL must ", "", "call_enum_check_hook", "guc.c", 6977),
    ("25001", "SET TRANSACTION [NOT] DEFERRABLE ", "", "call_bool_check_hook", "guc.c", 6825),
    ("25001", "transaction read-write mode must be set before any query", "", "call_bool_check_hook", "guc.c", 6825),
    ("25001", "cannot set transaction read-write mode inside a read-only transaction", "", "call_bool_check_hook", "guc.c", 6825),
    ("22023", "role \"", "\" does not exist", "call_string_check_hook", "guc.c", 6936),
    ("42501", "permission denied to set role \"", "", "call_string_check_hook", "guc.c", 6936),
    // Arithmetic and casts: each operator's, function's or cast's routine names its error
    // (`BY_ROUTINE`): int4pl, int48mi, dtoi4, ... (`int_op_error`, `cast_routine`).
    ("22003", "smallint out of range", "", "int2pl", "int.c", 942),
    ("22003", "smallint out of range", "", "int2mi", "int.c", 956),
    ("22003", "smallint out of range", "", "int2mul", "int.c", 970),
    ("22012", "division by zero", "", "int2div", "int.c", 986),
    ("22003", "smallint out of range", "", "int2div", "int.c", 1002),
    ("22012", "division by zero", "", "int2mod", "int.c", 1194),
    ("22003", "integer out of range", "", "int24pl", "int.c", 1024),
    ("22003", "integer out of range", "", "int24mi", "int.c", 1038),
    ("22003", "integer out of range", "", "int24mul", "int.c", 1052),
    ("22012", "division by zero", "", "int24div", "int.c", 1066),
    ("22003", "bigint out of range", "", "int28pl", "int8.c", 1150),
    ("22003", "bigint out of range", "", "int28mi", "int8.c", 1164),
    ("22003", "bigint out of range", "", "int28mul", "int8.c", 1178),
    ("22012", "division by zero", "", "int28div", "int8.c", 1192),
    ("22003", "integer out of range", "", "int42pl", "int.c", 1085),
    ("22003", "integer out of range", "", "int42mi", "int.c", 1099),
    ("22003", "integer out of range", "", "int42mul", "int.c", 1113),
    ("22012", "division by zero", "", "int42div", "int.c", 1128),
    ("22003", "integer out of range", "", "int42div", "int.c", 1144),
    ("22003", "integer out of range", "", "int4pl", "int.c", 827),
    ("22003", "integer out of range", "", "int4mi", "int.c", 841),
    ("22003", "integer out of range", "", "int4mul", "int.c", 855),
    ("22012", "division by zero", "", "int4div", "int.c", 870),
    ("22003", "integer out of range", "", "int4div", "int.c", 886),
    ("22012", "division by zero", "", "int4mod", "int.c", 1166),
    ("22003", "bigint out of range", "", "int48pl", "int8.c", 1008),
    ("22003", "bigint out of range", "", "int48mi", "int8.c", 1022),
    ("22003", "bigint out of range", "", "int48mul", "int8.c", 1036),
    ("22012", "division by zero", "", "int48div", "int8.c", 1050),
    ("22003", "bigint out of range", "", "int82pl", "int8.c", 1069),
    ("22003", "bigint out of range", "", "int82mi", "int8.c", 1083),
    ("22003", "bigint out of range", "", "int82mul", "int8.c", 1097),
    ("22012", "division by zero", "", "int82div", "int8.c", 1112),
    ("22003", "bigint out of range", "", "int82div", "int8.c", 1128),
    ("22003", "bigint out of range", "", "int84pl", "int8.c", 927),
    ("22003", "bigint out of range", "", "int84mi", "int8.c", 941),
    ("22003", "bigint out of range", "", "int84mul", "int8.c", 955),
    ("22012", "division by zero", "", "int84div", "int8.c", 970),
    ("22003", "bigint out of range", "", "int84div", "int8.c", 986),
    ("22003", "bigint out of range", "", "int8pl", "int8.c", 469),
    ("22003", "bigint out of range", "", "int8mi", "int8.c", 483),
    ("22003", "bigint out of range", "", "int8mul", "int8.c", 497),
    ("22012", "division by zero", "", "int8div", "int8.c", 512),
    ("22003", "bigint out of range", "", "int8div", "int8.c", 528),
    ("22012", "division by zero", "", "int8mod", "int8.c", 570),
    ("22003", "smallint out of range", "", "int2um", "int.c", 920),
    ("22003", "integer out of range", "", "int4um", "int.c", 805),
    ("22003", "bigint out of range", "", "int8um", "int8.c", 446),
    ("22003", "smallint out of range", "", "int2abs", "int.c", 1240),
    ("22003", "integer out of range", "", "int4abs", "int.c", 1226),
    ("22003", "bigint out of range", "", "int8abs", "int8.c", 552),
    ("22003", "integer out of range", "", "int4gcd_internal", "int.c", 1290),
    ("22003", "bigint out of range", "", "int8gcd_internal", "int8.c", 634),
    ("22003", "integer out of range", "", "int4lcm", "int.c", 1358),
    ("22003", "bigint out of range", "", "int8lcm", "int8.c", 702),
    ("22003", "smallint out of range", "", "i4toi2", "int.c", 382),
    ("22003", "smallint out of range", "", "int82", "int8.c", 1305),
    ("22003", "integer out of range", "", "int84", "int8.c", 1284),
    ("22003", "smallint out of range", "", "dtoi2", "float.c", 1252),
    ("22003", "integer out of range", "", "dtoi4", "float.c", 1227),
    ("22003", "bigint out of range", "", "dtoi8", "int8.c", 1340),
    ("22003", "smallint out of range", "", "ftoi2", "float.c", 1326),
    ("22003", "integer out of range", "", "ftoi4", "float.c", 1301),
    ("22003", "bigint out of range", "", "ftoi8", "int8.c", 1375),
    ("22003", "smallint out of range", "", "numeric_int2", "numeric.c", 4706),
    ("22003", "integer out of range", "", "numeric_int4_opt_error", "numeric.c", 4562),
    ("22003", "bigint out of range", "", "numeric_int8_opt_error", "numeric.c", 4650),
    ("0A000", "cannot convert NaN to smallint", "", "numeric_int2", "numeric.c", 4688),
    ("0A000", "cannot convert NaN to integer", "", "numeric_int4_opt_error", "numeric.c", 4540),
    ("0A000", "cannot convert NaN to bigint", "", "numeric_int8_opt_error", "numeric.c", 4628),
    ("22012", "division by zero", "", "float_zero_divide_error", "float.c", 104),
    ("22012", "division by zero", "", "div_var", "numeric.c", 9436),
    ("2201F", "cannot take square root of a negative number", "", "dsqrt", "float.c", 1452),
    ("2201F", "cannot take square root of a negative number", "", "sqrt_var", "numeric.c", 10491),
    ("2201F", "cannot take square root of a negative number", "", "numeric_sqrt", "numeric.c", 3817),
    ("2201E", "cannot take logarithm of zero", "", "dlog1", "float.c", 1700),
    ("2201E", "cannot take logarithm of a negative number", "", "dlog1", "float.c", 1704),
    ("2201E", "cannot take logarithm of zero", "", "dlog10", "float.c", 1733),
    ("2201E", "cannot take logarithm of a negative number", "", "dlog10", "float.c", 1737),
    ("2201E", "cannot take logarithm of zero", "", "ln_var", "numeric.c", 11162),
    ("2201E", "cannot take logarithm of a negative number", "", "ln_var", "numeric.c", 11166),
    ("2201F", "zero raised to a negative power is undefined", "", "dpow", "float.c", 1520),
    ("2201F", "a negative number raised to a non-integer power yields a complex result", "", "dpow", "float.c", 1524),
    ("2201F", "zero raised to a negative power is undefined", "", "numeric_power", "numeric.c", 4217),
    ("2201F", "a negative number raised to a non-integer power yields a complex result", "", "power_var", "numeric.c", 11382),
    ("22003", "value overflows numeric format", "", "make_result_opt_error", "numeric.c", 8031),
    ("22003", "value overflows numeric format", "", "exp_var", "numeric.c", 10965),
    ("22003", "value overflows numeric format", "", "numeric_in", "numeric.c", 793),
    ("2201G", "lower bound cannot equal upper bound", "", "width_bucket_float8", "float.c", 4160),
    ("22003", "input is out of range", "", "dacos", "float.c", 1770),
    ("22003", "input is out of range", "", "dasin", "float.c", 1801),
    ("22003", "input is out of range", "", "dcos", "float.c", 1896),
    ("22003", "input is out of range", "", "dcot", "float.c", 1923),
    ("22003", "input is out of range", "", "dsin", "float.c", 1951),
    ("22003", "input is out of range", "", "dtan", "float.c", 1978),
    ("22003", "input is out of range", "", "dacosh", "float.c", 2701),
    ("22003", "input is out of range", "", "datanh", "float.c", 2725),
    // (Messages one routine raises, matched by themselves.)
    ("22003", "value out of range: overflow", "", "float_overflow_error", "float.c", 88),
    ("22003", "value out of range: underflow", "", "float_underflow_error", "float.c", 96),
    ("22003", "\"", "\" is out of range for type double precision", "float8in_internal", "float.c", 487),
    ("22003", "\"", "\" is out of range for type real", "float4in_internal", "float.c", 285),
    ("22003", "value \"", "\" is out of range for type smallint", "pg_strtoint16_safe", "numutils.c", 348),
    ("22003", "value \"", "\" is out of range for type integer", "pg_strtoint32_safe", "numutils.c", 609),
    ("22003", "value \"", "\" is out of range for type bigint", "pg_strtoint64_safe", "numutils.c", 870),
    ("22003", "factorial of a negative number is undefined", "", "numeric_fac", "numeric.c", 3757),
    ("22003", "numeric field overflow", "", "apply_typmod", "numeric.c", 8121),
    ("22008", "date/time field value out of range: ", "", "DateTimeParseError", "datetime.c", 4228),
    ("22011", "negative substring length not allowed", "", "text_substring", "varlena.c", 985),
    ("22023", "character number must be positive", "", "chr", "oracle_compat.c", 1058),
    ("22023", "cannot cast jsonb ", "", "cannotCastJsonbValue", "jsonb.c", 2029),
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

/// NOTICEs matched by their message (`KNOWN`'s INFO messages are too general for that).
const NOTICES: &[(&str, &str, &str, &str, &str, u32)] = &[
    ("00000", "table \"", "\" does not exist, skipping", "DropErrorMsgNonExistent", "tablecmds.c", 1506),
    ("42P07", "relation \"", "\" already exists, skipping", "transformCreateStmt", "parse_utilcmd.c", 208),
    ("00000", "", " skipped due to data type incompatibility", "CopyFrom", "copyfrom.c", 1473),
    ("00000", "skipping row due to data type incompatibility at line ", "", "CopyFromTextLikeOneRow", "copyfromparse.c", 1055),
];

/// Messages matched only by the routine an error names (each type has its own).
const BY_ROUTINE: &[&str] = &[
    "division by zero",
    "smallint out of range",
    "integer out of range",
    "bigint out of range",
    "cannot convert NaN to ",
    "cannot take square root of a negative number",
    "cannot take logarithm of ",
    "zero raised to a negative power is undefined",
    "a negative number raised to a non-integer power yields a complex result",
    "value overflows numeric format",
    "input is out of range",
];

/// (file, line, routine) for an error.
pub fn location(e: &Error) -> (&'static str, String, &'static str) {
    let m = e.message.as_str();
    // (The suffix may be followed by a name: `... violates check constraint "c"`, `...
    // to type integer`.)
    let matches = |k: &&(&str, &str, &str, &str, &str, u32)| {
        k.0 == e.code
            && m.starts_with(k.1)
            && (m.ends_with(k.2)
                || (k.2.starts_with(" violates") || k.2.starts_with("violates") || k.2.ends_with(' ')) && m.contains(k.2))
    };
    if let Some(r) = e.objects.as_ref().and_then(|o| o.routine) {
        // (The routine's message that this is, else its first.)
        let mut of = KNOWN.iter().filter(|k| k.3 == r);
        if let Some(k) = of.clone().find(matches).or_else(|| of.next()) {
            return (k.4, special_line(k, e), r);
        }
        return match r {
            "RevalidateCachedQuery" => ("plancache.c", "866".into(), r),
            "exec_stmt_raise" => ("pl_exec.c", "3909".into(), r),
            _ => ("pg-engine", "0".into(), r),
        };
    }
    if !matches!(e.severity, crate::error::Severity::Error | crate::error::Severity::Fatal)
        && let Some(k) = NOTICES.iter().find(matches)
    {
        return (k.4, k.5.to_string(), k.3);
    }
    // (The INFO messages only by their routine: their prefixes are too general; so are
    // messages every type's routine raises.)
    match KNOWN.iter().filter(|k| k.0 != "00000" && !BY_ROUTINE.iter().any(|b| k.1.starts_with(b))).find(matches) {
        Some(k) => (k.4, special_line(k, e), k.3),
        None => ("pg-engine", "0".into(), ""),
    }
}


/// The line of a routine that raises one message at two places, told apart by what else
/// the error says.
fn special_line(k: &(&str, &str, &str, &str, &str, u32), e: &Error) -> String {
    match k.3 {
        // DTERR_MD_FIELD_OVERFLOW's hint about DateStyle.
        "DateTimeParseError" if k.0 == "22008" && e.hint.is_some() => "4228".into(),
        "DateTimeParseError" if k.0 == "22008" => "4221".into(),
        // A generated column's, not an identity column's.
        // A qualified name's (`relation "s.t"`).
        "parserOpenTable" if e.message.split('"').nth(1).is_some_and(|n| n.contains('.')) => "1477".into(),
        "rewriteTargetListIU" if e.detail.as_deref().is_some_and(|d| d.ends_with("is a generated column.")) => "981".into(),
        _ => k.5.to_string(),
    }
}
