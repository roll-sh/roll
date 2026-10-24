//! Virtual `pg_catalog` and `information_schema` relations, generated from the catalog.
//! Their rows are built when a query scans them; nothing is stored.

use super::acl::{self, AclItem, ObjKind};
use super::{
    Catalog, ConstraintKind, FkAction, INFORMATION_SCHEMA_OID, IndexKey, PG_CATALOG_OID, Table,
    quote_ident,
};
use crate::error::Result;
use crate::exec::{ExecCtx, Row};
use crate::sql::func::system::index_def;
use crate::sql::ruleutils::{Stored, stored_expr};
use crate::types::{DataType, Value};
use std::sync::{Arc, OnceLock};

macro_rules! sys_tables {
    ($($v:ident = ($schema:ident, $name:literal, $oid:literal, $cols:literal);)*) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum SysTable { $($v,)* }

        const ALL: &[(SysTable, u32, &str, u32, &str)] = &[
            $((SysTable::$v, $schema, $name, $oid, $cols),)*
        ];
    };
}

const PGC: u32 = PG_CATALOG_OID;
const INF: u32 = INFORMATION_SCHEMA_OID;

// Column specs: "name type, ...". `char1` is the one-byte "char" type.
sys_tables! {
    PgNamespace = (PGC, "pg_namespace", 2615, "oid oid, nspname name, nspowner oid, nspacl aclitem[]");
    PgClass = (PGC, "pg_class", 1259, "oid oid, relname name, relnamespace oid, reltype oid, reloftype oid, relowner oid, relam oid, relfilenode oid, reltablespace oid, relpages int4, reltuples float4, relallvisible int4, relallfrozen int4, reltoastrelid oid, relhasindex bool, relisshared bool, relpersistence char1, relkind char1, relnatts int2, relchecks int2, relhasrules bool, relhastriggers bool, relhassubclass bool, relrowsecurity bool, relforcerowsecurity bool, relispopulated bool, relreplident char1, relispartition bool, relrewrite oid, relfrozenxid xid, relminmxid xid, relacl aclitem[], reloptions text[], relpartbound pg_node_tree");
    PgAttribute = (PGC, "pg_attribute", 1249, "attrelid oid, attname name, atttypid oid, attlen int2, attnum int2, atttypmod int4, attndims int2, attbyval bool, attalign char1, attstorage char1, attcompression char1, attnotnull bool, atthasdef bool, atthasmissing bool, attidentity char1, attgenerated char1, attisdropped bool, attislocal bool, attinhcount int2, attcollation oid, attstattarget int2, attacl aclitem[], attoptions text[], attfdwoptions text[], attmissingval anyarray");
    PgType = (PGC, "pg_type", 1247, "oid oid, typname name, typnamespace oid, typowner oid, typlen int2, typbyval bool, typtype char1, typcategory char1, typispreferred bool, typisdefined bool, typdelim char1, typrelid oid, typsubscript regproc, typelem oid, typarray oid, typinput regproc, typoutput regproc, typreceive regproc, typsend regproc, typmodin regproc, typmodout regproc, typanalyze regproc, typalign char1, typstorage char1, typnotnull bool, typbasetype oid, typtypmod int4, typndims int4, typcollation oid, typdefaultbin pg_node_tree, typdefault text, typacl aclitem[]");
    PgIndex = (PGC, "pg_index", 2610, "indexrelid oid, indrelid oid, indnatts int2, indnkeyatts int2, indisunique bool, indnullsnotdistinct bool, indisprimary bool, indisexclusion bool, indimmediate bool, indisclustered bool, indisvalid bool, indcheckxmin bool, indisready bool, indislive bool, indisreplident bool, indkey int2vector, indcollation oidvector, indclass oidvector, indoption int2vector, indexprs pg_node_tree, indpred pg_node_tree");
    PgConstraint = (PGC, "pg_constraint", 2606, "oid oid, conname name, connamespace oid, contype char1, condeferrable bool, condeferred bool, conenforced bool, convalidated bool, conrelid oid, contypid oid, conindid oid, conparentid oid, confrelid oid, confupdtype char1, confdeltype char1, confmatchtype char1, conislocal bool, coninhcount int2, connoinherit bool, conperiod bool, conkey int2[], confkey int2[], conpfeqop oid[], conppeqop oid[], conffeqop oid[], confdelsetcols int2[], conexclop oid[], conbin pg_node_tree");
    PgProc = (PGC, "pg_proc", 1255, "oid oid, proname name, pronamespace oid, proowner oid, prolang oid, procost float4, prorows float4, provariadic oid, prosupport regproc, prokind char1, prosecdef bool, proleakproof bool, proisstrict bool, proretset bool, provolatile char1, proparallel char1, pronargs int2, pronargdefaults int2, prorettype oid, proargtypes oidvector, proallargtypes oid[], proargmodes char1[], proargnames text[], proargdefaults pg_node_tree, protrftypes oid[], prosrc text, probin text, prosqlbody pg_node_tree, proconfig text[], proacl aclitem[]");
    PgDatabase = (PGC, "pg_database", 1262, "oid oid, datname name, datdba oid, encoding int4, datlocprovider char1, datistemplate bool, datallowconn bool, dathasloginevt bool, datconnlimit int4, datfrozenxid xid, datminmxid xid, dattablespace oid, datcollate text, datctype text, datlocale text, daticurules text, datcollversion text, datacl aclitem[]");
    PgAuthid = (PGC, "pg_authid", 1260, "oid oid, rolname name, rolsuper bool, rolinherit bool, rolcreaterole bool, rolcreatedb bool, rolcanlogin bool, rolreplication bool, rolbypassrls bool, rolconnlimit int4, rolpassword text, rolvaliduntil timestamptz");
    PgAuthMembers = (PGC, "pg_auth_members", 1261, "oid oid, roleid oid, member oid, grantor oid, admin_option bool, inherit_option bool, set_option bool");
    PgDescription = (PGC, "pg_description", 2609, "objoid oid, classoid oid, objsubid int4, description text");
    PgShdescription = (PGC, "pg_shdescription", 2396, "objoid oid, classoid oid, description text");
    PgAttrdef = (PGC, "pg_attrdef", 2604, "oid oid, adrelid oid, adnum int2, adbin pg_node_tree");
    PgSequence = (PGC, "pg_sequence", 2224, "seqrelid oid, seqtypid oid, seqstart int8, seqincrement int8, seqmax int8, seqmin int8, seqcache int8, seqcycle bool");
    PgEnum = (PGC, "pg_enum", 3501, "oid oid, enumtypid oid, enumsortorder float4, enumlabel name");
    PgAm = (PGC, "pg_am", 2601, "oid oid, amname name, amhandler regproc, amtype char1");
    PgExtension = (PGC, "pg_extension", 3079, "oid oid, extname name, extowner oid, extnamespace oid, extrelocatable bool, extversion text, extconfig oid[], extcondition text[]");
    PgCollation = (PGC, "pg_collation", 3456, "oid oid, collname name, collnamespace oid, collowner oid, collprovider char1, collisdeterministic bool, collencoding int4, collcollate text, collctype text, colllocale text, collicurules text, collversion text");
    PgInherits = (PGC, "pg_inherits", 2611, "inhrelid oid, inhparent oid, inhseqno int4, inhdetachpending bool");
    PgDepend = (PGC, "pg_depend", 2608, "classid oid, objid oid, objsubid int4, refclassid oid, refobjid oid, refobjsubid int4, deptype char1");
    PgTrigger = (PGC, "pg_trigger", 2620, "oid oid, tgrelid oid, tgparentid oid, tgname name, tgfoid oid, tgtype int2, tgenabled char1, tgisinternal bool, tgconstrrelid oid, tgconstrindid oid, tgconstraint oid, tgdeferrable bool, tginitdeferred bool, tgnargs int2, tgattr int2vector, tgargs bytea, tgqual pg_node_tree, tgoldtable name, tgnewtable name");
    PgOperator = (PGC, "pg_operator", 2617, "oid oid, oprname name, oprnamespace oid, oprowner oid, oprkind char1, oprcanmerge bool, oprcanhash bool, oprleft oid, oprright oid, oprresult oid, oprcom oid, oprnegate oid, oprcode regproc, oprrest regproc, oprjoin regproc");
    PgRange = (PGC, "pg_range", 3541, "rngtypid oid, rngsubtype oid, rngmultitypid oid, rngcollation oid, rngsubopc oid, rngcanonical regproc, rngsubdiff regproc");
    PgTablespace = (PGC, "pg_tablespace", 1213, "oid oid, spcname name, spcowner oid, spcacl aclitem[], spcoptions text[]");
    PgLanguage = (PGC, "pg_language", 2612, "oid oid, lanname name, lanowner oid, lanispl bool, lanpltrusted bool, lanplcallfoid oid, laninline oid, lanvalidator oid, lanacl aclitem[]");
    PgCast = (PGC, "pg_cast", 2605, "oid oid, castsource oid, casttarget oid, castfunc oid, castcontext char1, castmethod char1");
    PgRewrite = (PGC, "pg_rewrite", 2618, "oid oid, rulename name, ev_class oid, ev_type char1, ev_enabled char1, is_instead bool, ev_qual pg_node_tree, ev_action pg_node_tree");
    PgPolicy = (PGC, "pg_policy", 3256, "oid oid, polname name, polrelid oid, polcmd char1, polpermissive bool, polroles oid[], polqual pg_node_tree, polwithcheck pg_node_tree");
    PgEventTrigger = (PGC, "pg_event_trigger", 3466, "oid oid, evtname name, evtevent name, evtowner oid, evtfoid oid, evtenabled char1, evttags text[]");
    PgAggregate = (PGC, "pg_aggregate", 2600, "aggfnoid regproc, aggkind char1, aggnumdirectargs int2, aggtransfn regproc, aggfinalfn regproc, aggcombinefn regproc, aggserialfn regproc, aggdeserialfn regproc, aggmtransfn regproc, aggminvtransfn regproc, aggmfinalfn regproc, aggfinalextra bool, aggmfinalextra bool, aggfinalmodify char1, aggmfinalmodify char1, aggsortop oid, aggtranstype oid, aggtransspace int4, aggmtranstype oid, aggmtransspace int4, agginitval text, aggminitval text");
    PgPartitionedTable = (PGC, "pg_partitioned_table", 3350, "partrelid oid, partstrat char1, partnatts int2, partdefid oid, partattrs int2vector, partclass oidvector, partcollation oidvector, partexprs pg_node_tree");
    PgForeignTable = (PGC, "pg_foreign_table", 3118, "ftrelid oid, ftserver oid, ftoptions text[]");
    PgPublicationNamespace = (PGC, "pg_publication_namespace", 6237, "oid oid, pnpubid oid, pnnspid oid");
    PgPublicationRel = (PGC, "pg_publication_rel", 6106, "oid oid, prpubid oid, prrelid oid, prqual pg_node_tree, prattrs int2vector");
    PgPublication = (PGC, "pg_publication", 6104, "oid oid, pubname name, pubowner oid, puballtables bool, pubinsert bool, pubupdate bool, pubdelete bool, pubtruncate bool, pubviaroot bool, pubgencols char1");
    PgOpclass = (PGC, "pg_opclass", 2616, "oid oid, opcmethod oid, opcname name, opcnamespace oid, opcowner oid, opcfamily oid, opcintype oid, opcdefault bool, opckeytype oid");
    PgOpfamily = (PGC, "pg_opfamily", 2753, "oid oid, opfmethod oid, opfname name, opfnamespace oid, opfowner oid");
    PgInitPrivs = (PGC, "pg_init_privs", 3394, "objoid oid, classoid oid, objsubid int4, privtype char1, initprivs aclitem[]");
    PgStatistic = (PGC, "pg_statistic", 2619, "starelid oid, staattnum int2, stainherit bool, stanullfrac float4, stawidth int4, stadistinct float4, stakind1 int2, stakind2 int2, stakind3 int2, stakind4 int2, stakind5 int2, staop1 oid, staop2 oid, staop3 oid, staop4 oid, staop5 oid, stacoll1 oid, stacoll2 oid, stacoll3 oid, stacoll4 oid, stacoll5 oid, stanumbers1 float4[], stanumbers2 float4[], stanumbers3 float4[], stanumbers4 float4[], stanumbers5 float4[], stavalues1 anyarray, stavalues2 anyarray, stavalues3 anyarray, stavalues4 anyarray, stavalues5 anyarray");
    PgStatisticExtData = (PGC, "pg_statistic_ext_data", 3429, "stxoid oid, stxdinherit bool, stxdndistinct pg_ndistinct, stxddependencies pg_dependencies, stxdmcv pg_mcv_list, stxdexpr pg_statistic[]");
    PgStatsExt = (PGC, "pg_stats_ext", 12058, "schemaname name, tablename name, statistics_schemaname name, statistics_name name, statistics_owner name, attnames name[], exprs text[], kinds char1[], inherited bool, n_distinct pg_ndistinct, dependencies pg_dependencies, most_common_vals text[], most_common_val_nulls bool[], most_common_freqs float8[], most_common_base_freqs float8[]");
    PgStatsExtExprs = (PGC, "pg_stats_ext_exprs", 12063, "schemaname name, tablename name, statistics_schemaname name, statistics_name name, statistics_owner name, expr text, inherited bool, null_frac float4, avg_width int4, n_distinct float4, most_common_vals anyarray, most_common_freqs float4[], histogram_bounds anyarray, correlation float4, most_common_elems anyarray, most_common_elem_freqs float4[], elem_count_histogram float4[]");
    PgStatisticExt = (PGC, "pg_statistic_ext", 3381, "oid oid, stxrelid oid, stxname name, stxnamespace oid, stxowner oid, stxkeys int2vector, stxstattarget int2, stxkind char1[], stxexprs pg_node_tree");
    PgDbRoleSetting = (PGC, "pg_db_role_setting", 2964, "setdatabase oid, setrole oid, setconfig text[]");
    PgDefaultAcl = (PGC, "pg_default_acl", 826, "oid oid, defaclrole oid, defaclnamespace oid, defaclobjtype char1, defaclacl aclitem[]");
    PgTsConfig = (PGC, "pg_ts_config", 3602, "oid oid, cfgname name, cfgnamespace oid, cfgowner oid, cfgparser oid");
    PgTsConfigMap = (PGC, "pg_ts_config_map", 3603, "mapcfg oid, maptokentype int4, mapseqno int4, mapdict oid");
    PgTsDict = (PGC, "pg_ts_dict", 3600, "oid oid, dictname name, dictnamespace oid, dictowner oid, dicttemplate oid, dictinitoption text");
    PgTsParser = (PGC, "pg_ts_parser", 3601, "oid oid, prsname name, prsnamespace oid, prsstart regproc, prstoken regproc, prsend regproc, prsheadline regproc, prslextype regproc");
    PgTsTemplate = (PGC, "pg_ts_template", 3764, "oid oid, tmplname name, tmplnamespace oid, tmplinit regproc, tmpllexize regproc");
    PgLargeobject = (PGC, "pg_largeobject", 2613, "loid oid, pageno int4, data bytea");
    PgLargeobjectMetadata = (PGC, "pg_largeobject_metadata", 2995, "oid oid, lomowner oid, lomacl aclitem[]");
    PgShdepend = (PGC, "pg_shdepend", 1214, "dbid oid, classid oid, objid oid, objsubid int4, refclassid oid, refobjid oid, deptype char1");
    // System views.
    PgRoles = (PGC, "pg_roles", 12000, "rolname name, rolsuper bool, rolinherit bool, rolcreaterole bool, rolcreatedb bool, rolcanlogin bool, rolreplication bool, rolconnlimit int4, rolpassword text, rolvaliduntil timestamptz, rolbypassrls bool, rolconfig text[], oid oid");
    PgUser = (PGC, "pg_user", 12014, "usename name, usesysid oid, usecreatedb bool, usesuper bool, userepl bool, usebypassrls bool, passwd text, valuntil timestamptz, useconfig text[]");
    PgSettings = (PGC, "pg_settings", 12104, "name text, setting text, unit text, category text, short_desc text, extra_desc text, context text, vartype text, source text, min_val text, max_val text, enumvals text[], boot_val text, reset_val text, sourcefile text, sourceline int4, pending_restart bool");
    PgTables = (PGC, "pg_tables", 12033, "schemaname name, tablename name, tableowner name, tablespace name, hasindexes bool, hasrules bool, hastriggers bool, rowsecurity bool");
    PgViews = (PGC, "pg_views", 12028, "schemaname name, viewname name, viewowner name, definition text");
    PgMatviews = (PGC, "pg_matviews", 12038, "schemaname name, matviewname name, matviewowner name, tablespace name, hasindexes bool, ispopulated bool, definition text");
    PgIndexes = (PGC, "pg_indexes", 12043, "schemaname name, tablename name, indexname name, tablespace name, indexdef text");
    PgSequences = (PGC, "pg_sequences", 12048, "schemaname name, sequencename name, sequenceowner name, data_type regtype, start_value int8, min_value int8, max_value int8, increment_by int8, cycle bool, cache_size int8, last_value int8");
    PgStatActivity = (PGC, "pg_stat_activity", 12226, "datid oid, datname name, pid int4, leader_pid int4, usesysid oid, usename name, application_name text, client_addr inet, client_hostname text, client_port int4, backend_start timestamptz, xact_start timestamptz, query_start timestamptz, state_change timestamptz, wait_event_type text, wait_event text, state text, backend_xid xid, backend_xmin xid, query_id int8, query text, backend_type text");
    PgLocks = (PGC, "pg_locks", 12073, "locktype text, database oid, relation oid, page int4, tuple int2, virtualxid text, transactionid xid, classid oid, objid oid, objsubid int2, virtualtransaction text, pid int4, mode text, granted bool, fastpath bool, waitstart timestamptz");
    PgStatUserTables = (PGC, "pg_stat_user_tables", 12165, "relid oid, schemaname name, relname name, seq_scan int8, last_seq_scan timestamptz, seq_tup_read int8, idx_scan int8, last_idx_scan timestamptz, idx_tup_fetch int8, n_tup_ins int8, n_tup_upd int8, n_tup_del int8, n_tup_hot_upd int8, n_tup_newpage_upd int8, n_live_tup int8, n_dead_tup int8, n_mod_since_analyze int8, n_ins_since_vacuum int8, last_vacuum timestamptz, last_autovacuum timestamptz, last_analyze timestamptz, last_autoanalyze timestamptz, vacuum_count int8, autovacuum_count int8, analyze_count int8, autoanalyze_count int8, total_vacuum_time float8, total_autovacuum_time float8, total_analyze_time float8, total_autoanalyze_time float8");
    PgStatDatabase = (PGC, "pg_stat_database", 12270, "datid oid, datname name, numbackends int4, xact_commit int8, xact_rollback int8, blks_read int8, blks_hit int8, tup_returned int8, tup_fetched int8, tup_inserted int8, tup_updated int8, tup_deleted int8, conflicts int8, temp_files int8, temp_bytes int8, deadlocks int8, checksum_failures int8, checksum_last_failure timestamptz, blk_read_time float8, blk_write_time float8, session_time float8, active_time float8, idle_in_transaction_time float8, sessions int8, sessions_abandoned int8, sessions_fatal int8, sessions_killed int8, parallel_workers_to_launch int8, parallel_workers_launched int8, stats_reset timestamptz");
    PgTimezoneNames = (PGC, "pg_timezone_names", 12126, "name text, abbrev text, utc_offset interval, is_dst bool");
    PgAvailableExtensions = (PGC, "pg_available_extensions", 12081, "name name, default_version text, installed_version text, comment text");
    PgPreparedStatements = (PGC, "pg_prepared_statements", 12095, "name text, statement text, prepare_time timestamptz, parameter_types regtype[], result_types regtype[], from_sql bool, generic_plans int8, custom_plans int8");
    PgCursors = (PGC, "pg_cursors", 12077, "name text, statement text, is_holdable bool, is_binary bool, is_scrollable bool, creation_time timestamptz");
    PgStatUserIndexes = (PGC, "pg_stat_user_indexes", 12196, "relid oid, indexrelid oid, schemaname name, relname name, indexrelname name, idx_scan int8, last_idx_scan timestamptz, idx_tup_read int8, idx_tup_fetch int8");
    PgStatioUserTables = (PGC, "pg_statio_user_tables", 12183, "relid oid, schemaname name, relname name, heap_blks_read int8, heap_blks_hit int8, idx_blks_read int8, idx_blks_hit int8, toast_blks_read int8, toast_blks_hit int8, tidx_blks_read int8, tidx_blks_hit int8");
    PgReplicationSlots = (PGC, "pg_replication_slots", 12261, "slot_name name, plugin name, slot_type text, datoid oid, database name, temporary bool, active bool, active_pid int4, xmin xid, catalog_xmin xid, restart_lsn pg_lsn, confirmed_flush_lsn pg_lsn, wal_status text, safe_wal_size int8, two_phase bool, two_phase_at pg_lsn, inactive_since timestamptz, conflicting bool, invalidation_reason text, failover bool, synced bool");
    PgPolicies = (PGC, "pg_policies", 12018, "schemaname name, tablename name, policyname name, permissive text, roles name[], cmd text, qual text, with_check text");
    PgStats = (PGC, "pg_stats", 12053, "schemaname name, tablename name, attname name, inherited bool, null_frac float4, avg_width int4, n_distinct float4, most_common_vals anyarray, most_common_freqs float4[], histogram_bounds anyarray, correlation float4, most_common_elems anyarray, most_common_elem_freqs float4[], elem_count_histogram float4[], range_length_histogram anyarray, range_empty_frac float4, range_bounds_histogram anyarray");
    PgStatReplication = (PGC, "pg_stat_replication", 12231, "pid int4, usesysid oid, usename name, application_name text, client_addr inet, client_hostname text, client_port int4, backend_start timestamptz, backend_xmin xid, state text, sent_lsn pg_lsn, write_lsn pg_lsn, flush_lsn pg_lsn, replay_lsn pg_lsn, write_lag interval, flush_lag interval, replay_lag interval, sync_priority int4, sync_state text, reply_time timestamptz");
    // information_schema
    IsSchemata = (INF, "schemata", 13501, "catalog_name sql_identifier, schema_name sql_identifier, schema_owner sql_identifier, default_character_set_catalog sql_identifier, default_character_set_schema sql_identifier, default_character_set_name sql_identifier, sql_path character_data");
    IsTables = (INF, "tables", 13544, "table_catalog sql_identifier, table_schema sql_identifier, table_name sql_identifier, table_type character_data, self_referencing_column_name sql_identifier, reference_generation character_data, user_defined_type_catalog sql_identifier, user_defined_type_schema sql_identifier, user_defined_type_name sql_identifier, is_insertable_into yes_or_no, is_typed yes_or_no, commit_action character_data");
    IsColumns = (INF, "columns", 13415, "table_catalog sql_identifier, table_schema sql_identifier, table_name sql_identifier, column_name sql_identifier, ordinal_position cardinal_number, column_default character_data, is_nullable yes_or_no, data_type character_data, character_maximum_length cardinal_number, character_octet_length cardinal_number, numeric_precision cardinal_number, numeric_precision_radix cardinal_number, numeric_scale cardinal_number, datetime_precision cardinal_number, interval_type character_data, interval_precision cardinal_number, character_set_catalog sql_identifier, character_set_schema sql_identifier, character_set_name sql_identifier, collation_catalog sql_identifier, collation_schema sql_identifier, collation_name sql_identifier, domain_catalog sql_identifier, domain_schema sql_identifier, domain_name sql_identifier, udt_catalog sql_identifier, udt_schema sql_identifier, udt_name sql_identifier, scope_catalog sql_identifier, scope_schema sql_identifier, scope_name sql_identifier, maximum_cardinality cardinal_number, dtd_identifier sql_identifier, is_self_referencing yes_or_no, is_identity yes_or_no, identity_generation character_data, identity_start character_data, identity_increment character_data, identity_maximum character_data, identity_minimum character_data, identity_cycle yes_or_no, is_generated character_data, generation_expression character_data, is_updatable yes_or_no");
    IsTableConstraints = (INF, "table_constraints", 13530, "constraint_catalog sql_identifier, constraint_schema sql_identifier, constraint_name sql_identifier, table_catalog sql_identifier, table_schema sql_identifier, table_name sql_identifier, constraint_type character_data, is_deferrable yes_or_no, initially_deferred yes_or_no, enforced yes_or_no, nulls_distinct yes_or_no");
    IsKeyColumnUsage = (INF, "key_column_usage", 13448, "constraint_catalog sql_identifier, constraint_schema sql_identifier, constraint_name sql_identifier, table_catalog sql_identifier, table_schema sql_identifier, table_name sql_identifier, column_name sql_identifier, ordinal_position cardinal_number, position_in_unique_constraint cardinal_number");
    IsReferentialConstraints = (INF, "referential_constraints", 13458, "constraint_catalog sql_identifier, constraint_schema sql_identifier, constraint_name sql_identifier, unique_constraint_catalog sql_identifier, unique_constraint_schema sql_identifier, unique_constraint_name sql_identifier, match_option character_data, update_rule character_data, delete_rule character_data");
    IsConstraintColumnUsage = (INF, "constraint_column_usage", 13420, "table_catalog sql_identifier, table_schema sql_identifier, table_name sql_identifier, column_name sql_identifier, constraint_catalog sql_identifier, constraint_schema sql_identifier, constraint_name sql_identifier");
    IsCheckConstraints = (INF, "check_constraints", 13380, "constraint_catalog sql_identifier, constraint_schema sql_identifier, constraint_name sql_identifier, check_clause character_data");
    IsViews = (INF, "views", 13602, "table_catalog sql_identifier, table_schema sql_identifier, table_name sql_identifier, view_definition character_data, check_option character_data, is_updatable yes_or_no, is_insertable_into yes_or_no, is_trigger_updatable yes_or_no, is_trigger_deletable yes_or_no, is_trigger_insertable_into yes_or_no");
    IsSequences = (INF, "sequences", 13505, "sequence_catalog sql_identifier, sequence_schema sql_identifier, sequence_name sql_identifier, data_type character_data, numeric_precision cardinal_number, numeric_precision_radix cardinal_number, numeric_scale cardinal_number, start_value character_data, minimum_value character_data, maximum_value character_data, increment character_data, cycle_option yes_or_no");
    IsRoutines = (INF, "routines", 13496, "specific_catalog sql_identifier, specific_schema sql_identifier, specific_name sql_identifier, routine_catalog sql_identifier, routine_schema sql_identifier, routine_name sql_identifier, routine_type character_data, module_catalog sql_identifier, module_schema sql_identifier, module_name sql_identifier, udt_catalog sql_identifier, udt_schema sql_identifier, udt_name sql_identifier, data_type character_data, character_maximum_length cardinal_number, character_octet_length cardinal_number, character_set_catalog sql_identifier, character_set_schema sql_identifier, character_set_name sql_identifier, collation_catalog sql_identifier, collation_schema sql_identifier, collation_name sql_identifier, numeric_precision cardinal_number, numeric_precision_radix cardinal_number, numeric_scale cardinal_number, datetime_precision cardinal_number, interval_type character_data, interval_precision cardinal_number, type_udt_catalog sql_identifier, type_udt_schema sql_identifier, type_udt_name sql_identifier, scope_catalog sql_identifier, scope_schema sql_identifier, scope_name sql_identifier, maximum_cardinality cardinal_number, dtd_identifier sql_identifier, routine_body character_data, routine_definition character_data, external_name character_data, external_language character_data, parameter_style character_data, is_deterministic yes_or_no, sql_data_access character_data, is_null_call yes_or_no, sql_path character_data, schema_level_routine yes_or_no, max_dynamic_result_sets cardinal_number, is_user_defined_cast yes_or_no, is_implicitly_invocable yes_or_no, security_type character_data, to_sql_specific_catalog sql_identifier, to_sql_specific_schema sql_identifier, to_sql_specific_name sql_identifier, as_locator yes_or_no, created time_stamp, last_altered time_stamp, new_savepoint_level yes_or_no, is_udt_dependent yes_or_no, result_cast_from_data_type character_data, result_cast_as_locator yes_or_no, result_cast_char_max_length cardinal_number, result_cast_char_octet_length cardinal_number, result_cast_char_set_catalog sql_identifier, result_cast_char_set_schema sql_identifier, result_cast_char_set_name sql_identifier, result_cast_collation_catalog sql_identifier, result_cast_collation_schema sql_identifier, result_cast_collation_name sql_identifier, result_cast_numeric_precision cardinal_number, result_cast_numeric_precision_radix cardinal_number, result_cast_numeric_scale cardinal_number, result_cast_datetime_precision cardinal_number, result_cast_interval_type character_data, result_cast_interval_precision cardinal_number, result_cast_type_udt_catalog sql_identifier, result_cast_type_udt_schema sql_identifier, result_cast_type_udt_name sql_identifier, result_cast_scope_catalog sql_identifier, result_cast_scope_schema sql_identifier, result_cast_scope_name sql_identifier, result_cast_maximum_cardinality cardinal_number, result_cast_dtd_identifier sql_identifier");
    IsTriggers = (INF, "triggers", 13559, "trigger_catalog sql_identifier, trigger_schema sql_identifier, trigger_name sql_identifier, event_manipulation character_data, event_object_catalog sql_identifier, event_object_schema sql_identifier, event_object_table sql_identifier, action_order cardinal_number, action_condition character_data, action_statement character_data, action_orientation character_data, action_timing character_data, action_reference_old_table sql_identifier, action_reference_new_table sql_identifier, action_reference_old_row sql_identifier, action_reference_new_row sql_identifier, created time_stamp");
    IsEnabledRoles = (INF, "enabled_roles", 13444, "role_name sql_identifier");
    IsTablePrivileges = (INF, "table_privileges", 13535, "grantor sql_identifier, grantee sql_identifier, table_catalog sql_identifier, table_schema sql_identifier, table_name sql_identifier, privilege_type character_data, is_grantable yes_or_no, with_hierarchy yes_or_no");
    IsParameters = (INF, "parameters", 13453, "specific_catalog sql_identifier, specific_schema sql_identifier, specific_name sql_identifier, ordinal_position cardinal_number, parameter_mode character_data, is_result yes_or_no, as_locator yes_or_no, parameter_name sql_identifier, data_type character_data, character_maximum_length cardinal_number, character_octet_length cardinal_number, character_set_catalog sql_identifier, character_set_schema sql_identifier, character_set_name sql_identifier, collation_catalog sql_identifier, collation_schema sql_identifier, collation_name sql_identifier, numeric_precision cardinal_number, numeric_precision_radix cardinal_number, numeric_scale cardinal_number, datetime_precision cardinal_number, interval_type character_data, interval_precision cardinal_number, udt_catalog sql_identifier, udt_schema sql_identifier, udt_name sql_identifier, scope_catalog sql_identifier, scope_schema sql_identifier, scope_name sql_identifier, maximum_cardinality cardinal_number, dtd_identifier sql_identifier, parameter_default character_data");
    IsDomains = (INF, "domains", 13439, "domain_catalog sql_identifier, domain_schema sql_identifier, domain_name sql_identifier, data_type character_data, character_maximum_length cardinal_number, character_octet_length cardinal_number, character_set_catalog sql_identifier, character_set_schema sql_identifier, character_set_name sql_identifier, collation_catalog sql_identifier, collation_schema sql_identifier, collation_name sql_identifier, numeric_precision cardinal_number, numeric_precision_radix cardinal_number, numeric_scale cardinal_number, datetime_precision cardinal_number, interval_type character_data, interval_precision cardinal_number, domain_default character_data, udt_catalog sql_identifier, udt_schema sql_identifier, udt_name sql_identifier, scope_catalog sql_identifier, scope_schema sql_identifier, scope_name sql_identifier, maximum_cardinality cardinal_number, dtd_identifier sql_identifier");
    IsCharacterSets = (INF, "character_sets", 13370, "character_set_catalog sql_identifier, character_set_schema sql_identifier, character_set_name sql_identifier, character_repertoire sql_identifier, form_of_use sql_identifier, default_collate_catalog sql_identifier, default_collate_schema sql_identifier, default_collate_name sql_identifier");
    IsApplicableRoles = (INF, "applicable_roles", 13356, "grantee sql_identifier, role_name sql_identifier, is_grantable yes_or_no");
    IsAdministrableRoleAuthorizations = (INF, "administrable_role_authorizations", 13361, "grantee sql_identifier, role_name sql_identifier, is_grantable yes_or_no");
    IsRoleTableGrants = (INF, "role_table_grants", 13540, "grantor sql_identifier, grantee sql_identifier, table_catalog sql_identifier, table_schema sql_identifier, table_name sql_identifier, privilege_type character_data, is_grantable yes_or_no, with_hierarchy yes_or_no");
    IsColumnPrivileges = (INF, "column_privileges", 13405, "grantor sql_identifier, grantee sql_identifier, table_catalog sql_identifier, table_schema sql_identifier, table_name sql_identifier, column_name sql_identifier, privilege_type character_data, is_grantable yes_or_no");
    IsRoleColumnGrants = (INF, "role_column_grants", 13463, "grantor sql_identifier, grantee sql_identifier, table_catalog sql_identifier, table_schema sql_identifier, table_name sql_identifier, column_name sql_identifier, privilege_type character_data, is_grantable yes_or_no");
    IsRoutinePrivileges = (INF, "routine_privileges", 13472, "grantor sql_identifier, grantee sql_identifier, specific_catalog sql_identifier, specific_schema sql_identifier, specific_name sql_identifier, routine_catalog sql_identifier, routine_schema sql_identifier, routine_name sql_identifier, privilege_type character_data, is_grantable yes_or_no");
    IsRoleRoutineGrants = (INF, "role_routine_grants", 13477, "grantor sql_identifier, grantee sql_identifier, specific_catalog sql_identifier, specific_schema sql_identifier, specific_name sql_identifier, routine_catalog sql_identifier, routine_schema sql_identifier, routine_name sql_identifier, privilege_type character_data, is_grantable yes_or_no");
    IsUsagePrivileges = (INF, "usage_privileges", 13573, "grantor sql_identifier, grantee sql_identifier, object_catalog sql_identifier, object_schema sql_identifier, object_name sql_identifier, object_type character_data, privilege_type character_data, is_grantable yes_or_no");
    IsRoleUsageGrants = (INF, "role_usage_grants", 13578, "grantor sql_identifier, grantee sql_identifier, object_catalog sql_identifier, object_schema sql_identifier, object_name sql_identifier, object_type character_data, privilege_type character_data, is_grantable yes_or_no");
    IsUdtPrivileges = (INF, "udt_privileges", 13564, "grantor sql_identifier, grantee sql_identifier, udt_catalog sql_identifier, udt_schema sql_identifier, udt_name sql_identifier, privilege_type character_data, is_grantable yes_or_no");
    IsRoleUdtGrants = (INF, "role_udt_grants", 13569, "grantor sql_identifier, grantee sql_identifier, udt_catalog sql_identifier, udt_schema sql_identifier, udt_name sql_identifier, privilege_type character_data, is_grantable yes_or_no");
    IsPrivPgForeignDataWrappers = (INF, "_pg_foreign_data_wrappers", 13626, "oid oid, fdwowner oid, fdwoptions text[], foreign_data_wrapper_catalog sql_identifier, foreign_data_wrapper_name sql_identifier, authorization_identifier sql_identifier, foreign_data_wrapper_language character_data");
    IsPrivPgForeignServers = (INF, "_pg_foreign_servers", 13638, "oid oid, srvoptions text[], foreign_server_catalog sql_identifier, foreign_server_name sql_identifier, foreign_data_wrapper_catalog sql_identifier, foreign_data_wrapper_name sql_identifier, foreign_server_type character_data, foreign_server_version character_data, authorization_identifier sql_identifier");
    IsPrivPgForeignTableColumns = (INF, "_pg_foreign_table_columns", 13617, "nspname name, relname name, attname name, attfdwoptions text[]");
    IsPrivPgForeignTables = (INF, "_pg_foreign_tables", 13650, "foreign_table_catalog sql_identifier, foreign_table_schema sql_identifier, foreign_table_name sql_identifier, ftoptions text[], foreign_server_catalog sql_identifier, foreign_server_name sql_identifier, authorization_identifier sql_identifier");
    IsPrivPgUserMappings = (INF, "_pg_user_mappings", 13663, "oid oid, umoptions text[], umuser oid, authorization_identifier sql_identifier, foreign_server_catalog sql_identifier, foreign_server_name sql_identifier, srvowner sql_identifier");
    IsAttributes = (INF, "attributes", 13365, "udt_catalog sql_identifier, udt_schema sql_identifier, udt_name sql_identifier, attribute_name sql_identifier, ordinal_position cardinal_number, attribute_default character_data, is_nullable yes_or_no, data_type character_data, character_maximum_length cardinal_number, character_octet_length cardinal_number, character_set_catalog sql_identifier, character_set_schema sql_identifier, character_set_name sql_identifier, collation_catalog sql_identifier, collation_schema sql_identifier, collation_name sql_identifier, numeric_precision cardinal_number, numeric_precision_radix cardinal_number, numeric_scale cardinal_number, datetime_precision cardinal_number, interval_type character_data, interval_precision cardinal_number, attribute_udt_catalog sql_identifier, attribute_udt_schema sql_identifier, attribute_udt_name sql_identifier, scope_catalog sql_identifier, scope_schema sql_identifier, scope_name sql_identifier, maximum_cardinality cardinal_number, dtd_identifier sql_identifier, is_derived_reference_attribute yes_or_no");
    IsCheckConstraintRoutineUsage = (INF, "check_constraint_routine_usage", 13375, "constraint_catalog sql_identifier, constraint_schema sql_identifier, constraint_name sql_identifier, specific_catalog sql_identifier, specific_schema sql_identifier, specific_name sql_identifier");
    IsCollationCharacterSetApplicability = (INF, "collation_character_set_applicability", 13390, "collation_catalog sql_identifier, collation_schema sql_identifier, collation_name sql_identifier, character_set_catalog sql_identifier, character_set_schema sql_identifier, character_set_name sql_identifier");
    IsCollations = (INF, "collations", 13385, "collation_catalog sql_identifier, collation_schema sql_identifier, collation_name sql_identifier, pad_attribute character_data");
    IsColumnColumnUsage = (INF, "column_column_usage", 13395, "table_catalog sql_identifier, table_schema sql_identifier, table_name sql_identifier, column_name sql_identifier, dependent_column sql_identifier");
    IsColumnDomainUsage = (INF, "column_domain_usage", 13400, "domain_catalog sql_identifier, domain_schema sql_identifier, domain_name sql_identifier, table_catalog sql_identifier, table_schema sql_identifier, table_name sql_identifier, column_name sql_identifier");
    IsColumnOptions = (INF, "column_options", 13622, "table_catalog sql_identifier, table_schema sql_identifier, table_name sql_identifier, column_name sql_identifier, option_name sql_identifier, option_value character_data");
    IsColumnUdtUsage = (INF, "column_udt_usage", 13410, "udt_catalog sql_identifier, udt_schema sql_identifier, udt_name sql_identifier, table_catalog sql_identifier, table_schema sql_identifier, table_name sql_identifier, column_name sql_identifier");
    IsConstraintTableUsage = (INF, "constraint_table_usage", 13425, "table_catalog sql_identifier, table_schema sql_identifier, table_name sql_identifier, constraint_catalog sql_identifier, constraint_schema sql_identifier, constraint_name sql_identifier");
    IsDataTypePrivileges = (INF, "data_type_privileges", 13607, "object_catalog sql_identifier, object_schema sql_identifier, object_name sql_identifier, object_type character_data, dtd_identifier sql_identifier");
    IsDomainConstraints = (INF, "domain_constraints", 13430, "constraint_catalog sql_identifier, constraint_schema sql_identifier, constraint_name sql_identifier, domain_catalog sql_identifier, domain_schema sql_identifier, domain_name sql_identifier, is_deferrable yes_or_no, initially_deferred yes_or_no");
    IsDomainUdtUsage = (INF, "domain_udt_usage", 13435, "udt_catalog sql_identifier, udt_schema sql_identifier, udt_name sql_identifier, domain_catalog sql_identifier, domain_schema sql_identifier, domain_name sql_identifier");
    IsElementTypes = (INF, "element_types", 13612, "object_catalog sql_identifier, object_schema sql_identifier, object_name sql_identifier, object_type character_data, collection_type_identifier sql_identifier, data_type character_data, character_maximum_length cardinal_number, character_octet_length cardinal_number, character_set_catalog sql_identifier, character_set_schema sql_identifier, character_set_name sql_identifier, collation_catalog sql_identifier, collation_schema sql_identifier, collation_name sql_identifier, numeric_precision cardinal_number, numeric_precision_radix cardinal_number, numeric_scale cardinal_number, datetime_precision cardinal_number, interval_type character_data, interval_precision cardinal_number, udt_catalog sql_identifier, udt_schema sql_identifier, udt_name sql_identifier, scope_catalog sql_identifier, scope_schema sql_identifier, scope_name sql_identifier, maximum_cardinality cardinal_number, dtd_identifier sql_identifier");
    IsForeignDataWrapperOptions = (INF, "foreign_data_wrapper_options", 13630, "foreign_data_wrapper_catalog sql_identifier, foreign_data_wrapper_name sql_identifier, option_name sql_identifier, option_value character_data");
    IsForeignDataWrappers = (INF, "foreign_data_wrappers", 13634, "foreign_data_wrapper_catalog sql_identifier, foreign_data_wrapper_name sql_identifier, authorization_identifier sql_identifier, library_name character_data, foreign_data_wrapper_language character_data");
    IsForeignServerOptions = (INF, "foreign_server_options", 13642, "foreign_server_catalog sql_identifier, foreign_server_name sql_identifier, option_name sql_identifier, option_value character_data");
    IsForeignServers = (INF, "foreign_servers", 13646, "foreign_server_catalog sql_identifier, foreign_server_name sql_identifier, foreign_data_wrapper_catalog sql_identifier, foreign_data_wrapper_name sql_identifier, foreign_server_type character_data, foreign_server_version character_data, authorization_identifier sql_identifier");
    IsForeignTableOptions = (INF, "foreign_table_options", 13655, "foreign_table_catalog sql_identifier, foreign_table_schema sql_identifier, foreign_table_name sql_identifier, option_name sql_identifier, option_value character_data");
    IsForeignTables = (INF, "foreign_tables", 13659, "foreign_table_catalog sql_identifier, foreign_table_schema sql_identifier, foreign_table_name sql_identifier, foreign_server_catalog sql_identifier, foreign_server_name sql_identifier");
    IsInformationSchemaCatalogName = (INF, "information_schema_catalog_name", 13347, "catalog_name sql_identifier");
    IsRoutineColumnUsage = (INF, "routine_column_usage", 13467, "specific_catalog sql_identifier, specific_schema sql_identifier, specific_name sql_identifier, routine_catalog sql_identifier, routine_schema sql_identifier, routine_name sql_identifier, table_catalog sql_identifier, table_schema sql_identifier, table_name sql_identifier, column_name sql_identifier");
    IsRoutineRoutineUsage = (INF, "routine_routine_usage", 13481, "specific_catalog sql_identifier, specific_schema sql_identifier, specific_name sql_identifier, routine_catalog sql_identifier, routine_schema sql_identifier, routine_name sql_identifier");
    IsRoutineSequenceUsage = (INF, "routine_sequence_usage", 13486, "specific_catalog sql_identifier, specific_schema sql_identifier, specific_name sql_identifier, routine_catalog sql_identifier, routine_schema sql_identifier, routine_name sql_identifier, sequence_catalog sql_identifier, sequence_schema sql_identifier, sequence_name sql_identifier");
    IsRoutineTableUsage = (INF, "routine_table_usage", 13491, "specific_catalog sql_identifier, specific_schema sql_identifier, specific_name sql_identifier, routine_catalog sql_identifier, routine_schema sql_identifier, routine_name sql_identifier, table_catalog sql_identifier, table_schema sql_identifier, table_name sql_identifier");
    IsSqlFeatures = (INF, "sql_features", 13510, "feature_id character_data, feature_name character_data, sub_feature_id character_data, sub_feature_name character_data, is_supported yes_or_no, is_verified_by character_data, comments character_data");
    IsSqlImplementationInfo = (INF, "sql_implementation_info", 13515, "implementation_info_id character_data, implementation_info_name character_data, integer_value cardinal_number, character_value character_data, comments character_data");
    IsSqlParts = (INF, "sql_parts", 13520, "feature_id character_data, feature_name character_data, is_supported yes_or_no, is_verified_by character_data, comments character_data");
    IsSqlSizing = (INF, "sql_sizing", 13525, "sizing_id cardinal_number, sizing_name character_data, supported_value cardinal_number, comments character_data");
    IsTransforms = (INF, "transforms", 13549, "udt_catalog sql_identifier, udt_schema sql_identifier, udt_name sql_identifier, specific_catalog sql_identifier, specific_schema sql_identifier, specific_name sql_identifier, group_name sql_identifier, transform_type character_data");
    IsTriggeredUpdateColumns = (INF, "triggered_update_columns", 13554, "trigger_catalog sql_identifier, trigger_schema sql_identifier, trigger_name sql_identifier, event_object_catalog sql_identifier, event_object_schema sql_identifier, event_object_table sql_identifier, event_object_column sql_identifier");
    IsUserDefinedTypes = (INF, "user_defined_types", 13582, "user_defined_type_catalog sql_identifier, user_defined_type_schema sql_identifier, user_defined_type_name sql_identifier, user_defined_type_category character_data, is_instantiable yes_or_no, is_final yes_or_no, ordering_form character_data, ordering_category character_data, ordering_routine_catalog sql_identifier, ordering_routine_schema sql_identifier, ordering_routine_name sql_identifier, reference_type character_data, data_type character_data, character_maximum_length cardinal_number, character_octet_length cardinal_number, character_set_catalog sql_identifier, character_set_schema sql_identifier, character_set_name sql_identifier, collation_catalog sql_identifier, collation_schema sql_identifier, collation_name sql_identifier, numeric_precision cardinal_number, numeric_precision_radix cardinal_number, numeric_scale cardinal_number, datetime_precision cardinal_number, interval_type character_data, interval_precision cardinal_number, source_dtd_identifier sql_identifier, ref_dtd_identifier sql_identifier");
    IsUserMappingOptions = (INF, "user_mapping_options", 13668, "authorization_identifier sql_identifier, foreign_server_catalog sql_identifier, foreign_server_name sql_identifier, option_name sql_identifier, option_value character_data");
    IsUserMappings = (INF, "user_mappings", 13673, "authorization_identifier sql_identifier, foreign_server_catalog sql_identifier, foreign_server_name sql_identifier");
    IsViewColumnUsage = (INF, "view_column_usage", 13587, "view_catalog sql_identifier, view_schema sql_identifier, view_name sql_identifier, table_catalog sql_identifier, table_schema sql_identifier, table_name sql_identifier, column_name sql_identifier");
    IsViewRoutineUsage = (INF, "view_routine_usage", 13592, "table_catalog sql_identifier, table_schema sql_identifier, table_name sql_identifier, specific_catalog sql_identifier, specific_schema sql_identifier, specific_name sql_identifier");
    IsViewTableUsage = (INF, "view_table_usage", 13597, "view_catalog sql_identifier, view_schema sql_identifier, view_name sql_identifier, table_catalog sql_identifier, table_schema sql_identifier, table_name sql_identifier");
    PgAios = (PGC, "pg_aios", 12355, "pid int4, io_id int4, io_generation int8, state text, operation text, off int8, length int8, target text, handle_data_len int2, raw_result int4, result text, target_desc text, f_sync bool, f_localmem bool, f_buffered bool");
    PgAmop = (PGC, "pg_amop", 2602, "oid oid, amopfamily oid, amoplefttype oid, amoprighttype oid, amopstrategy int2, amoppurpose char1, amopopr oid, amopmethod oid, amopsortfamily oid");
    PgAmproc = (PGC, "pg_amproc", 2603, "oid oid, amprocfamily oid, amproclefttype oid, amprocrighttype oid, amprocnum int2, amproc regproc");
    PgAvailableExtensionVersions = (PGC, "pg_available_extension_versions", 12085, "name name, version text, installed bool, superuser bool, trusted bool, relocatable bool, schema name, requires name[], comment text");
    PgBackendMemoryContexts = (PGC, "pg_backend_memory_contexts", 12142, "name text, ident text, type text, level int4, path int4[], total_bytes int8, total_nblocks int8, free_bytes int8, free_chunks int8, used_bytes int8");
    PgConfig = (PGC, "pg_config", 12130, "name text, setting text");
    PgConversion = (PGC, "pg_conversion", 2607, "oid oid, conname name, connamespace oid, conowner oid, conforencoding int4, contoencoding int4, conproc regproc, condefault bool");
    PgFileSettings = (PGC, "pg_file_settings", 12110, "sourcefile text, sourceline int4, seqno int4, name text, setting text, applied bool, error text");
    PgForeignDataWrapper = (PGC, "pg_foreign_data_wrapper", 2328, "oid oid, fdwname name, fdwowner oid, fdwhandler oid, fdwvalidator oid, fdwacl aclitem[], fdwoptions text[]");
    PgForeignServer = (PGC, "pg_foreign_server", 1417, "oid oid, srvname name, srvowner oid, srvfdw oid, srvtype text, srvversion text, srvacl aclitem[], srvoptions text[]");
    PgGroup = (PGC, "pg_group", 12010, "groname name, grosysid oid, grolist oid[]");
    PgHbaFileRules = (PGC, "pg_hba_file_rules", 12114, "rule_number int4, file_name text, line_number int4, type text, database text[], user_name text[], address text, netmask text, auth_method text, options text[], error text");
    PgIdentFileMappings = (PGC, "pg_ident_file_mappings", 12118, "map_number int4, file_name text, line_number int4, map_name text, sys_name text, pg_username text, error text");
    PgParameterAcl = (PGC, "pg_parameter_acl", 6243, "oid oid, parname text, paracl aclitem[]");
    PgPreparedXacts = (PGC, "pg_prepared_xacts", 12090, "transaction xid, gid text, prepared timestamptz, owner name, database name");
    PgPublicationTables = (PGC, "pg_publication_tables", 12068, "pubname name, schemaname name, tablename name, attnames name[], rowfilter text");
    PgReplicationOrigin = (PGC, "pg_replication_origin", 6000, "roident oid, roname text");
    PgReplicationOriginStatus = (PGC, "pg_replication_origin_status", 12343, "local_id oid, external_id text, remote_lsn pg_lsn, local_lsn pg_lsn");
    PgRules = (PGC, "pg_rules", 12023, "schemaname name, tablename name, rulename name, definition text");
    PgSeclabel = (PGC, "pg_seclabel", 3596, "objoid oid, classoid oid, objsubid int4, provider text, label text");
    PgSeclabels = (PGC, "pg_seclabels", 12099, "objoid oid, classoid oid, objsubid int4, objtype text, objnamespace oid, objname text, provider text, label text");
    PgShadow = (PGC, "pg_shadow", 12005, "usename name, usesysid oid, usecreatedb bool, usesuper bool, userepl bool, usebypassrls bool, passwd text, valuntil timestamptz, useconfig text[]");
    PgShmemAllocations = (PGC, "pg_shmem_allocations", 12134, "name text, off int8, size int8, allocated_size int8");
    PgShmemAllocationsNuma = (PGC, "pg_shmem_allocations_numa", 12138, "name text, numa_node int4, size int8");
    PgShseclabel = (PGC, "pg_shseclabel", 3592, "objoid oid, classoid oid, provider text, label text");
    PgStatAllIndexes = (PGC, "pg_stat_all_indexes", 12187, "relid oid, indexrelid oid, schemaname name, relname name, indexrelname name, idx_scan int8, last_idx_scan timestamptz, idx_tup_read int8, idx_tup_fetch int8");
    PgStatAllTables = (PGC, "pg_stat_all_tables", 12146, "relid oid, schemaname name, relname name, seq_scan int8, last_seq_scan timestamptz, seq_tup_read int8, idx_scan int8, last_idx_scan timestamptz, idx_tup_fetch int8, n_tup_ins int8, n_tup_upd int8, n_tup_del int8, n_tup_hot_upd int8, n_tup_newpage_upd int8, n_live_tup int8, n_dead_tup int8, n_mod_since_analyze int8, n_ins_since_vacuum int8, last_vacuum timestamptz, last_autovacuum timestamptz, last_analyze timestamptz, last_autoanalyze timestamptz, vacuum_count int8, autovacuum_count int8, analyze_count int8, autoanalyze_count int8, total_vacuum_time float8, total_autovacuum_time float8, total_analyze_time float8, total_autoanalyze_time float8");
    PgStatArchiver = (PGC, "pg_stat_archiver", 12289, "archived_count int8, last_archived_wal text, last_archived_time timestamptz, failed_count int8, last_failed_wal text, last_failed_time timestamptz, stats_reset timestamptz");
    PgStatBgwriter = (PGC, "pg_stat_bgwriter", 12293, "buffers_clean int8, maxwritten_clean int8, buffers_alloc int8, stats_reset timestamptz");
    PgStatCheckpointer = (PGC, "pg_stat_checkpointer", 12297, "num_timed int8, num_requested int8, num_done int8, restartpoints_timed int8, restartpoints_req int8, restartpoints_done int8, write_time float8, sync_time float8, buffers_written int8, slru_written int8, stats_reset timestamptz");
    PgStatDatabaseConflicts = (PGC, "pg_stat_database_conflicts", 12275, "datid oid, datname name, confl_tablespace int8, confl_lock int8, confl_snapshot int8, confl_bufferpin int8, confl_deadlock int8, confl_active_logicalslot int8");
    PgStatGssapi = (PGC, "pg_stat_gssapi", 12257, "pid int4, gss_authenticated bool, principal text, encrypted bool, credentials_delegated bool");
    PgStatIo = (PGC, "pg_stat_io", 12301, "backend_type text, object text, context text, reads int8, read_bytes numeric, read_time float8, writes int8, write_bytes numeric, write_time float8, writebacks int8, writeback_time float8, extends int8, extend_bytes numeric, extend_time float8, hits int8, evictions int8, reuses int8, fsyncs int8, fsync_time float8, stats_reset timestamptz");
    PgStatProgressAnalyze = (PGC, "pg_stat_progress_analyze", 12309, "pid int4, datid oid, datname name, relid oid, phase text, sample_blks_total int8, sample_blks_scanned int8, ext_stats_total int8, ext_stats_computed int8, child_tables_total int8, child_tables_done int8, current_child_table_relid oid, delay_time float8");
    PgStatProgressBasebackup = (PGC, "pg_stat_progress_basebackup", 12329, "pid int4, phase text, backup_total int8, backup_streamed int8, tablespaces_total int8, tablespaces_streamed int8");
    PgStatProgressCluster = (PGC, "pg_stat_progress_cluster", 12319, "pid int4, datid oid, datname name, relid oid, command text, phase text, cluster_index_relid oid, heap_tuples_scanned int8, heap_tuples_written int8, heap_blks_total int8, heap_blks_scanned int8, index_rebuild_count int8");
    PgStatProgressCopy = (PGC, "pg_stat_progress_copy", 12333, "pid int4, datid oid, datname name, relid oid, command text, type text, bytes_processed int8, bytes_total int8, tuples_processed int8, tuples_excluded int8, tuples_skipped int8");
    PgStatProgressCreateIndex = (PGC, "pg_stat_progress_create_index", 12324, "pid int4, datid oid, datname name, relid oid, index_relid oid, command text, phase text, lockers_total int8, lockers_done int8, current_locker_pid int8, blocks_total int8, blocks_done int8, tuples_total int8, tuples_done int8, partitions_total int8, partitions_done int8");
    PgStatProgressVacuum = (PGC, "pg_stat_progress_vacuum", 12314, "pid int4, datid oid, datname name, relid oid, phase text, heap_blks_total int8, heap_blks_scanned int8, heap_blks_vacuumed int8, index_vacuum_count int8, max_dead_tuple_bytes int8, dead_tuple_bytes int8, num_dead_item_ids int8, indexes_total int8, indexes_processed int8, delay_time float8");
    PgStatRecoveryPrefetch = (PGC, "pg_stat_recovery_prefetch", 12244, "stats_reset timestamptz, prefetch int8, hit int8, skip_init int8, skip_new int8, skip_fpw int8, skip_rep int8, wal_distance int4, block_distance int4, io_depth int4");
    PgStatReplicationSlots = (PGC, "pg_stat_replication_slots", 12266, "slot_name text, spill_txns int8, spill_count int8, spill_bytes int8, stream_txns int8, stream_count int8, stream_bytes int8, total_txns int8, total_bytes int8, stats_reset timestamptz");
    PgStatSlru = (PGC, "pg_stat_slru", 12236, "name text, blks_zeroed int8, blks_hit int8, blks_read int8, blks_written int8, blks_exists int8, flushes int8, truncates int8, stats_reset timestamptz");
    PgStatSsl = (PGC, "pg_stat_ssl", 12253, "pid int4, ssl bool, version text, cipher text, bits int4, client_dn text, client_serial numeric, issuer_dn text");
    PgStatSubscription = (PGC, "pg_stat_subscription", 12248, "subid oid, subname name, worker_type text, pid int4, leader_pid int4, relid oid, received_lsn pg_lsn, last_msg_send_time timestamptz, last_msg_receipt_time timestamptz, latest_end_lsn pg_lsn, latest_end_time timestamptz");
    PgStatSubscriptionStats = (PGC, "pg_stat_subscription_stats", 12347, "subid oid, subname name, apply_error_count int8, sync_error_count int8, confl_insert_exists int8, confl_update_origin_differs int8, confl_update_exists int8, confl_update_missing int8, confl_delete_origin_differs int8, confl_delete_missing int8, confl_multiple_unique_conflicts int8, stats_reset timestamptz");
    PgStatSysIndexes = (PGC, "pg_stat_sys_indexes", 12192, "relid oid, indexrelid oid, schemaname name, relname name, indexrelname name, idx_scan int8, last_idx_scan timestamptz, idx_tup_read int8, idx_tup_fetch int8");
    PgStatSysTables = (PGC, "pg_stat_sys_tables", 12156, "relid oid, schemaname name, relname name, seq_scan int8, last_seq_scan timestamptz, seq_tup_read int8, idx_scan int8, last_idx_scan timestamptz, idx_tup_fetch int8, n_tup_ins int8, n_tup_upd int8, n_tup_del int8, n_tup_hot_upd int8, n_tup_newpage_upd int8, n_live_tup int8, n_dead_tup int8, n_mod_since_analyze int8, n_ins_since_vacuum int8, last_vacuum timestamptz, last_autovacuum timestamptz, last_analyze timestamptz, last_autoanalyze timestamptz, vacuum_count int8, autovacuum_count int8, analyze_count int8, autoanalyze_count int8, total_vacuum_time float8, total_autovacuum_time float8, total_analyze_time float8, total_autoanalyze_time float8");
    PgStatUserFunctions = (PGC, "pg_stat_user_functions", 12279, "funcid oid, schemaname name, funcname name, calls int8, total_time float8, self_time float8");
    PgStatWal = (PGC, "pg_stat_wal", 12305, "wal_records int8, wal_fpi int8, wal_bytes numeric, wal_buffers_full int8, stats_reset timestamptz");
    PgStatWalReceiver = (PGC, "pg_stat_wal_receiver", 12240, "pid int4, status text, receive_start_lsn pg_lsn, receive_start_tli int4, written_lsn pg_lsn, flushed_lsn pg_lsn, received_tli int4, last_msg_send_time timestamptz, last_msg_receipt_time timestamptz, latest_end_lsn pg_lsn, latest_end_time timestamptz, slot_name text, sender_host text, sender_port int4, conninfo text");
    PgStatXactAllTables = (PGC, "pg_stat_xact_all_tables", 12151, "relid oid, schemaname name, relname name, seq_scan int8, seq_tup_read int8, idx_scan int8, idx_tup_fetch int8, n_tup_ins int8, n_tup_upd int8, n_tup_del int8, n_tup_hot_upd int8, n_tup_newpage_upd int8");
    PgStatXactSysTables = (PGC, "pg_stat_xact_sys_tables", 12161, "relid oid, schemaname name, relname name, seq_scan int8, seq_tup_read int8, idx_scan int8, idx_tup_fetch int8, n_tup_ins int8, n_tup_upd int8, n_tup_del int8, n_tup_hot_upd int8, n_tup_newpage_upd int8");
    PgStatXactUserFunctions = (PGC, "pg_stat_xact_user_functions", 12284, "funcid oid, schemaname name, funcname name, calls int8, total_time float8, self_time float8");
    PgStatXactUserTables = (PGC, "pg_stat_xact_user_tables", 12170, "relid oid, schemaname name, relname name, seq_scan int8, seq_tup_read int8, idx_scan int8, idx_tup_fetch int8, n_tup_ins int8, n_tup_upd int8, n_tup_del int8, n_tup_hot_upd int8, n_tup_newpage_upd int8");
    PgStatioAllIndexes = (PGC, "pg_statio_all_indexes", 12200, "relid oid, indexrelid oid, schemaname name, relname name, indexrelname name, idx_blks_read int8, idx_blks_hit int8");
    PgStatioAllSequences = (PGC, "pg_statio_all_sequences", 12213, "relid oid, schemaname name, relname name, blks_read int8, blks_hit int8");
    PgStatioAllTables = (PGC, "pg_statio_all_tables", 12174, "relid oid, schemaname name, relname name, heap_blks_read int8, heap_blks_hit int8, idx_blks_read int8, idx_blks_hit int8, toast_blks_read int8, toast_blks_hit int8, tidx_blks_read int8, tidx_blks_hit int8");
    PgStatioSysIndexes = (PGC, "pg_statio_sys_indexes", 12205, "relid oid, indexrelid oid, schemaname name, relname name, indexrelname name, idx_blks_read int8, idx_blks_hit int8");
    PgStatioSysSequences = (PGC, "pg_statio_sys_sequences", 12218, "relid oid, schemaname name, relname name, blks_read int8, blks_hit int8");
    PgStatioSysTables = (PGC, "pg_statio_sys_tables", 12179, "relid oid, schemaname name, relname name, heap_blks_read int8, heap_blks_hit int8, idx_blks_read int8, idx_blks_hit int8, toast_blks_read int8, toast_blks_hit int8, tidx_blks_read int8, tidx_blks_hit int8");
    PgStatioUserIndexes = (PGC, "pg_statio_user_indexes", 12209, "relid oid, indexrelid oid, schemaname name, relname name, indexrelname name, idx_blks_read int8, idx_blks_hit int8");
    PgStatioUserSequences = (PGC, "pg_statio_user_sequences", 12222, "relid oid, schemaname name, relname name, blks_read int8, blks_hit int8");
    PgSubscription = (PGC, "pg_subscription", 6100, "oid oid, subdbid oid, subskiplsn pg_lsn, subname name, subowner oid, subenabled bool, subbinary bool, substream char1, subtwophasestate char1, subdisableonerr bool, subpasswordrequired bool, subrunasowner bool, subfailover bool, subconninfo text, subslotname name, subsynccommit text, subpublications text[], suborigin text");
    PgSubscriptionRel = (PGC, "pg_subscription_rel", 6102, "srsubid oid, srrelid oid, srsubstate char1, srsublsn pg_lsn");
    PgTimezoneAbbrevs = (PGC, "pg_timezone_abbrevs", 12122, "abbrev text, utc_offset interval, is_dst bool");
    PgTransform = (PGC, "pg_transform", 3576, "oid oid, trftype oid, trflang oid, trffromsql regproc, trftosql regproc");
    PgUserMapping = (PGC, "pg_user_mapping", 1418, "oid oid, umuser oid, umserver oid, umoptions text[]");
    PgUserMappings = (PGC, "pg_user_mappings", 12338, "umid oid, srvid oid, srvname name, umuser oid, usename name, umoptions text[]");
    PgWaitEvents = (PGC, "pg_wait_events", 12351, "type text, name text, description text");
}

impl SysTable {
    fn entry(self) -> &'static (SysTable, u32, &'static str, u32, &'static str) {
        ALL.iter().find(|e| e.0 == self).expect("system table")
    }

    pub fn name(self) -> &'static str {
        self.entry().2
    }

    pub fn schema(self) -> u32 {
        self.entry().1
    }

    pub fn oid(self) -> u32 {
        self.entry().3
    }

    pub fn is_view(self) -> bool {
        let oid = self.oid();
        super::sys_rels::SYS_RELS
            .iter()
            .find(|r| r.2 == oid)
            .map_or(oid >= 12000, |r| r.5 == 'v')
    }

    pub fn all() -> impl Iterator<Item = SysTable> {
        ALL.iter().map(|e| e.0)
    }

    pub fn by_oid(oid: u32) -> Option<SysTable> {
        ALL.iter().find(|e| e.3 == oid).map(|e| e.0)
    }

    /// Find by name: an explicit schema, or pg_catalog (always searched) / information_schema
    /// when it's on the search path.
    pub fn lookup(schema: Option<&str>, name: &str, search_path: &[String]) -> Option<SysTable> {
        let schemas: Vec<u32> = match schema {
            Some("pg_catalog") => vec![PGC],
            Some("information_schema") => vec![INF],
            Some(_) => return None,
            None => {
                let mut v = vec![PGC];
                if search_path.iter().any(|s| s == "information_schema") {
                    v.push(INF);
                }
                v
            }
        };
        ALL.iter()
            .find(|e| e.2 == name && schemas.contains(&e.1))
            .map(|e| e.0)
    }

    /// The relation's row type (its pg_type entry), for those that have one.
    pub fn row_type(self) -> Option<DataType> {
        let oid = self.oid();
        let r = super::sys_rels::SYS_RELS
            .iter()
            .find(|r| r.2 == oid && r.3 != 0)?;
        Some(DataType::Row(Arc::new(crate::types::RowType {
            oid: r.3,
            array_oid: r.4,
            name: Some(Arc::from(r.1)),
            fields: (*self.columns()).clone(),
        })))
    }

    /// The relation whose row type `oid` is.
    pub fn by_rowtype(oid: u32) -> Option<SysTable> {
        let r = super::sys_rels::SYS_RELS
            .iter()
            .find(|r| r.3 == oid && oid != 0)?;
        Self::by_oid(r.2)
    }

    /// Column names and types.
    pub fn columns(self) -> Arc<Vec<(String, DataType)>> {
        static COLS: OnceLock<Vec<Arc<Vec<(String, DataType)>>>> = OnceLock::new();
        let all = COLS.get_or_init(|| ALL.iter().map(|e| Arc::new(parse_cols(e.4))).collect());
        let i = ALL.iter().position(|e| e.0 == self).unwrap();
        all[i].clone()
    }

    /// The information_schema domain each column is declared as (the column's type is its base type).
    pub fn domains(self) -> Arc<Vec<Option<u32>>> {
        static DOMAINS: OnceLock<Vec<Arc<Vec<Option<u32>>>>> = OnceLock::new();
        let all = DOMAINS.get_or_init(|| {
            ALL.iter()
                .map(|e| {
                    let doms =
                        e.4.split(", ")
                            .map(|c| match c.split_once(' ').map(|x| x.1) {
                                Some("cardinal_number") => Some(13341),
                                Some("character_data") => Some(13344),
                                Some("sql_identifier") => Some(13346),
                                Some("time_stamp") => Some(13352),
                                Some("yes_or_no") => Some(13354),
                                _ => None,
                            });
                    Arc::new(doms.collect())
                })
                .collect()
        });
        let i = ALL.iter().position(|e| e.0 == self).unwrap();
        all[i].clone()
    }
}

fn parse_cols(spec: &str) -> Vec<(String, DataType)> {
    spec.split(", ")
        .map(|c| {
            let (name, ty) = c.split_once(' ').expect("column spec");
            let (base, array) = match ty.strip_suffix("[]") {
                Some(b) => (b, true),
                None => (ty, false),
            };
            let t = match base {
                "char1" => DataType::Char,
                // information_schema's domains, as their base types.
                "yes_or_no" => DataType::Varchar(7),
                "time_stamp" => DataType::Timestamptz(2),
                "sql_identifier" => DataType::Name,
                "character_data" => DataType::Varchar(-1),
                "cardinal_number" => DataType::Int4,
                // pg_statistic_ext_data.stxdexpr: pg_statistic rows.
                "pg_statistic" => {
                    let spec = ALL.iter().find(|e| e.0 == SysTable::PgStatistic).unwrap().4;
                    DataType::Row(Arc::new(crate::types::RowType {
                        oid: 10029,
                        array_oid: 10028,
                        name: Some(Arc::from("pg_statistic")),
                        fields: parse_cols(spec),
                    }))
                }
                _ => DataType::from_name(base).unwrap_or_else(|| panic!("type {base}")),
            };
            (
                name.to_string(),
                if array { DataType::array_of(t) } else { t },
            )
        })
        .collect()
}

// ── value helpers ──

fn nm(s: impl AsRef<str>) -> Value {
    Value::Text(Arc::from(s.as_ref()))
}
fn o(x: u32) -> Value {
    Value::Oid(x)
}
fn b(x: bool) -> Value {
    Value::Bool(x)
}
fn c(x: char) -> Value {
    Value::Char(x as u8)
}
fn i2(x: i64) -> Value {
    Value::Int2(x as i16)
}
fn i4(x: i64) -> Value {
    Value::Int4(x as i32)
}
fn i8v(x: i64) -> Value {
    Value::Int8(x)
}
fn opt(s: Option<impl AsRef<str>>) -> Value {
    s.map(nm).unwrap_or(Value::Null)
}
fn yn(x: bool) -> Value {
    nm(if x { "YES" } else { "NO" })
}
fn arr(elem: DataType, items: Vec<Value>) -> Value {
    Value::Array(Arc::new(crate::types::array::ArrayValue::from_vec(
        elem, items,
    )))
}

/// A pg_statistic slot's stanumbers.
fn stat_numbers(slot: &crate::catalog::StatSlot) -> Value {
    match &slot.numbers {
        Some(n) => arr(
            DataType::Float4,
            n.iter().map(|f| Value::Float4(*f)).collect(),
        ),
        None => N,
    }
}

/// A pg_statistic slot's stavalues (an array of arrays shows its elements' text).
fn stat_values(slot: &crate::catalog::StatSlot) -> Value {
    let Some((ty, items)) = &slot.values else {
        return N;
    };
    if matches!(ty, DataType::Array(_)) {
        return arr(
            DataType::Text,
            items
                .iter()
                .map(|s| Value::Text(s.as_str().into()))
                .collect(),
        );
    }
    let fmt = crate::types::text::FormatCtx::default();
    arr(
        ty.clone(),
        items
            .iter()
            .map(|s| {
                crate::types::text::parse_value(s, ty, &fmt)
                    .unwrap_or_else(|_| Value::Text(s.as_str().into()))
            })
            .collect(),
    )
}

/// A pg_statistic row of relation `rel` (0 for statistics objects' expressions).
fn statistic_row(rel: u32, st: &super::ColStats) -> Row {
    let mut row = vec![
        o(rel),
        i2(st.attnum as i64),
        b(st.inherit),
        Value::Float4(st.nullfrac),
        i4(st.width as i64),
        Value::Float4(st.distinct),
    ];
    let slot = |i: usize| st.slots.get(i);
    row.extend((0..5).map(|i| i2(slot(i).map_or(0, |x| x.kind as i64))));
    row.extend((0..5).map(|i| o(slot(i).map_or(0, |x| x.op))));
    row.extend((0..5).map(|i| o(slot(i).map_or(0, |x| x.coll))));
    row.extend((0..5).map(|i| slot(i).map_or(N, stat_numbers)));
    row.extend((0..5).map(|i| slot(i).map_or(N, stat_values)));
    row
}

fn opt_bytes(b: &Option<Vec<u8>>) -> Value {
    b.as_ref().map_or(N, |b| Value::Bytea(b.as_slice().into()))
}

/// pg_statistic_ext_data.stxdexpr: a pg_statistic row per expression.
fn expr_stats(d: &super::StatExtData) -> Value {
    let Some(exprs) = &d.exprs else { return N };
    let ty = SysTable::PgStatistic
        .row_type()
        .expect("pg_statistic row type");
    arr(
        ty,
        exprs
            .iter()
            .map(|e| {
                e.as_ref()
                    .map_or(N, |cs| Value::Record(Arc::new(statistic_row(0, cs))))
            })
            .collect(),
    )
}

/// An MCV list's dimension types (as the engine handles them) and items.
pub fn mcv_items(
    cat: &Catalog,
    b: &[u8],
) -> Option<(Vec<DataType>, Vec<crate::types::extstats::McvItem>)> {
    let type_of = |oid: u32| {
        DataType::from_oid(oid)
            .or_else(|| cat.user_data_type(oid))
            .or_else(|| cat.domains.get(&oid).map(|d| d.base.clone()))
    };
    let label = |ty: u32, label: u32| {
        cat.enums.get(&ty).and_then(|e| {
            (0..e.labels.len())
                .find(|i| e.label_oid(*i) == label)
                .map(|i| e.labels[i].clone())
        })
    };
    let (types, items) = crate::types::extstats::mcv_items(b, &type_of, &label)?;
    Some((
        types
            .iter()
            .map(|o| type_of(*o).unwrap_or(DataType::Text))
            .collect(),
        items,
    ))
}

/// The built-in casts as pg_cast lists them: (oid, source, target).
pub(crate) fn builtin_casts() -> Vec<(u32, DataType, DataType)> {
    let rows = super::static_rows::rows(SysTable::PgCast);
    let at = super::static_rows::oid_at;
    rows.iter()
        .filter_map(|r| {
            Some((
                at(r, 0),
                DataType::from_oid(at(r, 1))?,
                DataType::from_oid(at(r, 2))?,
            ))
        })
        .collect()
}

/// A built-in cast's method (pg_cast.castmethod: 'f' function, 'b' binary, 'i' I/O) and
/// function.
pub(crate) fn builtin_cast_method(from: u32, to: u32) -> Option<(char, u32)> {
    let rows = super::static_rows::rows(SysTable::PgCast);
    let at = super::static_rows::oid_at;
    rows.iter()
        .find(|r| at(r, 1) == from && at(r, 2) == to)
        .map(|r| {
            let m = match r.get(5) {
                Some(Value::Char(c)) => *c as char,
                Some(Value::Text(t)) => t.chars().next().unwrap_or('f'),
                _ => 'f',
            };
            (m, at(r, 3))
        })
}

/// The oid of domain `d`'s check `i` (63: its NOT NULL constraint).
pub fn domain_constraint_oid(d: &super::Domain, i: usize) -> u32 {
    let name = if i == 63 {
        d.not_null_name
            .clone()
            .unwrap_or_else(|| format!("{}_not_null", d.name))
    } else {
        d.checks.get(i).map(|c| c.0.clone()).unwrap_or_default()
    };
    // Domains from before constraints had oids.
    d.con_oids.iter().find(|x| x.0 == name).map_or_else(
        || d.oid.wrapping_mul(64).wrapping_add(i as u32) | 0x4000_0000,
        |x| x.1,
    )
}

/// A Postgres-exported ACL with its bootstrap superuser (`postgres`) named as ours is.
fn boot_acl(v: &Value, boot: &str) -> Value {
    match v {
        Value::Array(a) if boot != "postgres" => arr(
            DataType::Aclitem,
            a.items
                .iter()
                .map(|v| {
                    nm(rename_acl_role(
                        super::static_rows::text_at(std::slice::from_ref(v), 0),
                        "postgres",
                        boot,
                    ))
                })
                .collect(),
        ),
        v => v.clone(),
    }
}

/// An aclitem's text with role `from` (grantee or grantor) renamed to `to`.
fn rename_acl_role(item: &str, from: &str, to: &str) -> String {
    let Some((grantee, rest)) = item.split_once('=') else {
        return item.to_string();
    };
    let Some((privs, grantor)) = rest.split_once('/') else {
        return item.to_string();
    };
    let ren = |r: &str| {
        if r == from {
            super::quote_ident(to)
        } else {
            r.to_string()
        }
    };
    format!("{}={privs}/{}", ren(grantee), ren(grantor))
}

/// An int2vector / oidvector: subscripts start at 0.
fn vector(elem: DataType, items: Vec<Value>) -> Value {
    let dims = if items.is_empty() {
        vec![]
    } else {
        vec![(items.len(), 0)]
    };
    Value::Array(Arc::new(crate::types::array::ArrayValue {
        elem,
        dims,
        items,
    }))
}

pub fn access_method_oid(name: &str) -> u32 {
    match name {
        "hash" => 405,
        "gist" => 783,
        "gin" => 2742,
        "spgist" => 4000,
        "brin" => 3580,
        _ => 403,
    }
}

/// The opclass input type a key of type `ty` is indexed as.
fn opclass_input(ty: &DataType) -> u32 {
    match ty {
        DataType::Varchar(_) => 25,
        DataType::Enum(..) => 3500,
        DataType::Array(_) => 2277,
        DataType::Row(_) | DataType::Record => 2249,
        DataType::Range(_) => 3831,
        DataType::Multirange(_) => 4537,
        t => t.oid(),
    }
}

/// A partition's bound as pg_get_expr(relpartbound) shows it.
pub fn partition_bound_text(
    cat: &Catalog,
    oid: u32,
    fmt: &crate::types::text::FormatCtx,
) -> Option<String> {
    let t = cat.tables.get(&oid)?;
    let bound = t.partbound.as_ref()?;
    let parent = cat.tables.get(t.inherits.first()?)?;
    let types = parent
        .partkey
        .as_ref()
        .map(|k| k.types())
        .unwrap_or_default();
    Some(super::partition::bound_text(bound, &types, fmt))
}

/// The default operator class of `method` for a column of type `ty` (0 if none).
pub fn default_opclass(method: u32, ty: &DataType) -> u32 {
    let input = opclass_input(ty);
    OPCLASSES
        .iter()
        .find(|(_, m, _, _, i, _, d)| *d && *m == method && *i == input)
        .map_or(0, |c| c.0)
}

/// Whether an opclass taking `intype` can index a key of type `ty` (binary-coercibly).
pub fn opclass_accepts(intype: u32, ty: &DataType) -> bool {
    intype == ty.oid()
        || intype == opclass_input(ty)
        || matches!(ty, DataType::Varchar(_) | DataType::Text | DataType::Name)
            && matches!(intype, 25 | 1043)
        // (text and varchar are binary-coercible to bpchar.)
        || matches!(ty, DataType::Varchar(_) | DataType::Text) && intype == 1042
        // (cidr is binary-coercible to inet.)
        || matches!(ty, DataType::Cidr) && intype == 869
}

/// Whether `opclass` is the default for its input type.
pub fn opclass_name(oid: u32) -> Option<(&'static str, bool)> {
    OPCLASSES.iter().find(|c| c.0 == oid).map(|c| (c.2, c.6))
}

/// A built-in operator class's input type (opcintype).
pub fn opclass_intype(oid: u32) -> Option<u32> {
    OPCLASSES.iter().find(|c| c.0 == oid).map(|c| c.4)
}
/// Oid of a built-in collation.
/// A built-in collation's name.
pub fn collation_name(oid: u32) -> Option<&'static str> {
    crate::types::collation::static_by_oid(oid).map(|d| d.name.as_str())
}

pub fn collation_oid(name: &str) -> Option<u32> {
    crate::types::collation::static_by_name(name).map(|d| d.oid)
}

/// The collation a value of `ty` has by default (0: not collatable).
pub fn type_collation(ty: &DataType) -> u32 {
    match ty {
        DataType::Name => 950,
        DataType::Array(e) => type_collation(e),
        t if t.collatable() => 100,
        _ => 0,
    }
}

/// A column's collation: its `COLLATE` clause, or its type's.
pub fn column_collation(col: &super::Column) -> u32 {
    match &col.collation {
        Some(c) if type_collation(&col.ty) != 0 => collation_oid(c).unwrap_or(100),
        _ => type_collation(&col.ty),
    }
}

const N: Value = Value::Null;

/// rolconfig: the role's settings for any database.
fn role_config(cat: &Catalog, role: u32) -> Value {
    match cat
        .role_settings
        .iter()
        .find(|e| e.role == role && e.database.is_none())
    {
        Some(e) => arr(
            DataType::Text,
            e.settings
                .iter()
                .map(|(k, v)| nm(format!("{k}={v}")))
                .collect(),
        ),
        None => N,
    }
}

/// An aclitem[] column: NULL for the default. One with no entries is a one-dimensional
/// array of length 0, as allocacl(0) makes it (array_length 1, not NULL).
fn acl_value(cat: &Catalog, acl: Option<&acl::Acl>) -> Value {
    match acl {
        None => N,
        Some(a) if a.is_empty() => Value::Array(Arc::new(crate::types::array::ArrayValue {
            elem: DataType::Aclitem,
            dims: vec![(0, 1)],
            items: Vec::new(),
        })),
        Some(a) => arr(
            DataType::Aclitem,
            acl::format_acl(a, &|oid| cat.roles.get(&oid).map(|r| r.name.clone()))
                .into_iter()
                .map(nm)
                .collect(),
        ),
    }
}

const OWNER: u32 = super::BOOTSTRAP_SUPERUSER_OID;

/// Oid of the type a column reports (its domain if declared with one).
fn col_type_oid(cat: &Catalog, col: &super::Column) -> u32 {
    col.domain.unwrap_or_else(|| type_oid(cat, &col.ty))
}

/// pg_type's typsubscript, typinput, typoutput, typreceive, typsend, typmodin, typmodout
/// and typanalyze of type `oid`.
fn type_procs(cat: &Catalog, oid: u32) -> [u32; 8] {
    const ARRAY: [u32; 8] = [6179, 750, 751, 2400, 2401, 0, 0, 3816];
    if let Some((_, p)) = super::type_procs::TYPE_PROCS.iter().find(|p| p.0 == oid) {
        return *p;
    }
    if super::sys_rels::SYS_RELS.iter().any(|r| r.3 == oid) {
        return [0, 2290, 2291, 2402, 2403, 0, 0, 0];
    }
    if super::sys_rels::SYS_RELS.iter().any(|r| r.4 == oid) {
        return ARRAY;
    }
    if let Some(d) = cat.base_types.get(&oid) {
        if d.is_shell() {
            // shell_in, shell_out
            return [0, 2398, 2399, 0, 0, 0, 0, 0];
        }
        return [
            d.subscript,
            d.input,
            d.output,
            d.receive,
            d.send,
            d.typmod_in,
            d.typmod_out,
            d.analyze,
        ];
    }
    if let Some(d) = cat.base_types.values().find(|d| d.ty.array_oid == oid) {
        let mut a = ARRAY;
        a[5] = d.typmod_in;
        a[6] = d.typmod_out;
        return a;
    }
    if cat.enums.contains_key(&oid) {
        [0, 3506, 3507, 3532, 3533, 0, 0, 0]
    } else if cat.composites.contains_key(&oid)
        || cat.tables.values().any(|t| t.rowtype == oid)
        || cat.views.values().any(|v| v.rowtype == oid)
    {
        [0, 2290, 2291, 2402, 2403, 0, 0, 0]
    } else if cat.ranges.values().any(|r| r.oid == oid) {
        [0, 3834, 3835, 3836, 3837, 0, 0, 3916]
    } else if cat.ranges.values().any(|r| r.multi_oid == oid) {
        [0, 4231, 4232, 4233, 4234, 0, 0, 4242]
    } else if let Some(d) = cat.domains.get(&oid) {
        let b = type_procs(cat, d.parent.unwrap_or_else(|| type_oid(cat, &d.base)));
        [0, 2597, b[2], 2598, b[4], 0, 0, b[7]]
    } else if cat.enums.values().any(|e| e.array_oid == oid)
        || cat.composites.values().any(|c| c.array_oid == oid)
        || cat.domains.values().any(|d| d.array_oid == oid)
        || cat
            .ranges
            .values()
            .any(|r| r.array_oid == oid || r.multi_array_oid == oid)
        || cat
            .tables
            .values()
            .any(|t| t.rowtype != 0 && t.array_type() == oid)
        || cat
            .views
            .values()
            .any(|v| v.rowtype != 0 && v.array_type() == oid)
    {
        ARRAY
    } else {
        [0; 8]
    }
}

pub(crate) fn type_oid(_cat: &Catalog, t: &DataType) -> u32 {
    match t {
        DataType::Array(e) => match &**e {
            DataType::Enum(_, _, a) => *a,
            e => e.array_oid(),
        },
        t => t.oid(),
    }
}

pub(crate) fn index_cols(ix: &super::Index) -> Vec<i64> {
    ix.columns
        .iter()
        .map(|c| match c.key {
            IndexKey::Column(i) => i as i64 + 1,
            IndexKey::Expr(_) => 0,
        })
        .chain(ix.include.iter().map(|i| *i as i64 + 1))
        .collect()
}

/// An index's attribute names, as ChooseIndexColumnNames picks them: the column's name,
/// `expr` for expressions, made unique with a number.
pub fn index_att_names(t: &super::Table, ix: &super::Index) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut exprs = ix.columns.iter().filter_map(|c| match &c.key {
        IndexKey::Expr(e) => Some(e),
        _ => None,
    });
    for a in index_cols(ix) {
        let base = if a > 0 {
            t.columns[a as usize - 1].name.clone()
        } else {
            let named = exprs
                .next()
                .and_then(|e| crate::sql::binder::ast::parse_expr(e).ok())
                .and_then(|n| crate::sql::binder::figure_index_colname(&n));
            super::truncate_ident(&named.unwrap_or_else(|| "expr".to_string()))
        };
        let mut name = base.clone();
        let mut n = 0;
        while out.contains(&name) {
            n += 1;
            name = format!("{base}{n}");
        }
        out.push(name);
    }
    out
}

/// Every relation visible in pg_class: (oid, name, namespace, kind, natts, has index, …).
struct RelInfo {
    oid: u32,
    name: String,
    schema: u32,
    kind: char,
    natts: usize,
    hasindex: bool,
    checks: usize,
    am: u32,
    persistence: char,
    owner: u32,
}

fn relations(cat: &Catalog) -> Vec<RelInfo> {
    let mut out = Vec::new();
    for t in cat.tables.values() {
        out.push(RelInfo {
            oid: t.oid,
            name: t.name.clone(),
            schema: t.schema,
            kind: t.relkind(),
            natts: t.live_columns().count(),
            hasindex: !t.indexes.is_empty() || t.pkey_index.is_some(),
            checks: t.checks().count(),
            am: cat
                .rel_am
                .get(&t.oid)
                .copied()
                .unwrap_or(if t.partkey.is_some() || t.foreign.is_some() { 0 } else { 2 }),
            persistence: if t.temporary {
                't'
            } else if t.unlogged {
                'u'
            } else {
                'p'
            },
            owner: t.owner,
        });
    }
    for v in cat.views.values() {
        out.push(RelInfo {
            oid: v.oid,
            name: v.name.clone(),
            schema: v.schema,
            kind: if v.materialized { 'm' } else { 'v' },
            natts: v.columns.len(),
            hasindex: v.storage.as_ref().is_some_and(|t| !t.indexes.is_empty()),
            checks: 0,
            am: if v.materialized {
                cat.rel_am.get(&v.oid).copied().unwrap_or(2)
            } else {
                0
            },
            persistence: if cat.schema_name(v.schema).starts_with("pg_temp") {
                't'
            } else {
                v.storage
                    .as_ref()
                    .map_or('p', |t| if t.unlogged { 'u' } else { 'p' })
            },
            owner: v.owner,
        });
    }
    for s in cat.sequences.values() {
        out.push(RelInfo {
            oid: s.oid,
            name: s.name.clone(),
            schema: s.schema,
            kind: 'S',
            natts: 3,
            hasindex: false,
            checks: 0,
            am: 0,
            persistence: if cat.schema_name(s.schema).starts_with("pg_temp") {
                't'
            } else if let Some(u) = s.unlogged {
                if u { 'u' } else { 'p' }
            } else if s
                .owned_by
                .and_then(|(t, _)| cat.tables.get(&t))
                .is_some_and(|t| t.unlogged)
            {
                // A serial or identity column's sequence is as persistent as its table.
                'u'
            } else {
                'p'
            },
            owner: s.owner,
        });
    }
    for i in cat.indexes.values() {
        let schema = cat
            .index_table(i.table)
            .map(|t| t.schema)
            .unwrap_or(super::PUBLIC_OID);
        out.push(RelInfo {
            oid: i.oid,
            name: i.name.clone(),
            schema,
            kind: if cat
                .index_table(i.table)
                .is_some_and(|t| t.partkey.is_some())
            {
                'I'
            } else {
                'i'
            },
            natts: i.columns.len() + i.include.len(),
            hasindex: false,
            checks: 0,
            am: cat
                .rel_am
                .get(&i.oid)
                .copied()
                .unwrap_or_else(|| access_method_oid(&i.method)),
            // An index is as persistent as its table.
            persistence: match cat.index_table(i.table) {
                Some(t) if t.temporary => 't',
                Some(t) if t.unlogged => 'u',
                _ => 'p',
            },
            owner: cat.index_table(i.table).map_or(OWNER, |t| t.owner),
        });
    }
    for t in cat.toasted() {
        let persistence = if t.temporary {
            't'
        } else if t.unlogged {
            'u'
        } else {
            'p'
        };
        for index in [false, true] {
            out.push(RelInfo {
                oid: t.toast + index as u32,
                name: super::toast_name(t.oid, index),
                schema: cat.toast_schema(t),
                kind: if index { 'i' } else { 't' },
                natts: if index { 2 } else { 3 },
                hasindex: !index,
                checks: 0,
                // A TOAST table has its table's access method.
                am: if index {
                    403
                } else {
                    cat.rel_am.get(&t.oid).copied().unwrap_or(2)
                },
                persistence,
                owner: t.owner,
            });
        }
    }
    for ct in cat.composites.values() {
        out.push(RelInfo {
            oid: ct.relid,
            name: ct.name.clone(),
            schema: ct.schema,
            kind: 'c',
            natts: ct.fields.len(),
            hasindex: false,
            checks: 0,
            am: 0,
            persistence: 'p',
            owner: ct.owner,
        });
    }
    for st in SysTable::all() {
        out.push(RelInfo {
            oid: st.oid(),
            name: st.name().to_string(),
            schema: st.schema(),
            kind: if st.is_view() { 'v' } else { 'r' },
            natts: st.columns().len(),
            hasindex: false,
            checks: 0,
            am: if st.is_view() { 0 } else { 2 },
            persistence: 'p',
            owner: OWNER,
        });
    }
    out
}

/// All columns of a relation as (attnum, name, type oid, typmod, not null, has default, column).
/// pg_type.typstorage: fixed-length types are plain; numeric and inet prefer inline
/// compression; other variable-length types are extended.
pub fn typstorage(t: &DataType) -> char {
    match t {
        DataType::Base(b) => b.storage,
        DataType::Numeric(..) | DataType::Inet | DataType::Cidr => 'm',
        DataType::Int2Vector | DataType::OidVector | DataType::TsQuery => 'p',
        _ if t.typlen() == -1 => 'x',
        _ => 'p',
    }
}

fn rel_columns(
    cat: &Catalog,
    oid: u32,
    ctx: &ExecCtx,
) -> Vec<(usize, String, DataType, Option<super::Column>)> {
    if let Some(t) = cat.tables.get(&oid) {
        return t
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| (i + 1, c.name.clone(), c.ty.clone(), Some(c.clone())))
            .collect();
    }
    if let Some(v) = cat.views.get(&oid) {
        if let Some(st) = &v.storage {
            return st
                .columns
                .iter()
                .enumerate()
                .map(|(i, c)| (i + 1, c.name.clone(), c.ty.clone(), Some(c.clone())))
                .collect();
        }
        return v
            .columns
            .iter()
            .enumerate()
            .map(|(i, (n, t))| (i + 1, n.clone(), t.clone(), None))
            .collect();
    }
    if cat.sequences.contains_key(&oid) {
        return vec![
            (1, "last_value".into(), DataType::Int8, None),
            (2, "log_cnt".into(), DataType::Int8, None),
            (3, "is_called".into(), DataType::Bool, None),
        ];
    }
    if let Some(ix) = cat.indexes.get(&oid) {
        let Some(t) = cat.index_table(ix.table) else {
            return Vec::new();
        };
        let names = index_att_names(&t, ix);
        return index_cols(ix)
            .into_iter()
            .zip(names)
            .enumerate()
            .map(|(i, (a, name))| {
                let stat = ix.columns.get(i).and_then(|c| c.stattarget);
                let ty = if a > 0 {
                    t.columns[a as usize - 1].ty.clone()
                } else {
                    // An expression column: its expression's type.
                    match ix.columns.get(i).map(|c| &c.key) {
                        Some(super::IndexKey::Expr(e)) => crate::sql::binder::ast::parse_expr(e)
                            .ok()
                            .and_then(|n| {
                                crate::sql::ruleutils::bind_stored(
                                    ctx,
                                    &t,
                                    &n,
                                    crate::sql::ruleutils::Stored::Plain,
                                )
                            })
                            .map_or(DataType::Text, |x| x.ty()),
                        _ => DataType::Text,
                    }
                };
                let ty = index_key_type(cat, ix, i, ty);
                // A plain column's storage carries over (ConstructTupleDescriptor copies the
                // heap attribute, and SET STORAGE reaches its indexes).
                let storage = (a > 0)
                    .then(|| &t.columns[a as usize - 1])
                    .filter(|c| c.ty == ty)
                    .and_then(|c| c.storage);
                let opts = ix
                    .columns
                    .get(i)
                    .map(|c| c.opclass_options.clone())
                    .unwrap_or_default();
                let col = (stat.is_some() || !opts.is_empty() || storage.is_some()).then(|| super::Column {
                    stattarget: stat,
                    options: opts,
                    storage,
                    ..super::Column::new("", ty.clone())
                });
                (i + 1, name, ty, col)
            })
            .collect();
    }
    if let Some(ct) = cat.composites.values().find(|ct| ct.relid == oid) {
        return ct
            .fields
            .iter()
            .enumerate()
            .map(|(i, (n, t))| (i + 1, n.clone(), t.clone(), None))
            .collect();
    }
    if let Some(st) = SysTable::by_oid(oid) {
        return st
            .columns()
            .iter()
            .enumerate()
            .map(|(i, (n, t))| (i + 1, n.clone(), t.clone(), None))
            .collect();
    }
    if let Some((_, index)) = cat.toast_owner(oid) {
        // A TOAST table's chunks (stored plain), and its index's key.
        let cols = [
            ("chunk_id", DataType::Oid),
            ("chunk_seq", DataType::Int4),
            ("chunk_data", DataType::Bytea),
        ];
        return cols[..if index { 2 } else { 3 }]
            .iter()
            .enumerate()
            .map(|(i, (n, t))| {
                let mut c = super::Column::new(*n, t.clone());
                c.storage = Some('p');
                (i + 1, n.to_string(), t.clone(), Some(c))
            })
            .collect();
    }
    Vec::new()
}

/// The type index column `i` stores (ConstructTupleDescriptor): its operator class's
/// opckeytype when it has one (an array's element type for `anyelement` over `anyarray`),
/// else the indexed value's type `ty`.
fn index_key_type(cat: &Catalog, ix: &super::Index, i: usize, ty: DataType) -> DataType {
    let Some(c) = ix.columns.get(i) else {
        return ty;
    };
    let opclass = if c.opclass != 0 {
        c.opclass
    } else {
        let am = cat
            .access_method_by_name(&ix.method)
            .map_or_else(|| access_method_oid(&ix.method), |a| a.oid);
        cat.default_opclass(am, &ty)
    };
    let Some(info) = cat.opclass_info(opclass) else {
        return ty;
    };
    match info.keytype {
        0 => ty,
        k if k == type_oid(cat, &ty) => ty,
        // anyelement over anyarray: the element type.
        2283 if info.intype == 2277 => ty.elem().cloned().unwrap_or(ty),
        k => DataType::from_oid(k).unwrap_or(ty),
    }
}

/// attnotnull, attcollation and attndims of a system relation's column, as Postgres has them.
/// A system catalog's (or system view's) column's collation, by name.
pub fn sys_column_collation(relid: u32, num: usize) -> Option<&'static str> {
    Some(match sys_attr(relid, num)?.1 {
        100 => "default",
        950 => "C",
        951 => "POSIX",
        _ => return None,
    })
}

fn sys_attr(relid: u32, num: usize) -> Option<(bool, u32, i64)> {
    static ATTRS: OnceLock<std::collections::HashMap<(u32, usize), (bool, u32, i64)>> =
        OnceLock::new();
    let all = ATTRS.get_or_init(|| {
        include_str!("sys_attrs.tsv")
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                Some((
                    (f[0].parse().ok()?, f[1].parse().ok()?),
                    (f[2] == "1", f[3].parse().ok()?, f[4].parse().ok()?),
                ))
            })
            .collect()
    });
    all.get(&(relid, num)).copied()
}

/// The definitions of the system views computed by running Postgres's own query (with
/// information_schema's domains as their base types): (schema, name) → query.
fn sql_views() -> &'static std::collections::HashMap<(&'static str, &'static str), String> {
    static DEFS: OnceLock<std::collections::HashMap<(&'static str, &'static str), String>> =
        OnceLock::new();
    DEFS.get_or_init(|| {
        include_str!("sys_views.tsv")
            .lines()
            .filter_map(|l| {
                let mut f = l.splitn(3, '\t');
                Some(((f.next()?, f.next()?), copy_field(f.next()?)?))
            })
            .collect()
    })
}

/// A system view's definition as Postgres's pg_get_viewdef prints it (and pretty).
pub struct SysViewDef {
    pub oid: u32,
    pub schema: String,
    pub name: String,
    pub def: String,
    pub pretty: String,
}

fn system_view_defs() -> &'static [SysViewDef] {
    static DEFS: OnceLock<Vec<SysViewDef>> = OnceLock::new();
    DEFS.get_or_init(|| {
        let raw = zstd::decode_all(&include_bytes!("data/sys_viewdefs.tsv.zst")[..])
            .expect("system view definitions");
        String::from_utf8(raw)
            .expect("system view definitions")
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                Some(SysViewDef {
                    oid: f.first()?.parse().ok()?,
                    schema: f.get(1)?.to_string(),
                    name: f.get(2)?.to_string(),
                    def: copy_field(f.get(3)?)?,
                    pretty: copy_field(f.get(4)?)?,
                })
            })
            .collect()
    })
}

/// The definition of system view `oid`, as Postgres prints it.
pub fn system_view_def(oid: u32) -> Option<&'static SysViewDef> {
    system_view_defs().iter().find(|d| d.oid == oid)
}

/// The rows of a view `sql_views` defines, as its columns' types.
fn sql_view_rows(table: SysTable, sql: &str, ctx: &ExecCtx) -> Result<Vec<Row>> {
    let cols = table.columns();
    let (rows, out) = crate::plpgsql::system_query(ctx, sql)?;
    let fmt = ctx.fmt();
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .zip(out.iter().zip(cols.iter()))
                .map(|(v, (oc, (_, ty)))| {
                    if oc.ty == *ty || v.is_null() {
                        Ok(v)
                    } else {
                        crate::types::cast::cast_value(v, &oc.ty, ty, fmt, true)
                    }
                })
                .collect()
        })
        .collect()
}

/// The errors Postgres's definition of a system view raises computing a column for some
/// of `rows` (row, column, error): a foreign table's `is_insertable_into` (tables) and
/// `is_updatable` (columns) ask its wrapper (pg_relation_is_updatable,
/// pg_column_is_updatable), which fails without a handler. Postgres computes them only for
/// the rows it returns, when the query reads them.
pub fn deferred_errors(
    table: SysTable,
    ctx: &ExecCtx,
    rows: &[Row],
) -> Vec<(usize, usize, crate::error::Error)> {
    let col = match table {
        SysTable::IsTables => 9,
        SysTable::IsColumns => 43,
        _ => return Vec::new(),
    };
    let cat = ctx.catalog();
    let failing: std::collections::HashMap<(&str, &str), crate::error::Error> = cat
        .tables
        .values()
        .filter(|t| t.is_foreign())
        .filter_map(|t| {
            let e = cat.fdw_routine_error(t)?;
            (e.code == crate::error::code::OBJECT_NOT_IN_PREREQUISITE_STATE)
                .then(|| ((cat.schema_name(t.schema), t.name.as_str()), e))
        })
        .collect();
    if failing.is_empty() {
        return Vec::new();
    }
    rows.iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let (Some(Value::Text(s)), Some(Value::Text(n))) = (r.get(1), r.get(2)) else {
                return None;
            };
            failing.get(&(&**s, &**n)).map(|e| (i, col, e.clone()))
        })
        .collect()
}

thread_local! {
    static GENERATED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How many system relations' rows this thread has generated so far: each costs as much as
/// the relation is big, so a statement's count mustn't grow with the catalog (a nested loop
/// over catalogs once generated its inner catalog for every outer row).
pub fn generated_rows_count() -> u64 {
    GENERATED.with(|c| c.get())
}

/// What a cached catalog's rows depend on: the catalog (held by a `Weak`, compared by
/// address), the current and session user, the search path and the settings' generation.
/// (One session: its database.)
struct SysRowsKey {
    table: u32,
    cat: std::sync::Weak<Catalog>,
    current_user: u32,
    session_user: u32,
    search_path: Vec<String>,
    settings: u64,
}

/// `SessionState::sys_rows`: the last few catalogs generated.
#[derive(Default)]
pub struct SysRowsCache {
    entries: std::collections::VecDeque<(SysRowsKey, std::rc::Rc<Vec<Row>>)>,
}

const SYS_ROWS_CACHED: usize = 16;

/// The catalogs whose rows are a function of the catalog, the roles, the search path and
/// the settings alone (no data, statistics, locks, other databases or large objects): what
/// psql's `\d` and ORMs' introspection read again and again.
fn rows_cacheable(table: SysTable) -> bool {
    use SysTable::*;
    matches!(
        table,
        PgClass | PgAttribute | PgAttrdef | PgType | PgNamespace | PgIndex | PgConstraint | PgProc
    )
}

fn rows_key(table: SysTable, ctx: &ExecCtx) -> SysRowsKey {
    let cat = ctx.catalog();
    SysRowsKey {
        table: table.oid(),
        cat: Arc::downgrade(&cat),
        current_user: ctx.current_user(),
        session_user: ctx.session_user(),
        search_path: ctx.search_path.clone(),
        settings: ctx.sess.settings.borrow().generation(),
    }
}

impl SysRowsKey {
    fn same(&self, o: &SysRowsKey) -> bool {
        self.table == o.table
            && std::sync::Weak::ptr_eq(&self.cat, &o.cat)
            && self.cat.strong_count() > 0
            && self.current_user == o.current_user
            && self.session_user == o.session_user
            && self.search_path == o.search_path
            && self.settings == o.settings
    }
}

/// Rows of a system relation, as the current transaction sees the catalog.
pub fn rows(table: SysTable, ctx: &ExecCtx) -> Result<Vec<Row>> {
    if !rows_cacheable(table) {
        return rows_generated(table, ctx);
    }
    let key = rows_key(table, ctx);
    let hit = ctx
        .sess
        .sys_rows
        .borrow()
        .entries
        .iter()
        .find(|(k, _)| k.same(&key))
        .map(|(_, r)| r.clone());
    if let Some(r) = hit {
        return Ok(r.as_ref().clone());
    }
    let rows = rows_generated(table, ctx)?;
    let mut c = ctx.sess.sys_rows.borrow_mut();
    c.entries.retain(|(k, _)| k.cat.strong_count() > 0 && k.table != key.table);
    if c.entries.len() >= SYS_ROWS_CACHED {
        c.entries.pop_front();
    }
    c.entries.push_back((key, std::rc::Rc::new(rows.clone())));
    Ok(rows)
}

fn rows_generated(table: SysTable, ctx: &ExecCtx) -> Result<Vec<Row>> {
    use SysTable::*;
    GENERATED.with(|c| c.set(c.get() + 1));
    let schema = if table.schema() == PGC {
        "pg_catalog"
    } else {
        "information_schema"
    };
    // Built without libnuma (pg_get_shmem_allocations_numa).
    if table == PgShmemAllocationsNuma {
        return Err(crate::error::Error::new(
            crate::error::code::FEATURE_NOT_SUPPORTED,
            "libnuma initialization failed or NUMA is not supported on this platform",
        ));
    }
    if let Some(sql) = sql_views().get(&(schema, table.name())) {
        return sql_view_rows(table, sql, ctx);
    }
    let cat = ctx.catalog();
    let cat = &*cat;
    let dbname = ctx.sess.database.clone();
    let role_name = |oid: u32| {
        cat.roles
            .get(&oid)
            .map(|r| r.name.clone())
            .unwrap_or_else(|| ctx.sess.user.clone())
    };
    let mut out: Vec<Row> = Vec::new();
    match table {
        PgNamespace => {
            for s in cat.schemas.values() {
                let acl = match s.oid {
                    PGC | INFORMATION_SCHEMA_OID => Some(vec![
                        AclItem {
                            grantee: OWNER,
                            grantor: OWNER,
                            privs: acl::USAGE | acl::CREATE,
                            goptions: 0,
                        },
                        AclItem {
                            grantee: 0,
                            grantor: OWNER,
                            privs: acl::USAGE,
                            goptions: 0,
                        },
                    ]),
                    _ => cat.acl(ObjKind::Schema, s.oid, 0).cloned(),
                };
                out.push(vec![
                    o(s.oid),
                    nm(&s.name),
                    o(s.owner),
                    acl_value(cat, acl.as_ref()),
                ]);
            }
        }
        PgClass => {
            // Tables with triggers, including the internal ones behind foreign keys.
            let triggered = crate::exec::trigger::triggered_tables(cat, true);
            // Partitioned indexes with partitions' indexes attached.
            let index_parents: std::collections::HashSet<u32> =
                cat.indexes.values().map(|i| i.parent).filter(|p| *p != 0).collect();
            // Postgres's own relations (in schemas still there), privileges as granted here.
            let boot = role_name(OWNER);
            for r in super::static_rows::rows(table)
                .iter()
                .filter(|r| cat.schemas.contains_key(&super::static_rows::oid_at(r, 2)))
            {
                let mut r = r.clone();
                r[31] = match cat.acl(ObjKind::Table, super::static_rows::oid_at(&r, 0), 0) {
                    Some(a) => acl_value(cat, Some(a)),
                    None => boot_acl(&r[31], &boot),
                };
                // REINDEX gave an index a new file (a mapped catalog's isn't shown here).
                if super::static_rows::oid_at(&r, 7) != 0
                    && let Some(n) = cat.filenodes.get(&super::static_rows::oid_at(&r, 0))
                {
                    r[7] = o(*n);
                }
                out.push(r);
            }
            for r in relations(cat)
                .into_iter()
                .filter(|r| r.oid >= super::FIRST_USER_OID)
            {
                let reltype = match r.kind {
                    _ if r.oid < super::FIRST_USER_OID => super::sys_rels::SYS_RELS
                        .iter()
                        .find(|x| x.2 == r.oid)
                        .map_or(0, |x| x.3),
                    'c' => cat
                        .composites
                        .values()
                        .find(|c| c.relid == r.oid)
                        .map_or(0, |c| c.oid),
                    'r' | 'p' | 'f' => cat.tables.get(&r.oid).map_or(0, |t| t.rowtype),
                    'v' | 'm' => cat.views.get(&r.oid).map_or(0, |v| v.rowtype),
                    _ => 0,
                };
                let st = cat.rel_stats(r.oid, r.kind);
                out.push(vec![
                    o(r.oid),
                    nm(&r.name),
                    o(r.schema),
                    o(reltype),
                    o(cat.tables.get(&r.oid).map_or(0, |t| t.of_type)),
                    o(r.owner),
                    o(r.am),
                    o(if matches!(r.kind, 'v' | 'c' | 'p' | 'I' | 'f') {
                        0
                    } else {
                        cat.filenodes.get(&r.oid).copied().unwrap_or(r.oid)
                    }),
                    // A TOAST table and its index lie with their table.
                    o(cat.toast_owner(r.oid).map_or_else(
                        || cat.rel_tablespace(r.oid),
                        |(t, _)| cat.rel_tablespace(t.oid),
                    )),
                    i4(st.pages as i64),
                    Value::Float4(st.tuples as f32),
                    i4(st.allvisible as i64),
                    i4(st.allfrozen as i64),
                    o(cat.index_table(r.oid).map_or(0, |t| t.toast)),
                    b(r.hasindex),
                    b(false),
                    c(r.persistence),
                    c(r.kind),
                    i2(r.natts as i64),
                    i2(r.checks as i64),
                    b(r.kind == 'v' || cat.class_has_rules.contains(&r.oid)),
                    b(triggered.contains(&r.oid)),
                    // (Set by a first child; only ANALYZE clears it, once the children
                    // are gone.)
                    b(!cat.children(r.oid).is_empty()
                        || cat.class_has_subclass.contains(&r.oid)
                        || index_parents.contains(&r.oid)),
                    b(cat.tables.get(&r.oid).is_some_and(|t| t.rls)),
                    b(cat.tables.get(&r.oid).is_some_and(|t| t.rls_force)),
                    b(r.kind != 'm' || cat.views.get(&r.oid).is_none_or(|v| v.populated)),
                    c(match r.kind {
                        'r' | 'p' => cat
                            .tables
                            .get(&r.oid)
                            .and_then(|t| t.replident)
                            .unwrap_or('d'),
                        'm' => 'd',
                        _ => 'n',
                    }),
                    // A partition, or an index attached to a partitioned index.
                    b(cat
                        .tables
                        .get(&r.oid)
                        .is_some_and(|t| t.partbound.is_some())
                        || cat.indexes.get(&r.oid).is_some_and(|i| i.parent != 0)),
                    o(0),
                    // Relations with storage of their own have a frozen xid and multixact id.
                    Value::Oid(if matches!(r.kind, 'r' | 'm' | 't') {
                        cat.frozen_xids
                            .get(&r.oid)
                            .map_or(crate::txn::xids::FIRST_NORMAL_XID, |x| *x)
                            as u32
                    } else {
                        0
                    }),
                    Value::Oid(matches!(r.kind, 'r' | 'm' | 't') as u32),
                    if r.oid < super::FIRST_USER_OID && SysTable::by_oid(r.oid).is_some() {
                        arr(
                            DataType::Aclitem,
                            vec![
                                nm(format!("{0}=arwdDxtm/{0}", role_name(OWNER))),
                                nm(format!("=r/{}", role_name(OWNER))),
                            ],
                        )
                    } else {
                        acl_value(
                            cat,
                            cat.acl(
                                if r.kind == 'S' {
                                    ObjKind::Sequence
                                } else {
                                    ObjKind::Table
                                },
                                r.oid,
                                0,
                            ),
                        )
                    },
                    match r.kind {
                        'v' => cat
                            .views
                            .get(&r.oid)
                            .filter(|v| !v.options.is_empty() || v.check_option.is_some())
                            .map_or(N, |v| {
                                let mut opts: Vec<Value> =
                                    v.options.iter().map(|o| nm(o.as_str())).collect();
                                if let Some(co) = &v.check_option {
                                    opts.push(nm(format!("check_option={co}")));
                                }
                                arr(DataType::Text, opts)
                            }),
                        'i' | 'I' => cat
                            .indexes
                            .get(&r.oid)
                            .filter(|i| !i.options.is_empty())
                            .map_or(N, |i| {
                                arr(
                                    DataType::Text,
                                    i.options.iter().map(|o| nm(o.as_str())).collect(),
                                )
                            }),
                        // (A table's `toast.` options are its TOAST table's.)
                        'r' | 'p' | 'm' => cat
                            .index_table(r.oid)
                            .filter(|t| t.options.iter().any(|o| !o.starts_with("toast.")))
                            .map_or(N, |t| {
                                arr(
                                    DataType::Text,
                                    t.options
                                        .iter()
                                        .filter(|o| !o.starts_with("toast."))
                                        .map(|o| nm(o.as_str()))
                                        .collect(),
                                )
                            }),
                        't' => cat
                            .toast_owner(r.oid)
                            .filter(|(t, index)| {
                                !*index && t.options.iter().any(|o| o.starts_with("toast."))
                            })
                            .map_or(N, |(t, _)| {
                                arr(
                                    DataType::Text,
                                    t.options
                                        .iter()
                                        .filter_map(|o| o.strip_prefix("toast."))
                                        .map(nm)
                                        .collect(),
                                )
                            }),
                        _ => N,
                    },
                    partition_bound_text(cat, r.oid, ctx.fmt()).map_or(N, nm),
                ]);
            }
        }
        PgAttribute => {
            let builtin: std::collections::HashSet<u32> = super::static_rows::rows(PgClass)
                .iter()
                .filter(|r| cat.schemas.contains_key(&super::static_rows::oid_at(r, 2)))
                .map(|r| super::static_rows::oid_at(r, 0))
                .collect();
            let boot = role_name(OWNER);
            for r in super::static_rows::rows(table)
                .iter()
                .filter(|r| builtin.contains(&super::static_rows::oid_at(r, 0)))
            {
                let mut r = r.clone();
                r[21] = boot_acl(&r[21], &boot);
                out.push(r);
            }
            for r in relations(cat)
                .into_iter()
                .filter(|r| r.oid >= super::FIRST_USER_OID)
            {
                for (num, name, ty, col) in rel_columns(cat, r.oid, ctx) {
                    let dropped = col.as_ref().is_some_and(|c| c.dropped);
                    let sys_domain = if col.is_none() {
                        SysTable::by_oid(r.oid)
                            .and_then(|st| st.domains().get(num - 1).copied().flatten())
                    } else {
                        None
                    };
                    let tyoid = match &col {
                        Some(c) if c.dropped => 0,
                        Some(c) => col_type_oid(cat, c),
                        None => sys_domain
                            .or_else(|| {
                                cat.views
                                    .get(&r.oid)
                                    .and_then(|v| v.domains.get(num - 1).copied().flatten())
                            })
                            .unwrap_or_else(|| type_oid(cat, &ty)),
                    };
                    // Everyone may read pg_subscription, all but the connection string.
                    let public_read = r.oid == 6100 && name != "subconninfo";
                    let name = if dropped {
                        format!("........pg.dropped.{num}........")
                    } else {
                        name
                    };
                    let sys = if col.is_none() && r.oid < super::FIRST_USER_OID {
                        sys_attr(r.oid, num)
                    } else {
                        None
                    };
                    // Declared array dimensions: none for what a query defines (views).
                    let ndims = match (sys, &col) {
                        (Some(a), _) => a.2,
                        (None, Some(c)) => c.attndims() as i64,
                        (None, None) => {
                            match cat.composites.values().find(|ct| ct.relid == r.oid) {
                                Some(ct) => ct.attndims(&name, &ty) as i64,
                                None if cat.views.contains_key(&r.oid) => 0,
                                None if ty.is_array()
                                    || matches!(ty, DataType::Int2Vector | DataType::OidVector) =>
                                {
                                    1
                                }
                                None => 0,
                            }
                        }
                    };
                    out.push(vec![
                        o(r.oid),
                        nm(name),
                        o(tyoid),
                        i2(ty.typlen() as i64),
                        i2(num as i64),
                        i4(if sys_domain.is_some() {
                            -1
                        } else {
                            ty.typmod() as i64
                        }),
                        i2(ndims),
                        b(ty.by_value()),
                        c(align(&ty)),
                        c(col
                            .as_ref()
                            .and_then(|c| c.storage)
                            .unwrap_or_else(|| typstorage(&ty))),
                        c(col.as_ref().and_then(|c| c.compression).unwrap_or('\0')),
                        b(col.as_ref().is_some_and(|c| c.not_null) || sys.is_some_and(|a| a.0)),
                        b(col
                            .as_ref()
                            .is_some_and(|c| c.default.is_some() || c.generated.is_some())),
                        b(col.as_ref().is_some_and(|c| c.missing.is_some())),
                        c(match col.as_ref().and_then(|c| c.identity.clone()) {
                            Some(super::Identity::Always) => 'a',
                            Some(super::Identity::ByDefault) => 'd',
                            None => '\0',
                        }),
                        c(match col.as_ref() {
                            Some(c) if c.virtual_gen => 'v',
                            Some(c) if c.generated.is_some() => 's',
                            _ => '\0',
                        }),
                        b(dropped),
                        b(col.as_ref().is_none_or(|c| c.islocal)),
                        i2(col.as_ref().map_or(0, |c| c.inhcount as i64)),
                        o(match (&col, sys) {
                            (Some(c), _) => column_collation(c),
                            (None, Some(a)) => a.1,
                            (None, None) => cat
                                .views
                                .get(&r.oid)
                                .and_then(|v| v.collations.get(num - 1).cloned().flatten())
                                .and_then(|c| collation_oid(&c))
                                .unwrap_or_else(|| type_collation(&ty)),
                        }),
                        col.as_ref()
                            .and_then(|c| c.stattarget)
                            .map_or(N, |s| i2(s as i64)),
                        N,
                        col.as_ref()
                            .filter(|c| !c.options.is_empty())
                            .map_or(N, |c| {
                                arr(
                                    DataType::Text,
                                    c.options.iter().map(|o| nm(o.as_str())).collect(),
                                )
                            }),
                        cat.tables
                            .get(&r.oid)
                            .and_then(|t| t.foreign.as_ref())
                            .and_then(|f| f.column_options(num - 1))
                            .map_or(N, |o| {
                                arr(DataType::Text, o.iter().map(|o| nm(o.as_str())).collect())
                            }),
                        col.as_ref()
                            .and_then(|c| c.missing.clone().map(|m| arr(c.ty.clone(), vec![m])))
                            .unwrap_or(N),
                    ]);
                    if public_read {
                        out.last_mut().unwrap()[21] = acl_value(
                            cat,
                            Some(&vec![acl::AclItem {
                                grantee: 0,
                                grantor: super::BOOTSTRAP_SUPERUSER_OID,
                                privs: acl::SELECT,
                                goptions: 0,
                            }]),
                        );
                    }
                    if let Some(a) = cat.acl(ObjKind::Table, r.oid, num as i32) {
                        out.last_mut().unwrap()[21] = acl_value(cat, Some(a));
                    }
                }
                // System columns of tables.
                // System columns: of all relations with tuples (not of views, composite types
                // or indexes).
                if matches!(r.kind, 'r' | 'p' | 'm' | 'S' | 't' | 'f') {
                    for (num, name, ty) in [
                        (-1, "ctid", 27u32),
                        (-2, "xmin", 28),
                        (-3, "cmin", 29),
                        (-4, "xmax", 28),
                        (-5, "cmax", 29),
                        (-6, "tableoid", 26),
                    ] {
                        let dt = DataType::from_oid(ty).unwrap();
                        out.push(vec![
                            o(r.oid),
                            nm(name),
                            o(ty),
                            i2(dt.typlen() as i64),
                            i2(num),
                            i4(-1),
                            i2(0),
                            b(dt.by_value()),
                            c(align(&dt)),
                            c('p'),
                            c('\0'),
                            b(true),
                            b(false),
                            b(false),
                            c('\0'),
                            c('\0'),
                            b(false),
                            b(true),
                            i2(0),
                            o(0),
                            N,
                            N,
                            N,
                            N,
                            N,
                        ]);
                        // (A system column can be granted privileges too.)
                        if let Some(a) = cat.acl(ObjKind::Table, r.oid, num as i32) {
                            out.last_mut().unwrap()[21] = acl_value(cat, Some(a));
                        }
                    }
                }
            }
        }
        PgType => {
            let mut push = |oid: u32,
                            name: String,
                            ns: u32,
                            t: &DataType,
                            typtype: char,
                            elem: u32,
                            array: u32,
                            base: u32,
                            typmod: i32,
                            notnull: bool,
                            relid: u32| {
                out.push(vec![
                    o(oid),
                    nm(name),
                    o(ns),
                    o(OWNER),
                    i2(t.typlen() as i64),
                    b(t.by_value()),
                    c(typtype),
                    c(if typtype == 'p' && t.is_array() {
                        'P'
                    } else {
                        t.category_char()
                    }),
                    b(t.is_preferred()),
                    b(true),
                    c(crate::types::array::typdelim(t.elem().unwrap_or(t))),
                    o(relid),
                    o(0),
                    o(elem),
                    o(array),
                    o(0),
                    o(0),
                    o(0),
                    o(0),
                    o(0),
                    o(0),
                    o(0),
                    c(align(t)),
                    c(typstorage(t)),
                    b(notnull),
                    o(base),
                    i4(typmod as i64),
                    i4(0),
                    o(if matches!(t, DataType::Name) {
                        950
                    } else if t.is_string() || matches!(t, DataType::PgNodeTree) {
                        100
                    } else {
                        0
                    }),
                    N,
                    N,
                    N,
                ]);
            };
            for t in DataType::all_scalars() {
                let typtype = match t {
                    DataType::Range(_) => 'r',
                    DataType::Multirange(_) => 'm',
                    _ if t.category() == crate::types::Category::Pseudo => 'p',
                    _ => 'b',
                };
                // Fixed-size geometric types subscript like arrays of their parts.
                // (name, int2vector and oidvector too.)
                let elem = match t {
                    DataType::Point | DataType::Line => 701,
                    DataType::Lseg | DataType::GBox => 600,
                    DataType::Name => 18,
                    DataType::Int2Vector => 21,
                    DataType::OidVector => 26,
                    _ => 0,
                };
                push(
                    t.oid(),
                    t.typname(),
                    PGC,
                    &t,
                    typtype,
                    elem,
                    t.array_oid(),
                    0,
                    -1,
                    false,
                    0,
                );
                if t.array_oid() != 0 {
                    let a = DataType::array_of(t.clone());
                    // record[] is a pseudo-type too.
                    push(
                        t.array_oid(),
                        a.typname(),
                        PGC,
                        &a,
                        if matches!(t, DataType::Record) {
                            'p'
                        } else {
                            'b'
                        },
                        t.oid(),
                        0,
                        0,
                        -1,
                        false,
                        0,
                    );
                }
            }
            for t in [
                DataType::Unknown,
                DataType::Void,
                DataType::Trigger,
                DataType::Any,
                DataType::AnyElement,
                DataType::AnyArray,
                DataType::AnyNonArray,
                DataType::AnyCompatible,
                DataType::AnyCompatibleArray,
                DataType::AnyEnum,
                DataType::AnyRange,
                DataType::AnyMultirange,
                DataType::AnyCompatibleRange,
                DataType::AnyCompatibleMultirange,
            ] {
                push(t.oid(), t.typname(), PGC, &t, 'p', 0, 0, 0, -1, false, 0);
            }
            for &(oid, name, _, _) in crate::types::oid::OTHER_PSEUDO {
                push(
                    oid,
                    name.to_string(),
                    PGC,
                    &DataType::Void,
                    'p',
                    0,
                    0,
                    0,
                    -1,
                    false,
                    0,
                );
            }
            for e in cat.enums.values() {
                let t = e.data_type();
                push(
                    e.oid,
                    e.name.clone(),
                    e.schema,
                    &t,
                    'e',
                    0,
                    e.array_oid,
                    0,
                    -1,
                    false,
                    0,
                );
                let a = DataType::array_of(t);
                push(
                    e.array_oid,
                    cat.array_typname(e.array_oid, &e.name),
                    e.schema,
                    &a,
                    'b',
                    e.oid,
                    0,
                    0,
                    -1,
                    false,
                    0,
                );
            }
            for d in cat.domains.values() {
                let base = cat.domain_base_oid(d);
                push(
                    d.oid,
                    d.name.clone(),
                    d.schema,
                    &d.base,
                    'd',
                    0,
                    d.array_oid,
                    base,
                    d.base.typmod(),
                    d.not_null,
                    0,
                );
                if d.array_oid != 0 {
                    push(
                        d.array_oid,
                        cat.array_typname(d.array_oid, &d.name),
                        d.schema,
                        &DataType::array_of(d.base.clone()),
                        'b',
                        d.oid,
                        0,
                        0,
                        -1,
                        false,
                        0,
                    );
                }
            }
            // Row types of the system relations.
            for &(schema, name, relid, reltype, typarray, _) in super::sys_rels::SYS_RELS {
                if SysTable::by_oid(relid).is_none() {
                    continue;
                }
                let ns = if schema == "pg_catalog" { PGC } else { INF };
                let t = DataType::Row(Arc::new(crate::types::RowType {
                    oid: reltype,
                    array_oid: typarray,
                    name: Some(Arc::from(name)),
                    fields: Vec::new(),
                }));
                push(
                    reltype,
                    name.to_string(),
                    ns,
                    &t,
                    'c',
                    0,
                    typarray,
                    0,
                    -1,
                    false,
                    relid,
                );
                push(
                    typarray,
                    cat.array_typname(typarray, name),
                    ns,
                    &DataType::array_of(t),
                    'b',
                    reltype,
                    0,
                    0,
                    -1,
                    false,
                    0,
                );
            }
            // Row types of tables and views.
            for (oid, array_oid, schema, name, relid) in cat
                .tables
                .values()
                .map(|t| (t.rowtype, t.array_type(), t.schema, &t.name, t.oid))
                .chain(
                    cat.views
                        .values()
                        .map(|v| (v.rowtype, v.array_type(), v.schema, &v.name, v.oid)),
                )
                .filter(|r| r.0 != 0)
            {
                let t = cat.user_data_type(oid).unwrap_or(DataType::Record);
                push(
                    oid,
                    name.clone(),
                    schema,
                    &t,
                    'c',
                    0,
                    array_oid,
                    0,
                    -1,
                    false,
                    relid,
                );
                let a = DataType::array_of(t);
                push(
                    array_oid,
                    cat.array_typname(array_oid, name),
                    schema,
                    &a,
                    'b',
                    oid,
                    0,
                    0,
                    -1,
                    false,
                    0,
                );
            }
            for ct in cat.composites.values() {
                let t = ct.data_type();
                push(
                    ct.oid,
                    ct.name.clone(),
                    ct.schema,
                    &t,
                    'c',
                    0,
                    ct.array_oid,
                    0,
                    -1,
                    false,
                    ct.relid,
                );
                let a = DataType::array_of(t);
                push(
                    ct.array_oid,
                    cat.array_typname(ct.array_oid, &ct.name),
                    ct.schema,
                    &a,
                    'b',
                    ct.oid,
                    0,
                    0,
                    -1,
                    false,
                    0,
                );
            }
            for r in cat.ranges.values() {
                let rt = r.range_type();
                for (oid, array_oid, name, t) in [
                    (r.oid, r.array_oid, &r.name, DataType::Range(rt.clone())),
                    (
                        r.multi_oid,
                        r.multi_array_oid,
                        &r.multi_name,
                        DataType::Multirange(rt.clone()),
                    ),
                ] {
                    let typtype = if oid == r.oid { 'r' } else { 'm' };
                    push(
                        oid,
                        name.clone(),
                        r.schema,
                        &t,
                        typtype,
                        0,
                        array_oid,
                        0,
                        -1,
                        false,
                        0,
                    );
                    let a = DataType::array_of(t);
                    push(
                        array_oid,
                        cat.array_typname(array_oid, name),
                        r.schema,
                        &a,
                        'b',
                        oid,
                        0,
                        0,
                        -1,
                        false,
                        0,
                    );
                }
            }
            for d in cat.base_types.values() {
                let t = d.data_type();
                if d.is_shell() {
                    push(d.oid(), d.ty.name.to_string(), d.ty.schema, &t, 'p', 0, 0, 0, -1, false, 0);
                    continue;
                }
                push(
                    d.oid(),
                    d.ty.name.to_string(),
                    d.ty.schema,
                    &t,
                    'b',
                    d.element,
                    d.ty.array_oid,
                    0,
                    -1,
                    false,
                    0,
                );
                push(
                    d.ty.array_oid,
                    cat.array_typname(d.ty.array_oid, &d.ty.name),
                    d.ty.schema,
                    &DataType::array_of(t),
                    'b',
                    d.oid(),
                    0,
                    0,
                    -1,
                    false,
                    0,
                );
            }
            // information_schema declares its text domains COLLATE "C".
            for row in out.iter_mut() {
                if let Value::Oid(oid) = row[0]
                    && cat.domains.get(&oid).is_some_and(|d| d.schema == INF)
                    && row[28] != o(0)
                {
                    row[28] = o(950);
                }
            }
            // Support functions, and what arrays and domains take from their element / base type.
            let collations: std::collections::HashMap<u32, Value> = out
                .iter()
                .filter_map(|r| {
                    if let Value::Oid(oid) = r[0] {
                        Some((oid, r[28].clone()))
                    } else {
                        None
                    }
                })
                .collect();
            for row in out.iter_mut() {
                let Value::Oid(oid) = row[0] else { continue };
                for (i, p) in type_procs(cat, oid).into_iter().enumerate() {
                    row[if i == 0 { 12 } else { 14 + i }] = o(p);
                }
                if let Some(d) = cat.domains.get(&oid) {
                    if matches!(d.base, DataType::Array(_)) {
                        row[27] = i4(1);
                    }
                } else if let Value::Oid(elem) = row[13]
                    && elem != 0
                    && row[7] == c('A')
                    && let Some(coll) = collations.get(&elem)
                {
                    row[28] = coll.clone();
                }
            }
            // Owners and privileges of user types (arrays follow their element type).
            for row in out.iter_mut() {
                let Value::Oid(oid) = row[0] else { continue };
                if let Some(&(_, _, len, align)) =
                    crate::types::oid::OTHER_PSEUDO.iter().find(|p| p.0 == oid)
                {
                    row[4] = i2(len as i64);
                    row[5] = b(true);
                    row[22] = c(align);
                    row[23] = c('p');
                }
                if let Some(def) = cat.domains.get(&oid).and_then(|d| d.default.as_ref()) {
                    row[30] = nm(def);
                }
                // A made base type's own attributes (its array's alignment follows it).
                if let Some(d) = cat.base_types.get(&oid) {
                    row[9] = b(!d.is_shell());
                    row[10] = c(d.delimiter);
                    if d.ty.collatable {
                        row[28] = o(100);
                    }
                    if let Some(def) = &d.default {
                        row[30] = nm(def);
                    }
                } else if let Some(d) = cat.base_types.values().find(|d| d.ty.array_oid == oid) {
                    row[10] = c(d.delimiter);
                    row[22] = c(if d.ty.align == 'd' { 'd' } else { 'i' });
                    if d.ty.collatable {
                        row[28] = o(100);
                    }
                }
                if oid < super::FIRST_USER_OID {
                    continue;
                }
                let elem = match row[13] {
                    Value::Oid(e) if e != 0 => e,
                    _ => oid,
                };
                let owner = cat
                    .enums
                    .get(&elem)
                    .map(|e| e.owner)
                    .or_else(|| cat.composites.get(&elem).map(|c| c.owner))
                    .or_else(|| cat.domains.get(&elem).map(|d| d.owner))
                    .or_else(|| cat.base_types.get(&elem).map(|d| d.owner))
                    .or_else(|| {
                        cat.ranges
                            .values()
                            .find(|r| r.oid == elem || r.multi_oid == elem)
                            .map(|r| r.owner)
                    })
                    .or_else(|| {
                        cat.tables
                            .values()
                            .find(|t| t.rowtype == elem)
                            .map(|t| t.owner)
                    })
                    .or_else(|| {
                        cat.views
                            .values()
                            .find(|v| v.rowtype == elem)
                            .map(|v| v.owner)
                    });
                if let Some(owner) = owner {
                    row[3] = o(owner);
                }
                if let Some(a) = cat.acl(ObjKind::Type, elem, 0) {
                    let n = row.len();
                    row[n - 1] = acl_value(cat, Some(a));
                }
            }
            // Postgres's own types as it has them (in schemas still there), then ours.
            let mine: Vec<Row> = out
                .drain(..)
                .filter(|r| matches!(r[0], Value::Oid(oid) if oid >= super::FIRST_USER_OID))
                .collect();
            for r in super::static_rows::rows(table)
                .iter()
                .filter(|r| cat.schemas.contains_key(&super::static_rows::oid_at(r, 2)))
            {
                let mut r = r.clone();
                let (oid, elem) = (
                    super::static_rows::oid_at(&r, 0),
                    super::static_rows::oid_at(&r, 13),
                );
                if let Some(a) = cat.acl(
                    ObjKind::Type,
                    if elem != 0 && r[7] == c('A') {
                        elem
                    } else {
                        oid
                    },
                    0,
                ) {
                    let n = r.len();
                    r[n - 1] = acl_value(cat, Some(a));
                }
                out.push(r);
            }
            out.extend(mine);
        }
        PgIndex => {
            out.extend(super::static_rows::rows(table).iter().cloned());
            for ix in cat.indexes.values() {
                let cols = index_cols(ix);
                let nkey = ix.columns.len();
                let options: Vec<Value> = ix
                    .columns
                    .iter()
                    .map(|c| i2((c.desc as i64) | ((c.nulls_first as i64) << 1)))
                    .collect();
                let table = cat.index_table(ix.table);
                let table = table.as_ref();
                let shown = |e: &str, kind| {
                    table
                        .and_then(|t| stored_expr(ctx, t, e, kind, false))
                        .unwrap_or_else(|| e.to_string())
                };
                let exprs: Vec<String> = ix
                    .columns
                    .iter()
                    .filter_map(|c| {
                        if let IndexKey::Expr(e) = &c.key {
                            Some(shown(e, Stored::Plain))
                        } else {
                            None
                        }
                    })
                    .collect();
                // Key types, for the default opclass and collation.
                let key_types: Vec<Option<DataType>> = ix
                    .columns
                    .iter()
                    .map(|c| match (&c.key, table) {
                        (IndexKey::Column(i), Some(t)) => Some(t.columns[*i].ty.clone()),
                        (IndexKey::Expr(e), Some(t)) => {
                            crate::sql::ruleutils::stored_expr_type(ctx, t, e, Stored::Plain)
                        }
                        _ => None,
                    })
                    .collect();
                let method = access_method_oid(&ix.method);
                let con = table.and_then(|t| {
                    t.constraints
                        .iter()
                        .find(|c| c.kind.index() == Some(ix.oid))
                });
                out.push(vec![
                    o(ix.oid),
                    o(ix.table),
                    i2(cols.len() as i64),
                    i2(nkey as i64),
                    // WITHOUT OVERLAPS: unique (and primary) for the catalogs, an exclusion
                    // constraint inside.
                    b(ix.unique || con.is_some_and(|c| c.without_overlaps())),
                    b(ix.nulls_not_distinct),
                    b(ix.primary || con.is_some_and(|c| c.without_overlaps() && c.period == Some('p'))),
                    b(con.is_some_and(|c| matches!(c.kind, ConstraintKind::Exclusion { .. }))),
                    b(!con.is_some_and(|c| c.deferral().0)),
                    b(ix.clustered),
                    b(ix.valid),
                    b(false),
                    b(true),
                    b(true),
                    b(ix.replident),
                    vector(DataType::Int2, cols.iter().map(|c| i2(*c)).collect()),
                    vector(
                        DataType::Oid,
                        ix.columns
                            .iter()
                            .zip(&key_types)
                            .map(|(c, t)| {
                                let Some(t) = t else { return o(0) };
                                o(match (&c.collation, &c.key, table) {
                                    _ if type_collation(t) == 0 => 0,
                                    (Some(n), _, _) => collation_oid(n).unwrap_or(100),
                                    (None, IndexKey::Column(i), Some(tb)) => {
                                        column_collation(&tb.columns[*i])
                                    }
                                    _ => type_collation(t),
                                })
                            })
                            .collect(),
                    ),
                    vector(
                        DataType::Oid,
                        ix.columns
                            .iter()
                            .zip(&key_types)
                            .map(|(c, t)| {
                                o(if c.opclass != 0 {
                                    c.opclass
                                } else {
                                    t.as_ref().map_or(0, |t| default_opclass(method, t))
                                })
                            })
                            .collect(),
                    ),
                    vector(DataType::Int2, options),
                    if exprs.is_empty() {
                        N
                    } else {
                        nm(exprs.join(", "))
                    },
                    opt(ix.predicate.as_deref().map(|p| shown(p, Stored::Condition))),
                ]);
            }
            // TOAST indexes: the unique (chunk_id, chunk_seq) key, as create_toast_table
            // makes it.
            for t in cat.toasted() {
                out.push(vec![
                    o(t.toast + 1),
                    o(t.toast),
                    i2(2),
                    i2(2),
                    b(true),
                    b(false),
                    b(true),
                    b(false),
                    b(true),
                    b(false),
                    b(true),
                    b(false),
                    b(true),
                    b(true),
                    b(false),
                    vector(DataType::Int2, vec![i2(1), i2(2)]),
                    vector(DataType::Oid, vec![o(0), o(0)]),
                    vector(DataType::Oid, vec![o(1981), o(1978)]),
                    vector(DataType::Int2, vec![i2(0), i2(0)]),
                    N,
                    N,
                ]);
            }
        }
        PgStatistic => {
            out.extend(super::static_rows::rows(table).iter().cloned());
            for (rel, rows) in &cat.colstats {
                for st in rows {
                    out.push(statistic_row(*rel, st));
                }
            }
        }
        PgStatisticExtData => {
            for st in cat.statistics.values() {
                for d in &st.data {
                    out.push(vec![
                        o(st.oid),
                        b(d.inherit),
                        opt_bytes(&d.ndistinct),
                        opt_bytes(&d.dependencies),
                        opt_bytes(&d.mcv),
                        expr_stats(d),
                    ]);
                }
            }
        }
        PgStatsExt | PgStatsExtExprs => {
            let me = ctx.current_user();
            let binder = crate::sql::binder::Binder::new(ctx, Vec::new());
            for st in cat.statistics.values() {
                let Some(t) = cat.tables.get(&st.table) else {
                    continue;
                };
                if !cat.has_privs_of_role(me, t.owner, ctx.db_owner())
                    || binder.rls_enabled(t).unwrap_or(true)
                {
                    continue;
                }
                let head = [
                    nm(cat.schema_name(t.schema)),
                    nm(&t.name),
                    nm(cat.schema_name(st.schema)),
                    nm(&st.name),
                    nm(cat
                        .roles
                        .get(&st.owner)
                        .map_or_else(|| format!("unknown (OID={})", st.owner), |r| r.name.clone())),
                ];
                let exprs = crate::sql::func::system::statistics_exprs(ctx, st.oid);
                if table == PgStatsExtExprs {
                    let Some(exprs) = exprs else { continue };
                    let data: Vec<Option<&super::StatExtData>> = if st.data.is_empty() {
                        vec![None]
                    } else {
                        st.data.iter().map(Some).collect()
                    };
                    for d in data {
                        for (i, e) in exprs.iter().enumerate() {
                            let mut row = head.to_vec();
                            row.push(nm(e));
                            row.push(d.map_or(N, |d| b(d.inherit)));
                            let cs = d
                                .and_then(|d| d.exprs.as_ref())
                                .and_then(|x| x.get(i).cloned().flatten());
                            match cs {
                                Some(cs) => {
                                    let kind = |k: i16| cs.slots.iter().find(|x| x.kind == k);
                                    let first = |k: i16| {
                                        kind(k)
                                            .and_then(|x| x.numbers.as_ref())
                                            .and_then(|n| n.first())
                                            .map_or(N, |f| Value::Float4(*f))
                                    };
                                    row.extend([
                                        Value::Float4(cs.nullfrac),
                                        i4(cs.width as i64),
                                        Value::Float4(cs.distinct),
                                        kind(1).map_or(N, stat_values),
                                        kind(1).map_or(N, stat_numbers),
                                        kind(2).map_or(N, stat_values),
                                        first(3),
                                        kind(4).map_or(N, stat_values),
                                        kind(4).map_or(N, stat_numbers),
                                        kind(5).map_or(N, stat_numbers),
                                    ]);
                                }
                                None => row.extend(std::iter::repeat_n(N, 10)),
                            }
                            out.push(row);
                        }
                    }
                    continue;
                }
                let attnames = if st.columns.is_empty() {
                    N
                } else {
                    arr(
                        DataType::Name,
                        st.columns.iter().map(|&i| nm(&t.columns[i].name)).collect(),
                    )
                };
                for d in &st.data {
                    let mut row = head.to_vec();
                    row.push(attnames.clone());
                    row.push(
                        exprs
                            .as_ref()
                            .map_or(N, |e| arr(DataType::Text, e.iter().map(nm).collect())),
                    );
                    row.push(arr(
                        DataType::Char,
                        st.stxkind().into_iter().map(c).collect(),
                    ));
                    row.push(b(d.inherit));
                    row.push(opt_bytes(&d.ndistinct));
                    row.push(opt_bytes(&d.dependencies));
                    match d.mcv.as_deref().and_then(|m| mcv_items(cat, m)) {
                        Some(items) => {
                            let fmt = crate::types::text::FormatCtx::default();
                            // array_agg of the items' arrays: two dimensions.
                            let grid =
                                |f: &dyn Fn(&crate::types::extstats::McvItem, usize) -> Value,
                                 elem: DataType| {
                                    let ndims = items.0.len();
                                    let cells: Vec<Value> = items
                                        .1
                                        .iter()
                                        .flat_map(|it| {
                                            (0..ndims).map(|j| f(it, j)).collect::<Vec<_>>()
                                        })
                                        .collect();
                                    Value::Array(Arc::new(crate::types::array::ArrayValue {
                                        elem,
                                        dims: vec![(items.1.len(), 1), (ndims, 1)],
                                        items: cells,
                                    }))
                                };
                            row.push(grid(
                                &|it, j| {
                                    if it.nulls[j] {
                                        N
                                    } else {
                                        nm(crate::types::text::format_value(
                                            &it.values[j],
                                            &items.0[j],
                                            &fmt,
                                        ))
                                    }
                                },
                                DataType::Text,
                            ));
                            row.push(grid(&|it, j| b(it.nulls[j]), DataType::Bool));
                            row.push(arr(
                                DataType::Float8,
                                items
                                    .1
                                    .iter()
                                    .map(|it| Value::Float8(it.frequency))
                                    .collect(),
                            ));
                            row.push(arr(
                                DataType::Float8,
                                items
                                    .1
                                    .iter()
                                    .map(|it| Value::Float8(it.base_frequency))
                                    .collect(),
                            ));
                        }
                        None => row.extend(std::iter::repeat_n(N, 4)),
                    }
                    out.push(row);
                }
            }
        }
        PgStats => {
            let me = ctx.current_user();
            // Postgres's statistics on its own catalogs, for those the user may read.
            let superuser = cat.roles.get(&me).is_some_and(|r| r.superuser);
            let classes = super::static_rows::rows(PgClass);
            let attrs = super::static_rows::rows(PgAttribute);
            let at = super::static_rows::oid_at;
            for s in super::static_rows::rows(PgStatistic).iter() {
                let Some(class) = classes.iter().find(|c| at(c, 0) == at(s, 0)) else {
                    continue;
                };
                let Some(att) = attrs.iter().find(|a| at(a, 0) == at(s, 0) && a[4] == s[1]) else {
                    continue;
                };
                let public = matches!(&class[31], Value::Array(a) if a.items.iter().any(|i| super::static_rows::text_at(std::slice::from_ref(i), 0).starts_with("=r")));
                if !(superuser || public) || !cat.schemas.contains_key(&at(class, 2)) {
                    continue;
                }
                let kind = |k: i16| (0..5).find(|i| s[6 + i] == Value::Int2(k));
                let vals = |k: i16| kind(k).map_or(N, |i| s[26 + i].clone());
                let nums = |k: i16| kind(k).map_or(N, |i| s[21 + i].clone());
                let first = |k: i16| match nums(k) {
                    Value::Array(a) => a.items.first().cloned().unwrap_or(N),
                    _ => N,
                };
                out.push(vec![
                    nm(cat.schema_name(at(class, 2))),
                    class[1].clone(),
                    att[1].clone(),
                    s[2].clone(),
                    s[3].clone(),
                    s[4].clone(),
                    s[5].clone(),
                    vals(1),
                    nums(1),
                    vals(2),
                    first(3),
                    vals(4),
                    nums(4),
                    nums(5),
                    vals(6),
                    first(6),
                    vals(7),
                ]);
            }
            let binder = crate::sql::binder::Binder::new(ctx, Vec::new());
            for (rel, rows) in &cat.colstats {
                let (schema, relname, names, rls): (u32, String, Vec<String>, bool) =
                    if let Some(t) = cat.tables.get(rel) {
                        (
                            t.schema,
                            t.name.clone(),
                            t.columns
                                .iter()
                                .map(|c| {
                                    if c.dropped {
                                        String::new()
                                    } else {
                                        c.name.clone()
                                    }
                                })
                                .collect(),
                            t.rls,
                        )
                    } else if let Some(ix) = cat.indexes.get(rel) {
                        let Some(t) = cat.index_table(ix.table) else {
                            continue;
                        };
                        (t.schema, ix.name.clone(), index_att_names(&t, ix), false)
                    } else if let Some(t) = cat.views.get(rel).and_then(|v| v.storage.clone()) {
                        (
                            t.schema,
                            t.name.clone(),
                            t.columns.iter().map(|c| c.name.clone()).collect(),
                            false,
                        )
                    } else {
                        continue;
                    };
                if rls
                    && cat
                        .tables
                        .get(rel)
                        .is_some_and(|t| binder.rls_enabled(t).unwrap_or(true))
                {
                    continue;
                }
                for st in rows {
                    let Some(att) = names.get(st.attnum as usize - 1).filter(|n| !n.is_empty())
                    else {
                        continue;
                    };
                    let held = cat.rel_mask(*rel, me, ctx.db_owner()).map_or(0, |m| m.0)
                        | cat.col_mask(*rel, st.attnum as i32, me, ctx.db_owner()).0;
                    if held & acl::SELECT == 0 {
                        continue;
                    }
                    let kind = |k: i16| st.slots.iter().find(|x| x.kind == k);
                    let first = |k: i16| {
                        kind(k)
                            .and_then(|x| x.numbers.as_ref())
                            .and_then(|n| n.first())
                            .map_or(N, |f| Value::Float4(*f))
                    };
                    out.push(vec![
                        nm(cat.schema_name(schema)),
                        nm(&relname),
                        nm(att),
                        b(st.inherit),
                        Value::Float4(st.nullfrac),
                        i4(st.width as i64),
                        Value::Float4(st.distinct),
                        kind(1).map_or(N, stat_values),
                        kind(1).map_or(N, stat_numbers),
                        kind(2).map_or(N, stat_values),
                        first(3),
                        kind(4).map_or(N, stat_values),
                        kind(4).map_or(N, stat_numbers),
                        kind(5).map_or(N, stat_numbers),
                        kind(6).map_or(N, stat_values),
                        first(6),
                        kind(7).map_or(N, stat_values),
                    ]);
                }
            }
        }
        PgStatisticExt => {
            for st in cat.statistics.values() {
                let table = cat.index_table(st.table);
                let exprs: Vec<String> = st
                    .exprs
                    .iter()
                    .map(|e| {
                        table
                            .as_ref()
                            .and_then(|t| stored_expr(ctx, t, e, Stored::Plain, false))
                            .unwrap_or_else(|| e.clone())
                    })
                    .collect();
                out.push(vec![
                    o(st.oid),
                    o(st.table),
                    nm(&st.name),
                    o(st.schema),
                    o(st.owner),
                    vector(
                        DataType::Int2,
                        st.columns.iter().map(|&i| i2(i as i64 + 1)).collect(),
                    ),
                    st.stattarget.map_or(N, |v| i2(v as i64)),
                    arr(DataType::Char, st.stxkind().into_iter().map(c).collect()),
                    if exprs.is_empty() {
                        N
                    } else {
                        nm(exprs.join(", "))
                    },
                ]);
            }
        }
        PgConstraint => {
            out.extend(super::static_rows::rows(table).iter().cloned());
            for t in cat.tables.values() {
                for con in &t.constraints {
                    out.push(constraint_row(ctx, cat, t, con));
                }
            }
            // Constraint triggers' own constraints (contype t).
            for x in cat.triggers.values().filter(|x| x.con_oid != 0) {
                let schema = cat.tables.get(&x.table).map_or(0, |t| t.schema);
                out.push(vec![
                    o(x.con_oid),
                    nm(&x.name),
                    o(schema),
                    c('t'),
                    b(x.deferrable),
                    b(x.initdeferred),
                    b(true),
                    b(true),
                    o(x.table),
                    o(0),
                    o(0),
                    o(0),
                    o(0),
                    c(' '),
                    c(' '),
                    c(' '),
                    b(true),
                    i2(0),
                    b(true),
                    b(false),
                    N,
                    N,
                    N,
                    N,
                    N,
                    N,
                    N,
                    N,
                ]);
            }
            // information_schema's domains are Postgres's own: their constraints are above.
            for d in cat
                .domains
                .values()
                .filter(|d| d.oid >= super::FIRST_USER_OID)
            {
                let not_null = d.not_null.then(|| {
                    (
                        63,
                        d.not_null_name
                            .clone()
                            .unwrap_or_else(|| format!("{}_not_null", d.name)),
                        'n',
                        String::new(),
                    )
                });
                let checks = d
                    .checks
                    .iter()
                    .enumerate()
                    .map(|(i, (n, e))| (i, n.clone(), 'c', e.clone()));
                for (i, name, kind, expr) in not_null.into_iter().chain(checks) {
                    let (name, expr) = (&name, &expr);
                    out.push(vec![
                        o(domain_constraint_oid(d, i)),
                        nm(name),
                        o(d.schema),
                        c(kind),
                        b(false),
                        b(false),
                        b(true),
                        b(!d.invalid.contains(name)),
                        o(0),
                        o(d.oid),
                        o(0),
                        o(0),
                        o(0),
                        c(' '),
                        c(' '),
                        c(' '),
                        b(true),
                        i2(0),
                        b(false),
                        b(false),
                        N,
                        N,
                        N,
                        N,
                        N,
                        N,
                        N,
                        nm(expr),
                    ]);
                }
            }
        }
        PgProc => {
            for row in builtin_procs() {
                let mut row = row.clone();
                let Value::Oid(oid) = row[0] else { continue };
                let stored = cat
                    .acl(ObjKind::Function, oid, 0)
                    .cloned()
                    .or_else(|| acl::builtin_proc_acl(oid));
                row[29] = acl_value(cat, stored.as_ref());
                out.push(row);
            }
            // The constructors of user range types and their multiranges.
            for r in cat.ranges.values().filter(|r| r.fn_oids.len() == 6) {
                let rt = r.range_type();
                let range = DataType::Range(rt);
                let st = r.subtype.clone();
                let ctors = if r.ctors.0.is_empty() {
                    (r.name.clone(), r.multi_name.clone())
                } else {
                    r.ctors.clone()
                };
                for (i, (name, args, src)) in [
                    (&ctors.0, vec![st.clone(), st.clone()], "range_constructor2"),
                    (
                        &ctors.0,
                        vec![st.clone(), st, DataType::Text],
                        "range_constructor3",
                    ),
                    (&ctors.1, vec![], "multirange_constructor0"),
                    (&ctors.1, vec![range.clone()], "multirange_constructor1"),
                    (
                        &ctors.1,
                        vec![DataType::array_of(range)],
                        "multirange_constructor2",
                    ),
                ]
                .into_iter()
                .enumerate()
                {
                    let ret = if i < 2 { r.oid } else { r.multi_oid };
                    let mut row = proc_row(
                        r.fn_oids[i],
                        name,
                        r.schema,
                        'f',
                        i >= 2,
                        false,
                        'i',
                        &args,
                        0,
                        ret,
                        i == 4,
                        src,
                        "",
                    );
                    row[3] = o(r.owner);
                    out.push(row);
                }
            }
            for f in cat.functions.values() {
                let args: Vec<DataType> = f.args.iter().map(|a| a.1.clone()).collect();
                let lang = if f.language == "sql" {
                    "sql"
                } else {
                    f.language.as_str()
                };
                let ndefaults = f.defaults.iter().filter(|d| d.is_some()).count();
                let kind = if f.procedure {
                    'p'
                } else if f.agg.is_some() {
                    'a'
                } else {
                    'f'
                };
                let mut row = proc_row(
                    f.oid,
                    &f.name,
                    f.schema,
                    kind,
                    f.strict,
                    f.returns_set,
                    f.volatility,
                    &args,
                    ndefaults,
                    f.ret.oid(),
                    f.is_variadic(),
                    &f.body,
                    "",
                );
                row[3] = o(f.owner);
                // Domain-typed parameters and results report the domain.
                if let Some(d) = f.ret_domain {
                    row[18] = o(d);
                }
                if f.arg_domains.iter().any(Option::is_some) {
                    row[19] = vector(
                        DataType::Oid,
                        args.iter()
                            .enumerate()
                            .map(|(i, a)| o(f.arg_domain(i).unwrap_or_else(|| a.oid())))
                            .collect(),
                    );
                }
                row[10] = b(f.security_definer);
                row[5] = Value::Float4(f.procost());
                row[6] = Value::Float4(f.prorows());
                row[11] = b(f.leakproof);
                row[15] = c(f.parallel);
                if !f.config.is_empty() {
                    row[28] = arr(
                        DataType::Text,
                        f.config
                            .iter()
                            .map(|s| Value::Text(Arc::from(s.as_str())))
                            .collect(),
                    );
                }
                // A SQL-standard body is kept parsed, not as source text: its analyzed
                // statements' node tree (or, beyond what that models, their text).
                if f.sql_body.is_some() {
                    row[25] = nm("");
                    row[27] = nm(crate::sql::nodetree::sql_body_tree(ctx, f)
                        .unwrap_or_else(|| f.body.clone()));
                }
                row[29] = acl_value(cat, cat.acl(ObjKind::Function, f.oid, 0));
                row[4] = o(match lang {
                    "sql" => 14,
                    "plpgsql" => 13681,
                    "c" => 13,
                    _ => 12,
                });
                if let Some(bin) = &f.probin {
                    row[26] = Value::Text(Arc::from(bin.as_str()));
                }
                let all = f.all_params();
                if !f.modes.is_empty() {
                    let (mut input, mut output) = (0, 0);
                    let oids = all
                        .iter()
                        .map(|p| match p.2 {
                            'i' | 'b' | 'v' => {
                                input += 1;
                                output += usize::from(p.2 == 'b');
                                o(f.arg_domain(input - 1).unwrap_or_else(|| p.1.oid()))
                            }
                            _ => {
                                output += 1;
                                o(f.out_domains
                                    .get(output - 1)
                                    .copied()
                                    .flatten()
                                    .unwrap_or_else(|| p.1.oid()))
                            }
                        })
                        .collect();
                    row[20] = arr(DataType::Oid, oids);
                    row[21] = arr(DataType::Char, f.modes.iter().map(|m| c(*m)).collect());
                }
                if all.iter().any(|p| p.0.is_some()) {
                    row[22] = arr(
                        DataType::Text,
                        all.iter()
                            .map(|p| nm(p.0.clone().unwrap_or_default()))
                            .collect(),
                    );
                }
                out.push(row);
            }
        }
        PgDatabase => {
            for (oid, name, owner) in ctx.sess.cluster.databases() {
                let owner = ctx
                    .sess
                    .cluster
                    .database_owner(&name)
                    .and_then(|n| cat.role_by_name(&n).map(|r| r.oid))
                    .unwrap_or(owner);
                let datacl = acl_value(cat, cat.acl(ObjKind::Database, oid, 0));
                // dathasloginevt: this database's login event triggers.
                let login = name == ctx.sess.database
                    && cat.event_triggers.values().any(|e| e.event == "login");
                out.push(vec![
                    o(oid),
                    nm(name),
                    o(owner),
                    i4(6),
                    c('b'),
                    b(false),
                    b(true),
                    b(login),
                    i4(-1),
                    Value::Oid(0),
                    Value::Oid(0),
                    o(1663),
                    nm("C"),
                    nm("C"),
                    nm("C.UTF-8"),
                    N,
                    N,
                    datacl,
                ]);
            }
        }
        PgAuthid | PgRoles | PgUser => {
            for r in cat.roles.values() {
                out.push(match table {
                    PgAuthid => vec![
                        o(r.oid),
                        nm(&r.name),
                        b(r.superuser),
                        b(r.inherit),
                        b(r.create_role),
                        b(r.create_db),
                        b(r.login),
                        b(r.replication),
                        b(r.bypass_rls),
                        i4(r.conn_limit as i64),
                        opt(r.password.as_ref()),
                        r.valid_until.map_or(N, Value::Timestamptz),
                    ],
                    PgRoles => vec![
                        nm(&r.name),
                        b(r.superuser),
                        b(r.inherit),
                        b(r.create_role),
                        b(r.create_db),
                        b(r.login),
                        b(r.replication),
                        i4(r.conn_limit as i64),
                        nm("********"),
                        r.valid_until.map_or(N, Value::Timestamptz),
                        b(r.bypass_rls),
                        role_config(cat, r.oid),
                        o(r.oid),
                    ],
                    _ => {
                        if !r.login {
                            continue;
                        }
                        vec![
                            nm(&r.name),
                            o(r.oid),
                            b(r.create_db),
                            b(r.superuser),
                            b(r.replication),
                            b(r.bypass_rls),
                            nm("********"),
                            r.valid_until.map_or(N, Value::Timestamptz),
                            role_config(cat, r.oid),
                        ]
                    }
                });
            }
        }
        PgPolicy => {
            for p in cat.policies.values() {
                let roles = p.roles.iter().map(|r| o(*r)).collect();
                // The expressions as their node trees (the SQL text if unmodelled).
                let tree = |sql: Option<&String>| match (sql, cat.tables.get(&p.table)) {
                    (Some(q), Some(t)) => {
                        nm(crate::sql::nodetree::policy_expr_tree(ctx, t, q)
                            .unwrap_or_else(|| q.clone()))
                    }
                    (q, _) => opt(q),
                };
                out.push(vec![
                    o(p.oid),
                    nm(&p.name),
                    o(p.table),
                    c(p.cmd),
                    b(p.permissive),
                    arr(DataType::Oid, roles),
                    tree(p.qual.as_ref()),
                    tree(p.with_check.as_ref()),
                ]);
            }
        }
        PgPolicies => {
            for p in cat.policies.values() {
                let Some(t) = cat.tables.get(&p.table) else {
                    continue;
                };
                let t = Arc::new((**t).clone());
                let show = |sql: &Option<String>| match sql {
                    Some(q) => nm(crate::sql::ruleutils::stored_expr(
                        ctx,
                        &t,
                        q,
                        Stored::Condition,
                        false,
                    )
                    .unwrap_or_else(|| q.clone())),
                    None => N,
                };
                let mut roles: Vec<String> = p
                    .roles
                    .iter()
                    .map(|r| {
                        if *r == 0 {
                            "public".to_string()
                        } else {
                            role_name(*r)
                        }
                    })
                    .collect();
                roles.sort();
                let cmd = match p.cmd {
                    'r' => "SELECT",
                    'a' => "INSERT",
                    'w' => "UPDATE",
                    'd' => "DELETE",
                    _ => "ALL",
                };
                out.push(vec![
                    nm(cat.schema_name(t.schema)),
                    nm(&t.name),
                    nm(&p.name),
                    nm(if p.permissive {
                        "PERMISSIVE"
                    } else {
                        "RESTRICTIVE"
                    }),
                    arr(DataType::Name, roles.into_iter().map(nm).collect()),
                    nm(cmd),
                    show(&p.qual),
                    show(&p.with_check),
                ]);
            }
        }
        PgTrigger => {
            for x in cat.triggers.values() {
                let mut args = Vec::new();
                for a in &x.args {
                    args.extend_from_slice(a.as_bytes());
                    args.push(0);
                }
                out.push(vec![
                    o(x.oid),
                    o(x.table),
                    o(x.parent),
                    nm(&x.name),
                    o(x.function),
                    i2(x.tgtype as i64),
                    c(x.enabled),
                    b(false),
                    o(x.constr_rel),
                    o(0),
                    o(x.con_oid),
                    b(x.deferrable),
                    b(x.initdeferred),
                    i2(x.args.len() as i64),
                    arr(
                        DataType::Int2,
                        x.columns.iter().map(|c| i2(*c as i64 + 1)).collect(),
                    ),
                    Value::bytea(args),
                    opt(x.when.as_deref()),
                    x.old_table.as_deref().map_or(N, nm),
                    x.new_table.as_deref().map_or(N, nm),
                ]);
            }
            // The internal triggers enforcing foreign keys (and deferrable unique keys).
            for r in crate::exec::trigger::internal_triggers(cat) {
                let x = &r.trig;
                out.push(vec![
                    o(x.oid),
                    o(x.table),
                    o(x.parent),
                    nm(&x.name),
                    o(x.function),
                    i2(x.tgtype as i64),
                    c(x.enabled),
                    b(true),
                    o(r.other),
                    o(r.index),
                    o(r.constraint),
                    b(x.deferrable),
                    b(x.initdeferred),
                    i2(0),
                    arr(DataType::Int2, Vec::new()),
                    Value::bytea(Vec::new()),
                    N,
                    N,
                    N,
                ]);
            }
        }
        PgAuthMembers => {
            // The predefined roles' memberships (pg_monitor's).
            for r in super::static_rows::rows(table).iter() {
                let at = super::static_rows::oid_at;
                if !cat
                    .members
                    .iter()
                    .any(|m| m.role == at(r, 1) && m.member == at(r, 2))
                {
                    out.push(r.clone());
                }
            }
            for m in &cat.members {
                out.push(vec![
                    o(m.oid),
                    o(m.role),
                    o(m.member),
                    o(m.grantor),
                    b(m.admin),
                    b(m.inherit),
                    b(m.set),
                ]);
            }
        }
        PgDbRoleSetting => {
            let dbs = ctx.sess.cluster.databases();
            for e in &cat.role_settings {
                let db = match &e.database {
                    Some(n) => dbs.iter().find(|d| &d.1 == n).map_or(0, |d| d.0),
                    None => 0,
                };
                out.push(vec![
                    o(db),
                    o(e.role),
                    arr(
                        DataType::Text,
                        e.settings
                            .iter()
                            .map(|(k, v)| nm(format!("{k}={v}")))
                            .collect(),
                    ),
                ]);
            }
        }
        PgLargeobject => {
            for (oid, pageno, data) in crate::largeobj::all_pages(&ctx.txn.borrow())? {
                out.push(vec![
                    o(oid),
                    i4(pageno as i64),
                    Value::Bytea(data.to_vec().into()),
                ]);
            }
        }
        PgLargeobjectMetadata => {
            for (oid, m) in crate::largeobj::all(&ctx.txn.borrow())? {
                out.push(vec![o(oid), o(m.owner), acl_value(cat, m.acl.as_ref())]);
            }
        }
        PgShdepend => {
            let los = crate::largeobj::all(&ctx.txn.borrow())?;
            let los: Vec<(u32, u32, Option<&acl::Acl>)> = los
                .iter()
                .map(|(o, m)| (*o, m.owner, m.acl.as_ref()))
                .collect();
            let db = ctx.sess.database_oid();
            let row = |db: u32, d: &crate::catalog::shdepend::ShDep| {
                vec![
                    o(if d.local { db } else { 0 }),
                    o(d.classid),
                    o(d.objid),
                    i4(d.objsubid as i64),
                    o(1260),
                    o(d.role),
                    c(d.deptype),
                ]
            };
            for d in cat.shdepends(&crate::catalog::shdepend::databases(ctx.sess), &los) {
                out.push(row(db, &d));
            }
            for (oid, _, deps) in crate::catalog::shdepend::remote(ctx.sess) {
                out.extend(deps.iter().map(|d| row(oid, d)));
            }
        }
        PgDefaultAcl => {
            for d in &cat.default_acls {
                out.push(vec![
                    o(d.oid),
                    o(d.role),
                    o(d.schema),
                    c(d.objtype),
                    acl_value(cat, Some(&d.acl)),
                ]);
            }
        }
        PgRange => {
            let reg = crate::sql::func::registry();
            let proc = |name: Option<&str>| {
                let Some(n) = name else { return 0 };
                reg.funcs
                    .get(n)
                    .and_then(|v| v.first())
                    .map(|d| d.oid)
                    .or_else(|| cat.functions.values().find(|f| f.name == n).map(|f| f.oid))
                    .unwrap_or(0)
            };
            for rt in crate::types::range::builtins()
                .iter()
                .cloned()
                .chain(cat.ranges.values().map(|r| r.range_type()))
            {
                let subopc = default_opclass(403, &rt.subtype);
                out.push(vec![
                    o(rt.oid),
                    o(type_oid(cat, &rt.subtype)),
                    o(rt.multi_oid),
                    o(rt.collation),
                    o(subopc),
                    o(proc(rt.canonical.as_deref())),
                    o(proc(rt.subdiff.as_deref())),
                ]);
            }
        }
        PgPartitionedTable => {
            for t in cat.tables.values() {
                let Some(key) = &t.partkey else { continue };
                let method = if key.strategy == super::PartStrategy::Hash {
                    405
                } else {
                    403
                };
                let def = cat.partitions(t.oid).into_iter().find(|p| {
                    cat.tables
                        .get(p)
                        .is_some_and(|p| p.partbound.as_ref().is_some_and(|b| b.is_default()))
                });
                let exprs: Vec<String> = key.cols.iter().filter_map(|k| k.expr.clone()).collect();
                out.push(vec![
                    o(t.oid),
                    c(key.strategy.code()),
                    i2(key.cols.len() as i64),
                    o(def.unwrap_or(0)),
                    vector(
                        DataType::Int2,
                        key.cols
                            .iter()
                            .map(|k| i2(k.col.map_or(0, |i| i as i64 + 1)))
                            .collect(),
                    ),
                    vector(
                        DataType::Oid,
                        key.cols
                            .iter()
                            .map(|k| {
                                o(match &k.opclass {
                                    Some(n) => OPCLASSES
                                        .iter()
                                        .find(|c| c.2 == n.as_str() && c.1 == method)
                                        .map_or(0, |c| c.0),
                                    None => default_opclass(method, &k.ty),
                                })
                            })
                            .collect(),
                    ),
                    vector(
                        DataType::Oid,
                        key.cols
                            .iter()
                            .map(|k| {
                                o(match &k.collation {
                                    _ if type_collation(&k.ty) == 0 => 0,
                                    Some(n) => collation_oid(n).unwrap_or(0),
                                    None => k.col.map_or_else(
                                        || type_collation(&k.ty),
                                        |i| column_collation(&t.columns[i]),
                                    ),
                                })
                            })
                            .collect(),
                    ),
                    if exprs.is_empty() {
                        N
                    } else {
                        nm(exprs.join(" "))
                    },
                ]);
            }
        }
        PgInherits => {
            for t in cat.tables.values() {
                for (i, p) in t.inherits.iter().enumerate() {
                    out.push(vec![o(t.oid), o(*p), i4(i as i64 + 1), b(false)]);
                }
            }
            for ix in cat.indexes.values().filter(|ix| ix.parent != 0) {
                out.push(vec![o(ix.oid), o(ix.parent), i4(1), b(false)]);
            }
        }
        PgRewrite => {
            out.extend(super::static_rows::rows(table).iter().cloned());
            let arc = ctx.catalog();
            for v in cat.views.values() {
                // The rule's action as Postgres stores it (its query's text when that
                // isn't modelled).
                let names: Vec<String> = v.columns.iter().map(|c| c.0.clone()).collect();
                let action = crate::sql::nodetree::view_rule_tree(ctx, &arc, &v.query, &names)
                    .unwrap_or_else(|| v.query.clone());
                out.push(vec![
                    o(v.rule_oid()),
                    nm("_RETURN"),
                    o(v.oid),
                    c('1'),
                    c('O'),
                    b(true),
                    nm("<>"),
                    nm(action),
                ]);
            }
            // CREATE RULE rules (their condition and actions as SQL text).
            for r in cat.rules.values() {
                out.push(vec![
                    o(r.oid),
                    nm(&r.name),
                    o(r.table),
                    c(r.event),
                    c(r.enabled),
                    b(r.instead),
                    nm(r.qual.as_deref().unwrap_or("<>")),
                    nm(format!("({})", r.actions.join("; "))),
                ]);
            }
        }
        PgShdescription => {
            let dbs: Vec<u32> = ctx.sess.cluster.databases().iter().map(|d| d.0).collect();
            // The template and maintenance databases' comments, unless changed.
            let all = ctx.sess.cluster.databases();
            for r in super::static_rows::rows(table).iter() {
                let oid = super::static_rows::oid_at(r, 0);
                if all
                    .iter()
                    .any(|d| d.0 == oid && builtin_database(oid) == Some(d.1.as_str()))
                    && !cat.comments.iter().any(|(o, _, _)| *o == oid)
                {
                    out.push(r.clone());
                }
            }
            for (oid, _, text) in cat.comments.iter().filter(|c| !c.2.is_empty()) {
                if let Some(class) = cat.shared_class_of(*oid, &dbs) {
                    out.push(vec![o(*oid), o(class), nm(text)]);
                }
            }
        }
        PgForeignTable => {
            for t in cat.tables.values() {
                if let Some(f) = &t.foreign {
                    out.push(vec![
                        o(t.oid),
                        o(f.server),
                        f.options
                            .as_ref()
                            .map_or(N, |v| arr(DataType::Text, v.iter().map(nm).collect())),
                    ]);
                }
            }
        }
        PgEventTrigger => {
            for e in cat.event_triggers.values() {
                out.push(vec![
                    o(e.oid),
                    nm(&e.name),
                    nm(&e.event),
                    o(e.owner),
                    o(e.function),
                    c(e.enabled),
                    match &e.tags {
                        Some(t) => arr(DataType::Text, t.iter().map(nm).collect()),
                        None => N,
                    },
                ]);
            }
        }
        PgPublication => {
            for p in cat.publications.values() {
                out.push(vec![
                    o(p.oid),
                    nm(&p.name),
                    o(p.owner),
                    b(p.all_tables),
                    b(p.insert),
                    b(p.update),
                    b(p.delete),
                    b(p.truncate),
                    b(p.via_root),
                    c(p.gencols),
                ]);
            }
        }
        PgPublicationRel => {
            for p in cat.publications.values() {
                for r in &p.rels {
                    out.push(vec![
                        o(r.oid),
                        o(p.oid),
                        o(r.rel),
                        r.qual.as_ref().map_or(N, nm),
                        r.cols.as_ref().map_or(N, |cols| {
                            vector(DataType::Int2, cols.iter().map(|a| i2(*a as i64)).collect())
                        }),
                    ]);
                }
            }
        }
        PgSubscription => {
            for x in cat.subscriptions.values() {
                out.push(vec![
                    o(x.oid),
                    o(x.dbid),
                    Value::PgLsn(x.skiplsn),
                    nm(&x.name),
                    o(x.owner),
                    b(x.enabled),
                    b(x.binary),
                    c(x.stream),
                    c(x.twophase),
                    b(x.disable_on_err),
                    b(x.password_required),
                    b(x.run_as_owner),
                    b(x.failover),
                    nm(&x.conninfo),
                    x.slot_name.as_ref().map_or(N, nm),
                    nm(&x.synccommit),
                    arr(DataType::Text, x.publications.iter().map(nm).collect()),
                    nm(&x.origin),
                ]);
            }
        }
        PgPublicationNamespace => {
            for p in cat.publications.values() {
                for sc in &p.schemas {
                    out.push(vec![o(sc.oid), o(p.oid), o(sc.schema)]);
                }
            }
        }
        PgLocks => {
            out.extend(crate::session::ddl::stats_import::relation_lock_rows(ctx));
            out.extend(crate::sql::func::system::advisory_lock_rows(ctx));
        }
        IsSqlFeatures | IsSqlImplementationInfo | IsSqlParts | IsSqlSizing => {
            out.extend(super::static_rows::rows(table).iter().cloned())
        }
        PgOpclass => {
            out.extend(super::static_rows::rows(table).iter().cloned());
            for x in cat.opclasses.values() {
                out.push(vec![
                    o(x.oid),
                    o(x.method),
                    nm(&x.name),
                    o(x.schema),
                    o(x.owner),
                    o(x.family),
                    o(x.intype),
                    b(x.default),
                    o(x.keytype),
                ]);
            }
        }
        PgOpfamily => {
            out.extend(super::static_rows::rows(table).iter().cloned());
            for x in cat.opfamilies.values() {
                out.push(vec![o(x.oid), o(x.method), nm(&x.name), o(x.schema), o(x.owner)]);
            }
        }
        PgAm => {
            out.extend(super::static_rows::rows(table).iter().cloned());
            for x in cat.access_methods.values() {
                out.push(vec![o(x.oid), nm(&x.name), o(x.handler), c(x.kind)]);
            }
        }
        PgAmop | PgAmproc => {
            let func = table == PgAmproc;
            out.extend(
                super::static_rows::rows(table)
                    .iter()
                    .filter(|r| {
                        !cat.opmembers_removed
                            .contains(&super::static_rows::oid_at(r, 0))
                    })
                    .cloned(),
            );
            for m in cat.opmembers.values().filter(|m| m.func == func) {
                let x = cat.opmember_info(func, m.oid).unwrap();
                out.push(if func {
                    vec![
                        o(x.oid),
                        o(x.family),
                        o(x.left),
                        o(x.right),
                        i2(x.number.into()),
                        o(x.object),
                    ]
                } else {
                    vec![
                        o(x.oid),
                        o(x.family),
                        o(x.left),
                        o(x.right),
                        i2(x.number.into()),
                        c(x.purpose),
                        o(x.object),
                        o(x.method),
                        o(x.sortfamily),
                    ]
                });
            }
        }
        PgConversion => {
            out.extend(super::static_rows::rows(table).iter().cloned());
            for x in cat.conversions.values() {
                out.push(vec![
                    o(x.oid),
                    nm(&x.name),
                    o(x.schema),
                    o(x.owner),
                    i4(x.for_encoding.into()),
                    i4(x.to_encoding.into()),
                    o(x.proc),
                    b(x.default),
                ]);
            }
        }
        PgForeignDataWrapper => {
            let opts = |o: &Option<Vec<String>>| {
                o.as_ref()
                    .map_or(N, |v| arr(DataType::Text, v.iter().map(nm).collect()))
            };
            let mut fdws: Vec<_> = cat.fdws.values().collect();
            fdws.sort_by_key(|f| f.heap_order);
            for f in fdws {
                out.push(vec![
                    o(f.oid),
                    nm(&f.name),
                    o(f.owner),
                    o(f.handler),
                    o(f.validator),
                    acl_value(cat, cat.acl(ObjKind::ForeignDataWrapper, f.oid, 0)),
                    opts(&f.options),
                ]);
            }
        }
        PgForeignServer => {
            let opts = |o: &Option<Vec<String>>| {
                o.as_ref()
                    .map_or(N, |v| arr(DataType::Text, v.iter().map(nm).collect()))
            };
            let mut servers: Vec<_> = cat.servers.values().collect();
            servers.sort_by_key(|x| x.heap_order);
            for x in servers {
                out.push(vec![
                    o(x.oid),
                    nm(&x.name),
                    o(x.owner),
                    o(x.fdw),
                    x.kind.as_ref().map_or(N, nm),
                    x.version.as_ref().map_or(N, nm),
                    acl_value(cat, cat.acl(ObjKind::ForeignServer, x.oid, 0)),
                    opts(&x.options),
                ]);
            }
        }
        PgUserMapping => {
            for m in cat.user_mappings.values() {
                out.push(vec![
                    o(m.oid),
                    o(m.user),
                    o(m.server),
                    m.options
                        .as_ref()
                        .map_or(N, |v| arr(DataType::Text, v.iter().map(nm).collect())),
                ]);
            }
        }
        PgTransform => {
            for x in cat.transforms.values() {
                out.push(vec![o(x.oid), o(x.ty), o(x.lang), o(x.fromsql), o(x.tosql)]);
            }
        }
        PgCollation => {
            out.extend(super::static_rows::rows(table).iter().cloned());
            let t = |s: &Option<String>| s.as_ref().map_or(N, nm);
            for d in cat.collations.values() {
                out.push(vec![
                    o(d.oid),
                    nm(&d.name),
                    o(d.namespace),
                    o(d.owner),
                    Value::Char(d.provider as u8),
                    b(d.deterministic),
                    i4(d.encoding as i64),
                    t(&d.collate),
                    t(&d.ctype),
                    t(&d.locale),
                    t(&d.icu_rules),
                    t(&d.version),
                ]);
            }
        }
        PgInitPrivs => {
            // Initial privileges name the bootstrap superuser.
            let boot = role_name(OWNER);
            for r in super::static_rows::rows(table).iter() {
                let mut r = r.clone();
                r[4] = boot_acl(&r[4], &boot);
                out.push(r);
            }
        }
        PgPreparedStatements => {
            for (name, text, at, params, result, from_sql, generic, custom) in
                ctx.sess.prepared_list.borrow().iter()
            {
                let types = |ts: &[DataType]| {
                    arr(
                        DataType::Regtype,
                        ts.iter().map(|t| o(type_oid(cat, t))).collect(),
                    )
                };
                out.push(vec![
                    nm(name),
                    nm(text),
                    Value::Timestamptz(*at),
                    types(params),
                    result.as_deref().map(types).unwrap_or(N),
                    b(*from_sql),
                    i8v(*generic as i64),
                    i8v(*custom as i64),
                ]);
            }
        }
        PgCursors => {
            for (name, c) in ctx.sess.cursors.borrow().iter() {
                let (text, hold, scroll, at) = (&c.text, &c.hold, &c.scroll, &c.created);
                // (CURSOR_OPT_BINARY of the DECLARE it was opened by.)
                let binary = pg_query::parse(text, 0).ok().is_some_and(|p| {
                    p.protobuf.stmts.iter().any(|st| {
                        matches!(
                            st.stmt.as_ref().and_then(|n| n.node.as_ref()),
                            Some(pg_query::NodeEnum::DeclareCursorStmt(d)) if d.options & 1 != 0
                        )
                    })
                });
                out.push(vec![
                    nm(name),
                    nm(text),
                    b(*hold),
                    b(binary),
                    b(*scroll),
                    Value::Timestamptz(*at),
                ]);
            }
        }
        PgDescription => {
            let dbs: Vec<u32> = ctx.sess.cluster.databases().iter().map(|d| d.0).collect();
            for r in super::static_rows::rows(table).iter() {
                let (oid, class) = (
                    super::static_rows::oid_at(r, 0),
                    super::static_rows::oid_at(r, 1),
                );
                if builtin_object_exists(cat, oid, class)
                    && !cat
                        .comments
                        .iter()
                        .any(|(o, s, _)| *o == oid && Value::Int4(*s) == r[2])
                {
                    out.push(r.clone());
                }
            }
            for (oid, sub, text) in cat.comments.iter().filter(|c| !c.2.is_empty()) {
                if cat.shared_class_of(*oid, &dbs).is_some() {
                    continue;
                }
                let class = cat
                    .class_of(*oid)
                    .unwrap_or(crate::catalog::depend::CONSTRAINT);
                out.push(vec![o(*oid), o(class), i4(*sub as i64), nm(text)]);
            }
            for (oid, m) in crate::largeobj::all(&ctx.txn.borrow())? {
                if let Some(text) = &m.comment {
                    out.push(vec![o(oid), o(crate::largeobj::CLASS_OID), i4(0), nm(text)]);
                }
            }
            for (oid, class, text) in extension_descriptions(cat) {
                if !cat.comments.iter().any(|(o, s, _)| *o == oid && *s == 0) {
                    out.push(vec![o(oid), o(class), i4(0), nm(text)]);
                }
            }
        }
        PgAttrdef => {
            for t in cat.tables.values() {
                for (i, col) in t.live_columns() {
                    if let Some(d) = col.default.as_ref().or(col.generated.as_ref()) {
                        out.push(vec![
                            o(col.def_oid),
                            o(t.oid),
                            i2(i as i64 + 1),
                            nm(shown_default(ctx, t, d, &col.ty)),
                        ]);
                    }
                }
            }
        }
        PgSequence => {
            for s in cat.sequences.values() {
                out.push(vec![
                    o(s.oid),
                    o(s.ty.oid()),
                    i8v(s.start),
                    i8v(s.increment),
                    i8v(s.max),
                    i8v(s.min),
                    i8v(s.cache),
                    b(s.cycle),
                ]);
            }
        }
        PgSequences => {
            for s in cat.sequences.values() {
                if cat.tables.values().any(|t| t.rowid_seq == Some(s.oid)) {
                    continue;
                }
                let (last, called) = ctx.sess.db.seq_state(s)?;
                out.push(vec![
                    nm(cat.schema_name(s.schema)),
                    nm(&s.name),
                    nm(role_name(s.owner)),
                    o(s.ty.oid()),
                    i8v(s.start),
                    i8v(s.min),
                    i8v(s.max),
                    i8v(s.increment),
                    b(s.cycle),
                    i8v(s.cache),
                    if called { i8v(last) } else { N },
                ]);
            }
        }
        PgEnum => {
            for e in cat.enums.values() {
                for (i, l) in e.labels.iter().enumerate() {
                    out.push(vec![
                        o(e.label_oid(i)),
                        o(e.oid),
                        Value::Float4(e.sort_order(i)),
                        nm(l),
                    ]);
                }
            }
        }
        PgExtension => {
            out.push(vec![
                o(13677),
                nm("plpgsql"),
                o(OWNER),
                o(PGC),
                b(false),
                nm("1.0"),
                N,
                N,
            ]);
            for (oid, name, schema, version) in &cat.extensions {
                let info = cat.extension_info.get(oid);
                let config = info.filter(|i| !i.config.is_empty());
                out.push(vec![
                    o(*oid),
                    nm(name),
                    o(info.map_or(OWNER, |i| i.owner)),
                    o(*schema),
                    b(info.is_none_or(|i| i.relocatable)),
                    nm(version),
                    config.map_or(N, |i| arr(DataType::Oid, i.config.iter().map(|c| o(c.0)).collect())),
                    config.map_or(N, |i| arr(DataType::Text, i.config.iter().map(|c| nm(&c.1)).collect())),
                ]);
            }
        }
        PgAvailableExtensions => {
            for (name, version, comment) in crate::ext::available() {
                let installed = if name == "plpgsql" {
                    Some("1.0".to_string())
                } else {
                    cat.extensions.iter().find(|e| e.1 == name).map(|e| e.3.clone())
                };
                out.push(vec![
                    nm(&name),
                    nm(&version),
                    installed.map_or(N, |v| nm(&v)),
                    nm(&comment),
                ]);
            }
        }
        PgTsConfig => {
            for (oid, _, _) in cat.ts_objects(super::tsearch::TsKind::Config) {
                let Some(cf) = cat.ts_config(oid) else {
                    continue;
                };
                out.push(vec![
                    o(cf.oid),
                    nm(&cf.name),
                    o(cf.namespace),
                    o(cf.owner),
                    o(cf.parser),
                ]);
            }
        }
        PgTsConfigMap => {
            for (oid, _, _) in cat.ts_objects(super::tsearch::TsKind::Config) {
                let Some(cf) = cat.ts_config(oid) else {
                    continue;
                };
                for (t, dicts) in cf.map.iter().enumerate() {
                    for (seq, d) in dicts.iter().enumerate() {
                        out.push(vec![o(cf.oid), i4(t as i64), i4(seq as i64 + 1), o(*d)]);
                    }
                }
            }
        }
        PgTsDict => {
            for (oid, _, _) in cat.ts_objects(super::tsearch::TsKind::Dict) {
                let Some(d) = cat.ts_dict(oid) else { continue };
                let opts = if d.options.is_empty() {
                    N
                } else {
                    nm(crate::types::tsearch::config::serialize_options(&d.options))
                };
                out.push(vec![
                    o(d.oid),
                    nm(&d.name),
                    o(d.namespace),
                    o(d.owner),
                    o(d.template),
                    opts,
                ]);
            }
        }
        PgTsParser => {
            for (oid, _, _) in cat.ts_objects(super::tsearch::TsKind::Parser) {
                let Some(p) = cat.ts_parser(oid) else {
                    continue;
                };
                out.push(vec![
                    o(p.oid),
                    nm(&p.name),
                    o(p.namespace),
                    o(p.start),
                    o(p.token),
                    o(p.end),
                    o(p.headline),
                    o(p.lextype),
                ]);
            }
        }
        PgTsTemplate => {
            for (oid, _, _) in cat.ts_objects(super::tsearch::TsKind::Template) {
                let Some(t) = cat.ts_template(oid) else {
                    continue;
                };
                out.push(vec![
                    o(t.oid),
                    nm(&t.name),
                    o(t.namespace),
                    o(t.init),
                    o(t.lexize),
                ]);
            }
        }
        PgDepend => {
            // Postgres's own objects' dependencies, then those of objects made here (with
            // what superuser DML on pg_depend took out or put in).
            let cat = ctx.catalog();
            let edits = &cat.depend_edits;
            let removed = |r: &Row| {
                let n = |i: usize| match &r[i] {
                    Value::Oid(o) => *o,
                    _ => 0,
                };
                let s = |i: usize| match &r[i] {
                    Value::Int4(v) => *v,
                    _ => 0,
                };
                let k = match &r[6] {
                    Value::Char(c) => *c as char,
                    _ => '\0',
                };
                edits
                    .removed
                    .contains(&((n(0), n(1), s(2)), (n(3), n(4), s(5)), k))
            };
            out.extend(
                super::static_rows::rows(table)
                    .iter()
                    .filter(|r| edits.removed.is_empty() || !removed(r))
                    .cloned(),
            );
            for d in super::depend::depends(ctx, &cat).into_iter().filter(|d| {
                d.obj.1 >= super::FIRST_USER_OID
                    || edits.added.contains(&(d.obj, d.refobj, d.kind))
            }) {
                out.push(vec![
                    o(d.obj.0),
                    o(d.obj.1),
                    i4(d.obj.2 as i64),
                    o(d.refobj.0),
                    o(d.refobj.1),
                    i4(d.refobj.2 as i64),
                    c(d.kind),
                ]);
            }
        }
        PgOperator => {
            out.extend(super::static_rows::rows(table).iter().cloned());
            let est = |n: &Option<String>, join: bool| {
                o(n.as_ref().map_or(0, |n| cat.estimator_oid(n, join)))
            };
            for x in cat.operators.values() {
                out.push(vec![
                    o(x.oid),
                    nm(&x.name),
                    o(x.schema),
                    o(x.owner),
                    c(if x.left.is_some() { 'b' } else { 'l' }),
                    b(x.merges),
                    b(x.hashes),
                    o(x.left.as_ref().map_or(0, |t| t.oid())),
                    o(x.right.oid()),
                    o(x.result.as_ref().map_or(0, |t| t.oid())),
                    o(x.commutator),
                    o(x.negator),
                    o(x.func),
                    est(&x.restrict, false),
                    est(&x.join, true),
                ]);
            }
        }
        PgParameterAcl => {
            for (oid, name) in &cat.parameters {
                out.push(vec![
                    o(*oid),
                    nm(name),
                    acl_value(cat, cat.acl(ObjKind::Parameter, *oid, 0)),
                ]);
            }
        }
        PgTablespace => {
            for (oid, name) in [(1663, "pg_default"), (1664, "pg_global")] {
                out.push(vec![
                    o(oid),
                    nm(name),
                    o(OWNER),
                    acl_value(cat, cat.acl(ObjKind::Tablespace, oid, 0)),
                    N,
                ]);
            }
            for t in cat.tablespaces.values() {
                out.push(vec![
                    o(t.oid),
                    nm(&t.name),
                    o(t.owner),
                    acl_value(cat, cat.acl(ObjKind::Tablespace, t.oid, 0)),
                    t.options
                        .as_ref()
                        .map_or(N, |v| arr(DataType::Text, v.iter().map(nm).collect())),
                ]);
            }
        }
        PgLanguage => {
            for l in cat.languages_all() {
                out.push(vec![
                    o(l.oid),
                    nm(&l.name),
                    o(l.owner),
                    b(l.handler != 0),
                    b(l.trusted),
                    o(l.handler),
                    o(l.inline),
                    o(l.validator),
                    acl_value(cat, cat.acl(ObjKind::Language, l.oid, 0)),
                ]);
            }
        }
        PgCast => {
            out.extend(super::static_rows::rows(table).iter().cloned());
            for x in cat.casts.values() {
                out.push(vec![
                    o(x.oid),
                    o(x.source),
                    o(x.target),
                    o(x.func),
                    c(x.context),
                    c(x.method),
                ]);
            }
            for r in cat.ranges.values().filter(|r| r.fn_oids.len() == 6) {
                out.push(vec![
                    o(r.fn_oids[5]),
                    o(r.oid),
                    o(r.multi_oid),
                    o(r.fn_oids[3]),
                    c('e'),
                    c('f'),
                ]);
            }
        }
        PgAggregate => {
            out.extend(super::static_rows::rows(table).iter().cloned());
            for f in cat.functions.values() {
                let Some(a) = f.agg.as_deref() else { continue };
                let opt = |v: &Option<String>| {
                    v.as_ref().map_or(N, |s| Value::Text(Arc::from(s.as_str())))
                };
                out.push(vec![
                    o(f.oid),
                    c(a.kind),
                    i2(a.num_direct.into()),
                    o(a.transfn),
                    o(a.finalfn),
                    o(a.combinefn),
                    o(a.serialfn),
                    o(a.deserialfn),
                    o(a.mtransfn),
                    o(a.minvtransfn),
                    o(a.mfinalfn),
                    b(a.finalextra),
                    b(a.mfinalextra),
                    c(a.finalmodify),
                    c(a.mfinalmodify),
                    o(a.sortop),
                    o(a.transtype.oid()),
                    i4(a.transspace.into()),
                    o(a.mtranstype.as_ref().map_or(0, |t| t.oid())),
                    i4(a.mtransspace.into()),
                    opt(&a.initval),
                    opt(&a.minitval),
                ]);
            }
        }
        PgSettings => {
            let settings = ctx.sess.settings.borrow();
            for r in settings.all() {
                out.push(vec![
                    nm(&r.name),
                    nm(&r.setting),
                    opt(r.unit),
                    nm(r.category),
                    nm(r.short_desc),
                    opt(r.extra_desc),
                    nm(r.context),
                    nm(r.vartype),
                    nm(&r.source),
                    opt(r.min_val),
                    opt(r.max_val),
                    r.enumvals.map_or(N, |v| {
                        arr(DataType::Text, v.iter().map(|x| nm(*x)).collect())
                    }),
                    nm(&r.boot_val),
                    nm(&r.reset_val),
                    N,
                    N,
                    b(false),
                ]);
            }
        }
        PgTables => {
            // relhastriggers: foreign keys' internal triggers count.
            let triggered = crate::exec::trigger::triggered_tables(cat, true);
            let spc = |rel: u32| {
                cat.tablespace_name(cat.rel_tablespace(rel)).map_or(N, nm)
            };
            for t in cat.tables.values().filter(|t| t.foreign.is_none()) {
                let triggers = triggered.contains(&t.oid);
                out.push(vec![
                    nm(cat.schema_name(t.schema)),
                    nm(&t.name),
                    nm(role_name(t.owner)),
                    spc(t.oid),
                    b(!t.indexes.is_empty() || t.pkey_index.is_some()),
                    b(cat.class_has_rules.contains(&t.oid)),
                    b(triggers),
                    b(t.rls),
                ]);
            }
            for st in SysTable::all().filter(|s| !s.is_view()) {
                out.push(vec![
                    nm("pg_catalog"),
                    nm(st.name()),
                    nm(role_name(OWNER)),
                    N,
                    b(false),
                    b(false),
                    b(false),
                    b(false),
                ]);
            }
        }
        PgViews => {
            // Postgres's own views (in schemas still there).
            for d in system_view_defs() {
                if cat.schema_by_name(&d.schema).is_none() {
                    continue;
                }
                out.push(vec![
                    nm(&d.schema),
                    nm(&d.name),
                    nm(role_name(OWNER)),
                    nm(crate::sql::scs::session_literals(&d.def)),
                ]);
            }
            for v in cat.views.values().filter(|v| !v.materialized) {
                out.push(vec![
                    nm(cat.schema_name(v.schema)),
                    nm(&v.name),
                    nm(role_name(v.owner)),
                    nm(crate::sql::scs::session_literals(
                        &crate::sql::ruleutils::view_def(ctx, v, false)
                            .unwrap_or_else(|| format!(" {};", v.query)),
                    )),
                ]);
            }
        }
        PgMatviews => {
            for v in cat.views.values().filter(|v| v.materialized) {
                out.push(vec![
                    nm(cat.schema_name(v.schema)),
                    nm(&v.name),
                    nm(role_name(v.owner)),
                    cat.tablespace_name(cat.rel_tablespace(v.oid)).map_or(N, nm),
                    b(v.storage.as_ref().is_some_and(|t| !t.indexes.is_empty())),
                    b(v.populated),
                    nm(crate::sql::scs::session_literals(
                        &crate::sql::ruleutils::view_def(ctx, v, false)
                            .unwrap_or_else(|| format!(" {};", v.query)),
                    )),
                ]);
            }
        }
        PgIndexes => {
            for ix in cat.indexes.values() {
                let Some(t) = cat.index_table(ix.table) else {
                    continue;
                };
                out.push(vec![
                    nm(cat.schema_name(t.schema)),
                    nm(&t.name),
                    nm(&ix.name),
                    cat.tablespace_name(cat.rel_tablespace(ix.oid)).map_or(N, nm),
                    opt(index_def(ctx, ix.oid, false)),
                ]);
            }
        }
        PgTimezoneNames => {
            use chrono::{Offset, TimeZone};
            let now = chrono::Utc::now().naive_utc();
            for tz in chrono_tz::TZ_VARIANTS {
                let off = tz.offset_from_utc_datetime(&now);
                let secs = off.fix().local_minus_utc();
                let abbrev = chrono_tz::OffsetName::abbreviation(&off)
                    .map(str::to_string)
                    .unwrap_or_else(|| tz_offset_abbrev(secs));
                let dst = chrono_tz::OffsetComponents::dst_offset(&off).num_seconds() != 0;
                out.push(vec![
                    nm(tz.name()),
                    nm(abbrev),
                    Value::Interval(crate::types::value::Interval::new(
                        0,
                        0,
                        secs as i64 * 1_000_000,
                    )),
                    b(dst),
                ]);
            }
        }
        IsSchemata => {
            for s in cat.schemas.values() {
                out.push(vec![
                    nm(&dbname),
                    nm(&s.name),
                    nm(role_name(s.owner)),
                    N,
                    N,
                    N,
                    N,
                ]);
            }
        }
        IsTables => {
            for t in cat.tables.values() {
                let kind = if t.temporary {
                    "LOCAL TEMPORARY"
                } else if t.foreign.is_some() {
                    "FOREIGN"
                } else {
                    "BASE TABLE"
                };
                out.push(vec![
                    nm(&dbname),
                    nm(cat.schema_name(t.schema)),
                    nm(&t.name),
                    nm(kind),
                    N,
                    N,
                    N,
                    N,
                    N,
                    yn(t.foreign.is_none()),
                    yn(false),
                    N,
                ]);
            }
            for v in cat.views.values().filter(|v| !v.materialized) {
                let (mask, _) = crate::sql::binder::views::view_updatability(ctx, v, false);
                out.push(vec![
                    nm(&dbname),
                    nm(cat.schema_name(v.schema)),
                    nm(&v.name),
                    nm("VIEW"),
                    N,
                    N,
                    N,
                    N,
                    N,
                    yn(mask & 8 == 8),
                    yn(false),
                    N,
                ]);
            }
            for st in SysTable::all() {
                let kind = if st.is_view() { "VIEW" } else { "BASE TABLE" };
                out.push(vec![
                    nm(&dbname),
                    nm(cat.schema_name(st.schema())),
                    nm(st.name()),
                    nm(kind),
                    N,
                    N,
                    N,
                    N,
                    N,
                    yn(!st.is_view()),
                    yn(false),
                    N,
                ]);
            }
        }
        IsColumns => {
            let mut rels: Vec<(u32, u32, String, bool)> = cat
                .tables
                .values()
                .map(|t| (t.oid, t.schema, t.name.clone(), true))
                .collect();
            rels.extend(
                cat.views
                    .values()
                    .map(|v| (v.oid, v.schema, v.name.clone(), false)),
            );
            rels.extend(
                SysTable::all().map(|s| (s.oid(), s.schema(), s.name().to_string(), false)),
            );
            for (oid, schema, name, is_table) in rels {
                // Views: pg_column_is_updatable.
                let updatable = cat
                    .views
                    .get(&oid)
                    .map(|v| crate::sql::binder::views::view_updatability(ctx, v, false).1)
                    .unwrap_or_default();
                let mut pos = 0;
                for (_, cname, ty, col) in rel_columns(cat, oid, ctx) {
                    if col.as_ref().is_some_and(|c| c.dropped) {
                        continue;
                    }
                    pos += 1;
                    let shown = |d: &String| {
                        cat.tables
                            .get(&oid)
                            .map_or_else(|| d.clone(), |t| shown_default(ctx, t, d, &ty))
                    };
                    let default = col.as_ref().and_then(|c| c.default.as_ref()).map(shown);
                    let generated = col.as_ref().and_then(|c| c.generated.as_ref()).map(shown);
                    let upd = is_table || updatable.get(pos as usize - 1).copied().unwrap_or(false);
                    out.push(info_column(
                        cat,
                        &dbname,
                        schema,
                        &name,
                        &cname,
                        pos,
                        &ty,
                        col.as_ref(),
                        upd,
                        default,
                        generated,
                    ));
                }
            }
        }
        IsTableConstraints => {
            for t in cat.tables.values() {
                for con in &t.constraints {
                    let kind = match con.kind {
                        ConstraintKind::PrimaryKey { .. } => "PRIMARY KEY",
                        ConstraintKind::Unique { .. } => "UNIQUE",
                        ConstraintKind::Check { .. } | ConstraintKind::NotNull { .. } => "CHECK",
                        ConstraintKind::ForeignKey { .. } => "FOREIGN KEY",
                        // (a key WITHOUT OVERLAPS is one: contype p / u)
                        ConstraintKind::Exclusion { .. } if con.period == Some('p') => {
                            "PRIMARY KEY"
                        }
                        ConstraintKind::Exclusion { .. } if con.period == Some('u') => "UNIQUE",
                        ConstraintKind::Exclusion { .. } => continue,
                    };
                    let nulls = match con.kind {
                        ConstraintKind::Exclusion { .. } => Some("YES"),
                        ConstraintKind::Unique { index } | ConstraintKind::PrimaryKey { index } => {
                            Some(
                                if cat
                                    .indexes
                                    .get(&index)
                                    .is_some_and(|i| i.nulls_not_distinct)
                                {
                                    "NO"
                                } else {
                                    "YES"
                                },
                            )
                        }
                        _ => None,
                    };
                    let (deferrable, deferred) = con.deferral();
                    let sch = cat.schema_name(t.schema);
                    out.push(vec![
                        nm(&dbname),
                        nm(sch),
                        nm(&con.name),
                        nm(&dbname),
                        nm(sch),
                        nm(&t.name),
                        nm(kind),
                        yn(deferrable),
                        yn(deferred),
                        yn(true),
                        opt(nulls),
                    ]);
                }
            }
        }
        IsKeyColumnUsage => {
            for t in cat.tables.values() {
                let sch = cat.schema_name(t.schema);
                for con in &t.constraints {
                    let (cols, refpos): (Vec<usize>, Option<Vec<i64>>) = match &con.kind {
                        ConstraintKind::PrimaryKey { index }
                        | ConstraintKind::Unique { index }
                        | ConstraintKind::Exclusion { index, .. }
                            if !matches!(con.kind, ConstraintKind::Exclusion { .. })
                                || con.without_overlaps() =>
                        {
                            let Some(ix) = cat.indexes.get(index) else {
                                continue;
                            };
                            (
                                index_cols(ix)
                                    .iter()
                                    .take(ix.columns.len())
                                    .filter(|a| **a > 0)
                                    .map(|a| *a as usize - 1)
                                    .collect(),
                                None,
                            )
                        }
                        ConstraintKind::ForeignKey {
                            columns,
                            ref_columns,
                            ..
                        } => {
                            // position_in_unique_constraint: position of the referenced column in the parent's key.
                            (
                                columns.clone(),
                                Some(
                                    ref_columns
                                        .iter()
                                        .enumerate()
                                        .map(|(i, _)| i as i64 + 1)
                                        .collect(),
                                ),
                            )
                        }
                        _ => continue,
                    };
                    for (i, col) in cols.iter().enumerate() {
                        out.push(vec![
                            nm(&dbname),
                            nm(sch),
                            nm(&con.name),
                            nm(&dbname),
                            nm(sch),
                            nm(&t.name),
                            nm(&t.columns[*col].name),
                            i4(i as i64 + 1),
                            refpos.as_ref().map(|r| i4(r[i])).unwrap_or(N),
                        ]);
                    }
                }
            }
        }
        IsReferentialConstraints => {
            for t in cat.tables.values() {
                for con in &t.constraints {
                    let ConstraintKind::ForeignKey {
                        ref_table,
                        ref_columns,
                        on_delete,
                        on_update,
                        match_full,
                        ..
                    } = &con.kind
                    else {
                        continue;
                    };
                    let parent = cat.tables.get(ref_table);
                    let uniq = parent.and_then(|p| {
                        p.constraints.iter().find(|pc| match pc.kind {
                            ConstraintKind::Exclusion { .. } if !pc.without_overlaps() => false,
                            ConstraintKind::PrimaryKey { index }
                            | ConstraintKind::Unique { index }
                            | ConstraintKind::Exclusion { index, .. } => {
                                cat.indexes.get(&index).is_some_and(|ix| {
                                    let cols: Vec<usize> = ix
                                        .columns
                                        .iter()
                                        .filter_map(|c| {
                                            if let IndexKey::Column(i) = c.key {
                                                Some(i)
                                            } else {
                                                None
                                            }
                                        })
                                        .collect();
                                    &cols == ref_columns
                                })
                            }
                            _ => false,
                        })
                    });
                    let rule = |a: &FkAction| match a {
                        FkAction::NoAction => "NO ACTION",
                        FkAction::Restrict => "RESTRICT",
                        FkAction::Cascade => "CASCADE",
                        FkAction::SetNull => "SET NULL",
                        FkAction::SetDefault => "SET DEFAULT",
                    };
                    out.push(vec![
                        nm(&dbname),
                        nm(cat.schema_name(t.schema)),
                        nm(&con.name),
                        if uniq.is_some() { nm(&dbname) } else { N },
                        parent
                            .filter(|_| uniq.is_some())
                            .map(|p| nm(cat.schema_name(p.schema)))
                            .unwrap_or(N),
                        uniq.map(|u| nm(&u.name)).unwrap_or(N),
                        nm(if *match_full { "FULL" } else { "NONE" }),
                        nm(rule(on_update)),
                        nm(rule(on_delete)),
                    ]);
                }
            }
        }
        IsConstraintColumnUsage => {
            for t in cat.tables.values() {
                for con in &t.constraints {
                    let (tab, cols): (&Table, Vec<usize>) = match &con.kind {
                        ConstraintKind::Exclusion { index, .. } if con.without_overlaps() => {
                            let Some(ix) = cat.indexes.get(index) else {
                                continue;
                            };
                            (
                                t,
                                ix.columns
                                    .iter()
                                    .filter_map(|c| match c.key {
                                        IndexKey::Column(i) => Some(i),
                                        _ => None,
                                    })
                                    .collect(),
                            )
                        }
                        ConstraintKind::PrimaryKey { index } | ConstraintKind::Unique { index } => {
                            let Some(ix) = cat.indexes.get(index) else {
                                continue;
                            };
                            (
                                t,
                                ix.columns
                                    .iter()
                                    .filter_map(|c| {
                                        if let IndexKey::Column(i) = c.key {
                                            Some(i)
                                        } else {
                                            None
                                        }
                                    })
                                    .collect(),
                            )
                        }
                        ConstraintKind::ForeignKey {
                            ref_table,
                            ref_columns,
                            ..
                        } => {
                            let Some(p) = cat.tables.get(ref_table) else {
                                continue;
                            };
                            (p, ref_columns.clone())
                        }
                        ConstraintKind::Check { expr, .. } => (
                            t,
                            t.live_columns()
                                .filter(|(_, c)| expr_mentions(expr, &c.name))
                                .map(|(i, _)| i)
                                .collect(),
                        ),
                        ConstraintKind::NotNull { column } => (t, vec![*column]),
                        ConstraintKind::Exclusion { .. } => continue,
                    };
                    let mut cols = cols;
                    cols.sort();
                    cols.dedup();
                    for col in cols {
                        out.push(vec![
                            nm(&dbname),
                            nm(cat.schema_name(tab.schema)),
                            nm(&tab.name),
                            nm(&tab.columns[col].name),
                            nm(&dbname),
                            nm(cat.schema_name(t.schema)),
                            nm(&con.name),
                        ]);
                    }
                }
            }
        }
        IsCheckConstraints => {
            for t in cat.tables.values() {
                for con in &t.constraints {
                    let clause = match &con.kind {
                        // pg_get_expr(conbin, conrelid).
                        ConstraintKind::Check { expr, .. } => {
                            stored_expr(ctx, t, expr, Stored::Condition, false)
                                .unwrap_or_else(|| expr.clone())
                        }
                        ConstraintKind::NotNull { column } => {
                            format!("{} IS NOT NULL", quote_ident(&t.columns[*column].name))
                        }
                        _ => continue,
                    };
                    out.push(vec![
                        nm(&dbname),
                        nm(cat.schema_name(t.schema)),
                        nm(&con.name),
                        nm(clause),
                    ]);
                }
            }
            for d in cat.domains.values() {
                // (pg_get_constraintdef's expression; the NOT NULL `VALUE IS NOT NULL`.)
                if d.not_null {
                    out.push(vec![
                        nm(&dbname),
                        nm(cat.schema_name(d.schema)),
                        nm(d.not_null_name
                            .clone()
                            .unwrap_or_else(|| format!("{}_not_null", d.name))),
                        nm("VALUE IS NOT NULL"),
                    ]);
                }
                for (name, sql) in &d.checks {
                    let expr = crate::sql::ruleutils::domain_check(ctx, &d.base, sql, false)
                        .unwrap_or_else(|| sql.clone());
                    out.push(vec![
                        nm(&dbname),
                        nm(cat.schema_name(d.schema)),
                        nm(name),
                        nm(expr),
                    ]);
                }
            }
        }
        IsViews => {
            use super::trigger_type as tt;
            for v in cat.views.values().filter(|v| !v.materialized) {
                let instead = |ev: u16| {
                    cat.triggers.values().any(|x| {
                        x.table == v.oid && x.tgtype & tt::INSTEAD != 0 && x.tgtype & ev != 0
                    })
                };
                let (mask, _) = crate::sql::binder::views::view_updatability(ctx, v, false);
                out.push(vec![
                    nm(&dbname),
                    nm(cat.schema_name(v.schema)),
                    nm(&v.name),
                    nm(crate::sql::scs::session_literals(
                        &crate::sql::ruleutils::view_def(ctx, v, false)
                            .unwrap_or_else(|| format!(" {};", v.query)),
                    )),
                    nm(v.check_option
                        .as_deref()
                        .map_or_else(|| "NONE".to_string(), str::to_uppercase)),
                    yn(mask & 20 == 20),
                    yn(mask & 8 == 8),
                    yn(instead(tt::UPDATE)),
                    yn(instead(tt::DELETE)),
                    yn(instead(tt::INSERT)),
                ]);
            }
        }
        IsTriggers => {
            use super::trigger_type as tt;
            let me = ctx.current_user();
            let dbo = ctx.db_owner();
            let mut trigs: Vec<_> = cat.triggers.values().cloned().collect();
            trigs.sort_by(|a, b| (a.table, &a.name).cmp(&(b.table, &b.name)));
            for x in &trigs {
                let Some(t) = crate::sql::func::system::trigger_relation(cat, x.table) else {
                    continue;
                };
                // Owners and holders of a non-SELECT privilege see the trigger.
                let seen = cat.has_privs_of_role(me, t.owner, dbo)
                    || cat.rel_mask(t.oid, me, dbo).is_some_and(|(p, _)| {
                        p & (acl::INSERT
                            | acl::UPDATE
                            | acl::DELETE
                            | acl::TRUNCATE
                            | acl::REFERENCES
                            | acl::TRIGGER)
                            != 0
                    });
                if !seen {
                    continue;
                }
                let cond = x.when.as_ref().map(|w| {
                    crate::sql::ruleutils::trigger_when(ctx, &t, w, false)
                        .unwrap_or_else(|| w.clone())
                });
                let action = crate::sql::func::system::trigger_action(ctx, x);
                let timing = if x.tgtype & tt::BEFORE != 0 {
                    "BEFORE"
                } else if x.tgtype & tt::INSTEAD != 0 {
                    "INSTEAD OF"
                } else {
                    "AFTER"
                };
                for (bit, word) in [
                    (tt::INSERT, "INSERT"),
                    (tt::DELETE, "DELETE"),
                    (tt::UPDATE, "UPDATE"),
                ] {
                    if x.tgtype & bit == 0 {
                        continue;
                    }
                    // rank() over (table, event, row/statement, timing) by name.
                    let order = 1 + trigs
                        .iter()
                        .filter(|y| {
                            y.table == x.table
                                && y.tgtype & bit != 0
                                && y.tgtype & tt::ROW == x.tgtype & tt::ROW
                                && y.tgtype & 66 == x.tgtype & 66
                                && y.name < x.name
                        })
                        .count();
                    out.push(vec![
                        nm(&dbname),
                        nm(cat.schema_name(t.schema)),
                        nm(&x.name),
                        nm(word),
                        nm(&dbname),
                        nm(cat.schema_name(t.schema)),
                        nm(&t.name),
                        Value::Int4(order as i32),
                        cond.as_deref().map_or(N, nm),
                        nm(&action),
                        nm(if x.tgtype & tt::ROW != 0 {
                            "ROW"
                        } else {
                            "STATEMENT"
                        }),
                        nm(timing),
                        x.old_table.as_deref().map_or(N, nm),
                        x.new_table.as_deref().map_or(N, nm),
                        N,
                        N,
                        N,
                    ]);
                }
            }
        }
        IsSequences => {
            for s in cat.sequences.values() {
                if cat.tables.values().any(|t| t.rowid_seq == Some(s.oid)) {
                    continue;
                }
                // An identity column's sequence depends on it internally: not shown.
                if s.owned_by.is_some_and(|(t, c)| {
                    cat.tables.get(&t).and_then(|t| t.columns.get(c)).is_some_and(|c| {
                        c.identity.is_some() && c.seq == Some(s.oid)
                    })
                }) {
                    continue;
                }
                let prec = match s.ty {
                    DataType::Int2 => 16,
                    DataType::Int4 => 32,
                    _ => 64,
                };
                out.push(vec![
                    nm(&dbname),
                    nm(cat.schema_name(s.schema)),
                    nm(&s.name),
                    nm(s.ty.sql_name()),
                    i4(prec),
                    i4(2),
                    i4(0),
                    nm(s.start.to_string()),
                    nm(s.min.to_string()),
                    nm(s.max.to_string()),
                    nm(s.increment.to_string()),
                    yn(s.cycle),
                ]);
            }
        }
        IsEnabledRoles
        | IsTablePrivileges
        | IsApplicableRoles
        | IsAdministrableRoleAuthorizations
        | IsRoleTableGrants
        | IsColumnPrivileges
        | IsRoleColumnGrants
        | IsRoutinePrivileges
        | IsRoleRoutineGrants
        | IsUsagePrivileges
        | IsRoleUsageGrants
        | IsUdtPrivileges
        | IsRoleUdtGrants => {
            out = privilege_rows(table, ctx, cat);
        }
        IsCharacterSets => {
            out.push(vec![
                N,
                N,
                nm("UTF8"),
                nm("UCS"),
                nm("UTF8"),
                nm(&dbname),
                nm("pg_catalog"),
                nm("C"),
            ]);
        }
        // Views computed from their definitions (above), and catalogs that stay empty.
        _ => {}
    }
    Ok(out)
}

/// The information_schema views over ACLs (aclexplode joined with roles).
fn privilege_rows(table: SysTable, ctx: &ExecCtx, cat: &Catalog) -> Vec<Row> {
    use SysTable::*;
    let me = ctx.current_user();
    let dbo = ctx.db_owner();
    let dbname = ctx.sess.database.as_str();
    let name = |oid: u32| {
        if oid == 0 {
            "PUBLIC".to_string()
        } else {
            cat.roles
                .get(&oid)
                .map(|r| r.name.clone())
                .unwrap_or_default()
        }
    };
    // pg_has_role(r, 'USAGE') for the current user.
    let usage = |r: u32| r != 0 && cat.has_privs_of_role(me, r, dbo);
    let enabled = |r: u32| usage(r);
    // aclexplode, filtered to the privileges asked for and to rows the user may see.
    let explode = |a: &acl::Acl, owner: u32, want: u32| -> Vec<(u32, u32, u32, &'static str)> {
        let mut out = Vec::new();
        for item in a {
            for &(bit, _) in acl::NAMES {
                if item.privs & bit == 0 || want & bit == 0 {
                    continue;
                }
                if !(usage(item.grantor) || usage(item.grantee) || item.grantee == 0) {
                    continue;
                }
                let grantable = item.goptions & bit != 0
                    || (item.grantee != 0 && cat.has_privs_of_role(item.grantee, owner, dbo));
                out.push((
                    item.grantor,
                    item.grantee,
                    bit,
                    if grantable { "YES" } else { "NO" },
                ));
            }
        }
        out
    };
    let role_grants = matches!(
        table,
        IsRoleTableGrants
            | IsRoleColumnGrants
            | IsRoleRoutineGrants
            | IsRoleUsageGrants
            | IsRoleUdtGrants
    );
    let keep = |grantor: u32, grantee: u32| !role_grants || enabled(grantor) || enabled(grantee);
    let word = |bit: u32| {
        acl::NAMES
            .iter()
            .find(|(b, _)| *b == bit)
            .map_or("", |(_, w)| *w)
    };
    let mut out: Vec<Row> = Vec::new();
    // User and system relations of kinds r, v, f, p: (oid, schema, name, owner).
    let rels = || {
        let mut v: Vec<(u32, u32, String, u32)> = Vec::new();
        for t in SysTable::all() {
            v.push((t.oid(), t.schema(), t.name().to_string(), OWNER));
        }
        v.extend(
            cat.tables
                .values()
                .map(|t| (t.oid, t.schema, t.name.clone(), t.owner)),
        );
        v.extend(
            cat.views
                .values()
                .filter(|x| !x.materialized)
                .map(|x| (x.oid, x.schema, x.name.clone(), x.owner)),
        );
        v
    };
    match table {
        IsEnabledRoles => {
            for r in cat.roles.values().filter(|r| enabled(r.oid)) {
                out.push(vec![nm(&r.name)]);
            }
            if enabled(super::PG_DATABASE_OWNER_OID)
                && !cat.roles.contains_key(&super::PG_DATABASE_OWNER_OID)
            {
                out.push(vec![nm("pg_database_owner")]);
            }
        }
        IsApplicableRoles | IsAdministrableRoleAuthorizations => {
            let mut m: Vec<(u32, u32, bool)> = cat
                .members
                .iter()
                .map(|m| (m.member, m.role, m.admin))
                .collect();
            if !m
                .iter()
                .any(|x| x.0 == dbo && x.1 == super::PG_DATABASE_OWNER_OID && !x.2)
            {
                m.push((dbo, super::PG_DATABASE_OWNER_OID, false));
            }
            m.sort();
            m.dedup();
            for (member, role, admin) in m {
                if !usage(member) || (table == IsAdministrableRoleAuthorizations && !admin) {
                    continue;
                }
                out.push(vec![nm(name(member)), nm(name(role)), yn(admin)]);
            }
        }
        IsTablePrivileges | IsRoleTableGrants => {
            let want = acl::INSERT
                | acl::SELECT
                | acl::UPDATE
                | acl::DELETE
                | acl::TRUNCATE
                | acl::REFERENCES
                | acl::TRIGGER;
            for (oid, schema, rel, owner) in rels() {
                for (gr, ge, bit, g) in
                    explode(&cat.rel_acl(oid, owner, ObjKind::Table), owner, want)
                {
                    if keep(gr, ge) {
                        out.push(vec![
                            nm(name(gr)),
                            nm(name(ge)),
                            nm(dbname),
                            nm(cat.schema_name(schema)),
                            nm(&rel),
                            nm(word(bit)),
                            nm(g),
                            yn(bit == acl::SELECT),
                        ]);
                    }
                }
            }
        }
        IsColumnPrivileges | IsRoleColumnGrants => {
            let want = acl::INSERT | acl::SELECT | acl::UPDATE | acl::REFERENCES;
            let mut seen = std::collections::HashSet::new();
            for (oid, schema, rel, owner) in rels() {
                let cols: Vec<String> = match (
                    cat.tables.get(&oid),
                    cat.views.get(&oid),
                    SysTable::by_oid(oid),
                ) {
                    (Some(t), _, _) => t
                        .columns
                        .iter()
                        .filter(|c| !c.dropped)
                        .map(|c| c.name.clone())
                        .collect(),
                    (_, Some(v), _) => v.columns.iter().map(|c| c.0.clone()).collect(),
                    (_, _, Some(st)) => st.columns().iter().map(|c| c.0.to_string()).collect(),
                    _ => Vec::new(),
                };
                let table_rows = explode(&cat.rel_acl(oid, owner, ObjKind::Table), owner, want);
                let all_cols = cat.tables.get(&oid).map(|t| {
                    t.columns
                        .iter()
                        .enumerate()
                        .filter(|c| !c.1.dropped)
                        .map(|(i, c)| (i as i32 + 1, c.name.clone()))
                        .collect::<Vec<_>>()
                });
                for c in &cols {
                    for &(gr, ge, bit, g) in &table_rows {
                        if keep(gr, ge) && seen.insert((gr, ge, oid, c.clone(), bit, g)) {
                            out.push(vec![
                                nm(name(gr)),
                                nm(name(ge)),
                                nm(dbname),
                                nm(cat.schema_name(schema)),
                                nm(&rel),
                                nm(c),
                                nm(word(bit)),
                                nm(g),
                            ]);
                        }
                    }
                }
                for (attnum, c) in all_cols.unwrap_or_default() {
                    let Some(a) = cat.acl(ObjKind::Table, oid, attnum) else {
                        continue;
                    };
                    for (gr, ge, bit, g) in explode(a, owner, want) {
                        if keep(gr, ge) && seen.insert((gr, ge, oid, c.clone(), bit, g)) {
                            out.push(vec![
                                nm(name(gr)),
                                nm(name(ge)),
                                nm(dbname),
                                nm(cat.schema_name(schema)),
                                nm(&rel),
                                nm(&c),
                                nm(word(bit)),
                                nm(g),
                            ]);
                        }
                    }
                }
            }
        }
        IsRoutinePrivileges | IsRoleRoutineGrants => {
            for f in cat.functions.values() {
                for (gr, ge, bit, g) in explode(
                    &cat.effective_acl(ObjKind::Function, f.oid, f.owner),
                    f.owner,
                    acl::EXECUTE,
                ) {
                    if keep(gr, ge) {
                        let sch = cat.schema_name(f.schema);
                        out.push(vec![
                            nm(name(gr)),
                            nm(name(ge)),
                            nm(dbname),
                            nm(sch),
                            nm(format!("{}_{}", f.name, f.oid)),
                            nm(dbname),
                            nm(sch),
                            nm(&f.name),
                            nm(word(bit)),
                            nm(g),
                        ]);
                    }
                }
            }
        }
        IsUsagePrivileges | IsRoleUsageGrants => {
            if !role_grants {
                for coll in ["default", "C", "POSIX", "ucs_basic", "unicode", "pg_c_utf8"] {
                    out.push(vec![
                        nm(name(OWNER)),
                        nm("PUBLIC"),
                        nm(dbname),
                        nm("pg_catalog"),
                        nm(coll),
                        nm("COLLATION"),
                        nm("USAGE"),
                        nm("NO"),
                    ]);
                }
            }
            for d in cat.domains.values() {
                for (gr, ge, _, g) in explode(
                    &cat.effective_acl(ObjKind::Type, d.oid, d.owner),
                    d.owner,
                    acl::USAGE,
                ) {
                    if keep(gr, ge) {
                        out.push(vec![
                            nm(name(gr)),
                            nm(name(ge)),
                            nm(dbname),
                            nm(cat.schema_name(d.schema)),
                            nm(&d.name),
                            nm("DOMAIN"),
                            nm("USAGE"),
                            nm(g),
                        ]);
                    }
                }
            }
            // Foreign-data wrappers and servers.
            let fdws = cat
                .fdws
                .values()
                .map(|f| (ObjKind::ForeignDataWrapper, f.oid, f.owner, &f.name, "FOREIGN DATA WRAPPER"));
            let servers = cat
                .servers
                .values()
                .map(|x| (ObjKind::ForeignServer, x.oid, x.owner, &x.name, "FOREIGN SERVER"));
            for (kind, oid, owner, oname, otype) in fdws.chain(servers) {
                for (gr, ge, _, g) in
                    explode(&cat.effective_acl(kind, oid, owner), owner, acl::USAGE)
                {
                    if keep(gr, ge) {
                        out.push(vec![
                            nm(name(gr)),
                            nm(name(ge)),
                            nm(dbname),
                            nm(""),
                            nm(oname),
                            nm(otype),
                            nm("USAGE"),
                            nm(g),
                        ]);
                    }
                }
            }
            for q in cat.sequences.values() {
                for (gr, ge, _, g) in explode(
                    &cat.rel_acl(q.oid, q.owner, ObjKind::Table),
                    q.owner,
                    acl::USAGE,
                ) {
                    if keep(gr, ge) {
                        out.push(vec![
                            nm(name(gr)),
                            nm(name(ge)),
                            nm(dbname),
                            nm(cat.schema_name(q.schema)),
                            nm(&q.name),
                            nm("SEQUENCE"),
                            nm("USAGE"),
                            nm(g),
                        ]);
                    }
                }
            }
        }
        IsUdtPrivileges | IsRoleUdtGrants => {
            let mut types: Vec<(u32, u32, String, u32)> = cat
                .composites
                .values()
                .map(|c| (c.oid, c.schema, c.name.clone(), c.owner))
                .collect();
            types.extend(
                cat.tables
                    .values()
                    .map(|t| (t.rowtype, t.schema, t.name.clone(), t.owner)),
            );
            types.extend(
                cat.views
                    .values()
                    .map(|v| (v.rowtype, v.schema, v.name.clone(), v.owner)),
            );
            for (oid, schema, tname, owner) in types {
                for (gr, ge, _, g) in explode(
                    &cat.effective_acl(ObjKind::Type, oid, owner),
                    owner,
                    acl::USAGE,
                ) {
                    if keep(gr, ge) {
                        out.push(vec![
                            nm(name(gr)),
                            nm(name(ge)),
                            nm(dbname),
                            nm(cat.schema_name(schema)),
                            nm(&tname),
                            nm("TYPE USAGE"),
                            nm(g),
                        ]);
                    }
                }
            }
        }
        _ => {}
    }
    out
}

fn tz_offset_abbrev(secs: i32) -> String {
    let sign = if secs < 0 { '-' } else { '+' };
    let a = secs.abs();
    if a % 3600 == 0 {
        format!("{sign}{:02}", a / 3600)
    } else {
        format!("{sign}{:02}{:02}", a / 3600, a % 3600 / 60)
    }
}

pub(crate) fn align(t: &DataType) -> char {
    use DataType as T;
    match t {
        // Composite types (and the polymorphic types that may stand for anything) align as
        // strictly as anything can.
        T::Row(_)
        | T::Record
        | T::Aclitem
        | T::AnyArray
        | T::AnyCompatibleArray
        | T::AnyRange
        | T::AnyMultirange
        | T::AnyCompatibleRange
        | T::AnyCompatibleMultirange => 'd',
        T::Name | T::Cstring | T::Unknown | T::Uuid => 'c',
        T::Tid => 's',
        T::Point
        | T::Lseg
        | T::Path
        | T::GBox
        | T::Polygon
        | T::Line
        | T::Circle
        | T::Interval(_)
        | T::Timetz(_) => 'd',
        T::Macaddr8 => 'i',
        T::Base(b) => b.align,
        T::Range(r) | T::Multirange(r) => {
            if align(&r.subtype) == 'd' {
                'd'
            } else {
                'i'
            }
        }
        // An array aligns as its elements, or at least as an int.
        T::Array(e) if align(e) == 'd' => 'd',
        _ => match t.typlen() {
            1 => 'c',
            2 => 's',
            8 => 'd',
            _ => 'i',
        },
    }
}

fn expr_mentions(expr: &str, col: &str) -> bool {
    let bytes = expr.as_bytes();
    let mut i = 0;
    while let Some(p) = expr[i..].find(col) {
        let s = i + p;
        let e = s + col.len();
        let before = s == 0 || !(bytes[s - 1].is_ascii_alphanumeric() || bytes[s - 1] == b'_');
        let after = e >= bytes.len() || !(bytes[e].is_ascii_alphanumeric() || bytes[e] == b'_');
        if before && after {
            return true;
        }
        i = e;
    }
    false
}

fn constraint_row(ctx: &ExecCtx, cat: &Catalog, t: &Arc<Table>, con: &super::Constraint) -> Row {
    let action = |a: &FkAction| c(a.code());
    let (deferrable, deferred) = con.deferral();
    let mut exclop = N;
    let (keys, fkeys, frel, upd, del, mtype, indid, bin) = match &con.kind {
        ConstraintKind::Exclusion { index, ops } => {
            let k = cat
                .indexes
                .get(index)
                .map(|i| index_cols(i))
                .unwrap_or_default();
            if let Some(ix) = cat.indexes.get(index) {
                exclop = arr(
                    DataType::Oid,
                    exclusion_op_oids(ctx, t, ix, ops)
                        .into_iter()
                        .map(o)
                        .collect(),
                );
            }
            let k: Vec<i64> = match cat.indexes.get(index) {
                Some(ix) => k.into_iter().take(ix.columns.len()).collect(),
                None => k,
            };
            (Some(k), None, 0, c(' '), c(' '), c(' '), *index, N)
        }
        ConstraintKind::PrimaryKey { index } | ConstraintKind::Unique { index } => {
            let k = cat
                .indexes
                .get(index)
                .map(|i| index_cols(i))
                .unwrap_or_default();
            let k: Vec<i64> = match cat.indexes.get(index) {
                Some(ix) => k.into_iter().take(ix.columns.len()).collect(),
                None => k,
            };
            (Some(k), None, 0, c(' '), c(' '), c(' '), *index, N)
        }
        ConstraintKind::Check { expr, .. } => {
            let cols: Vec<i64> = t
                .live_columns()
                .filter(|(_, col)| expr_mentions(expr, &col.name))
                .map(|(i, _)| i as i64 + 1)
                .collect();
            let shown =
                stored_expr(ctx, t, expr, Stored::Condition, false).unwrap_or_else(|| expr.clone());
            (Some(cols), None, 0, c(' '), c(' '), c(' '), 0, nm(shown))
        }
        ConstraintKind::NotNull { column } => (
            Some(vec![*column as i64 + 1]),
            None,
            0,
            c(' '),
            c(' '),
            c(' '),
            0,
            N,
        ),
        ConstraintKind::ForeignKey {
            columns,
            ref_table,
            ref_columns,
            on_delete,
            on_update,
            match_full,
            ..
        } => {
            let indid = super::depend::fk_index(cat, *ref_table, ref_columns).unwrap_or(0);
            (
                Some(columns.iter().map(|c| *c as i64 + 1).collect()),
                Some(
                    ref_columns
                        .iter()
                        .map(|c| *c as i64 + 1)
                        .collect::<Vec<i64>>(),
                ),
                *ref_table,
                action(on_update),
                action(on_delete),
                c(if *match_full { 'f' } else { 's' }),
                indid,
                N,
            )
        }
    };
    let i2arr = |v: Option<Vec<i64>>| {
        v.map(|v| arr(DataType::Int2, v.into_iter().map(i2).collect()))
            .unwrap_or(N)
    };
    vec![
        o(con.oid),
        nm(&con.name),
        o(t.schema),
        c(con.contype()),
        b(deferrable),
        b(deferred),
        b(con.enforced),
        b(con.validated),
        o(t.oid),
        o(0),
        o(indid),
        o(con.parent),
        o(frel),
        upd,
        del,
        mtype,
        b(con.islocal),
        i2(con.inhcount as i64),
        // Check and not-null constraints say whether they're inherited; a partition's copy
        // of its parent's key or foreign key, and a partitioned table's foreign key, are
        // inheritable (the partitions get them).
        b(match con.kind {
            ConstraintKind::Check { no_inherit, .. } => no_inherit,
            ConstraintKind::NotNull { .. } => con.no_inherit,
            ConstraintKind::ForeignKey { .. } => !con.from_parent && t.partkey.is_none(),
            _ => !con.from_parent,
        }),
        b(con.is_period()),
        i2arr(keys),
        i2arr(fkeys),
        N,
        N,
        N,
        // confdelsetcols
        i2arr((!con.del_set_cols.is_empty())
            .then(|| con.del_set_cols.iter().map(|c| *c as i64 + 1).collect())),
        exclop,
        bin,
    ]
}

/// The operators of an exclusion constraint over index `ix` (pg_operator oids).
pub fn exclusion_op_oids(
    ctx: &ExecCtx,
    t: &Arc<Table>,
    ix: &super::Index,
    ops: &[String],
) -> Vec<u32> {
    ix.columns
        .iter()
        .zip(ops)
        .map(|(c, op)| {
            let ty = match &c.key {
                IndexKey::Column(i) => Some(t.columns[*i].ty.clone()),
                IndexKey::Expr(e) => {
                    crate::sql::ruleutils::stored_expr_type(ctx, t, e, Stored::Plain)
                }
            };
            ty.and_then(|ty| crate::sql::func::resolve_binop(op, &ty, &ty).ok())
                .map_or(0, |r| crate::sql::func::oper_row(op, &r.def).oid)
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
/// pg_proc's rows of the built-in functions, as Postgres 18 has them (proacl left NULL).
pub(crate) fn builtin_procs() -> &'static [Row] {
    static ROWS: OnceLock<Vec<Row>> = OnceLock::new();
    ROWS.get_or_init(|| {
        include_str!("pg_proc.tsv")
            .lines()
            .map(builtin_proc)
            .collect()
    })
}

/// A built-in function's signature, as pg_proc has it.
pub struct ProcSig {
    pub oid: u32,
    pub name: &'static str,
    pub args: Vec<u32>,
    pub ret: u32,
    pub strict: bool,
    pub retset: bool,
    pub volatility: char,
    pub nargdefaults: usize,
    /// OUT / TABLE columns: (name, type).
    pub out: Vec<(String, u32)>,
}

/// Every built-in function's signature, in oid order.
pub fn builtin_proc_sigs() -> &'static [ProcSig] {
    static SIGS: OnceLock<Vec<ProcSig>> = OnceLock::new();
    SIGS.get_or_init(|| {
        include_str!("pg_proc.tsv")
            .lines()
            .map(|line| {
                let f: Vec<&'static str> = line.split('\t').collect();
                let num = |i: usize| f[i].parse::<u32>().unwrap_or(0);
                let list = |i: usize| {
                    if f[i] == "\\N" {
                        Vec::new()
                    } else {
                        array_items(f[i])
                    }
                };
                let types: Vec<u32> = list(19).iter().map(|x| x.parse().unwrap_or(0)).collect();
                let modes = list(20);
                let names = list(21);
                let out = types
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| {
                        matches!(modes.get(*i).map(String::as_str), Some("o" | "t" | "b"))
                    })
                    .map(|(i, t)| (names.get(i).cloned().unwrap_or_default(), *t))
                    .collect();
                ProcSig {
                    oid: num(0),
                    name: f[1],
                    args: f[18]
                        .split_whitespace()
                        .map(|x| x.parse().unwrap_or(0))
                        .collect(),
                    ret: num(17),
                    strict: f[11] == "t",
                    retset: f[12] == "t",
                    volatility: f[13].chars().next().unwrap_or('v'),
                    nargdefaults: num(16) as usize,
                    out,
                }
            })
            .collect()
    })
}

/// The built-in function whose C symbol (prosrc of an internal-language function) is
/// `prosrc`: its pg_proc oid (`fmgr_internal_function`, which takes the lowest).
pub fn builtin_by_prosrc(prosrc: &str) -> Option<u32> {
    static MAP: OnceLock<std::collections::HashMap<&'static str, u32>> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut m = std::collections::HashMap::new();
        for line in include_str!("pg_proc.tsv").lines() {
            let f: Vec<&'static str> = line.split('\t').collect();
            if f.get(3) == Some(&"12")
                && let (Some(src), Ok(oid)) = (f.get(24), f[0].parse::<u32>())
            {
                m.entry(*src).or_insert(oid);
            }
        }
        m
    })
    .get(prosrc)
    .copied()
}

/// Whether an internal-language function may name `prosrc` as its C symbol.
pub fn builtin_prosrc_exists(prosrc: &str) -> bool {
    builtin_by_prosrc(prosrc).is_some()
}

/// One line of `pg_proc.tsv` (COPY text format, without proowner and proacl).
fn builtin_proc(line: &str) -> Row {
    let f: Vec<Option<String>> = line.split('\t').map(copy_field).collect();
    let s = |i: usize| f.get(i).cloned().flatten();
    let num = |i: usize| s(i).and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0);
    let oid = |i: usize| o(num(i) as u32);
    let ch = |i: usize| c(s(i).and_then(|x| x.chars().next()).unwrap_or(' '));
    let bl = |i: usize| b(s(i).as_deref() == Some("t"));
    let text = |i: usize| s(i).map_or(N, nm);
    let list = |i: usize, elem: DataType| {
        s(i).map_or(N, |x| {
            let items = array_items(&x).into_iter().map(|e| match elem {
                DataType::Oid => o(e.parse().unwrap_or(0)),
                DataType::Char => c(e.chars().next().unwrap_or(' ')),
                _ => Value::Text(Arc::from(e.as_str())),
            });
            arr(elem.clone(), items.collect())
        })
    };
    let argtypes = s(18)
        .unwrap_or_default()
        .split_whitespace()
        .map(|x| o(x.parse().unwrap_or(0)))
        .collect();
    vec![
        oid(0),
        text(1),
        oid(2),
        o(OWNER),
        oid(3),
        Value::Float4(num(4) as f32),
        Value::Float4(num(5) as f32),
        oid(6),
        oid(7),
        ch(8),
        bl(9),
        bl(10),
        bl(11),
        bl(12),
        ch(13),
        ch(14),
        i2(num(15) as i64),
        i2(num(16) as i64),
        oid(17),
        vector(DataType::Oid, argtypes),
        list(19, DataType::Oid),
        list(20, DataType::Char),
        list(21, DataType::Text),
        text(22),
        list(23, DataType::Oid),
        text(24),
        text(25),
        text(26),
        list(27, DataType::Text),
        N,
    ]
}

/// A COPY text-format field: None for `\N`, else unescaped.
pub(crate) fn copy_field(x: &str) -> Option<String> {
    if x == "\\N" {
        return None;
    }
    let mut out = String::with_capacity(x.len());
    let mut it = x.chars();
    while let Some(ch) = it.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match it.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('v') => out.push('\u{b}'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    Some(out)
}

/// The elements of a one-dimensional array literal (`{a,"b c",""}`).
fn array_items(x: &str) -> Vec<String> {
    let body = x.trim().trim_start_matches('{').trim_end_matches('}');
    let mut items = Vec::new();
    if body.is_empty() {
        return items;
    }
    let (mut cur, mut quoted, mut it) = (String::new(), false, body.chars());
    while let Some(ch) = it.next() {
        match ch {
            '"' => quoted = !quoted,
            '\\' => cur.extend(it.next()),
            ',' if !quoted => items.push(std::mem::take(&mut cur)),
            _ => cur.push(ch),
        }
    }
    items.push(cur);
    items
}

fn proc_row(
    oid: u32,
    name: &str,
    ns: u32,
    kind: char,
    strict: bool,
    retset: bool,
    volatility: char,
    args: &[DataType],
    ndefaults: usize,
    ret: u32,
    variadic: bool,
    src: &str,
    _sym: &str,
) -> Row {
    vec![
        o(oid),
        nm(name),
        o(ns),
        o(OWNER),
        o(12),
        Value::Float4(1.0),
        Value::Float4(if retset { 1000.0 } else { 0.0 }),
        o(match args.last() {
            _ if !variadic => 0,
            Some(DataType::AnyArray) => crate::types::oid::ANYELEMENT,
            Some(DataType::AnyCompatibleArray) => DataType::AnyCompatible.oid(),
            a => a
                .and_then(|a| a.elem())
                .map(|e| e.oid())
                .unwrap_or(crate::types::oid::ANY),
        }),
        o(0),
        c(kind),
        b(false),
        b(false),
        b(strict),
        b(retset),
        c(volatility),
        c('s'),
        i2(args.len() as i64),
        i2(ndefaults as i64),
        o(ret),
        vector(DataType::Oid, args.iter().map(|a| o(a.oid())).collect()),
        // A VARIADIC parameter is only told apart by its mode.
        if variadic {
            arr(DataType::Oid, args.iter().map(|a| o(a.oid())).collect())
        } else {
            N
        },
        if variadic {
            arr(
                DataType::Char,
                (0..args.len())
                    .map(|i| c(if i + 1 == args.len() { 'v' } else { 'i' }))
                    .collect(),
            )
        } else {
            N
        },
        N,
        N,
        N,
        nm(src),
        N,
        N,
        N,
        N,
    ]
}

/// information_schema `data_type`.
fn info_data_type(t: &DataType) -> String {
    match t {
        DataType::Array(_) => "ARRAY".into(),
        DataType::Enum(..) | DataType::Row(_) => "USER-DEFINED".into(),
        t => t.without_typmod().sql_name(),
    }
}

/// (character_maximum_length, numeric_precision, numeric_scale)
fn type_lengths(t: &DataType) -> (Value, Value, Value) {
    match t {
        DataType::Varchar(m) | DataType::Bpchar(m) if *m >= 4 => (i4(*m as i64 - 4), N, N),
        DataType::Bpchar(_) => (N, N, N),
        DataType::Int2 => (N, i4(16), i4(0)),
        DataType::Int4 => (N, i4(32), i4(0)),
        DataType::Int8 => (N, i4(64), i4(0)),
        DataType::Float4 => (N, i4(24), N),
        DataType::Float8 => (N, i4(53), N),
        DataType::Numeric(m) if *m >= 4 => {
            let (p, s) = crate::types::numeric_typmod_parts(*m);
            (N, i4(p as i64), i4(s as i64))
        }
        _ => (N, N, N),
    }
}

/// The comments of extensions created here: (objoid, classoid, text).
pub fn extension_descriptions(cat: &Catalog) -> Vec<(u32, u32, &'static str)> {
    let comment = |name: &str| {
        crate::session::ddl::EXTENSIONS
            .iter()
            .find(|e| e.0 == name)
            .map(|e| e.2)
            .unwrap_or("")
    };
    // (Those their scripts made have their control file's comment in pg_description.)
    cat.extensions
        .iter()
        .filter(|(oid, ..)| !cat.extension_info.contains_key(oid))
        .map(|(oid, name, ..)| (*oid, 3079, comment(name)))
        .collect()
}

/// Whether a built-in object Postgres ships a comment for is still there (only schemas can go).
fn builtin_object_exists(cat: &Catalog, oid: u32, class: u32) -> bool {
    class != 2615 || cat.schemas.contains_key(&oid)
}

/// The comment Postgres ships for built-in object `oid` (of catalog `class`), unless changed or
/// removed here.
pub fn builtin_description(
    cat: &Catalog,
    oid: u32,
    class: Option<u32>,
    sub: i32,
) -> Option<String> {
    if cat.comments.iter().any(|(o, s, _)| *o == oid && *s == sub) {
        return None;
    }
    let found = super::static_rows::rows(SysTable::PgDescription)
        .iter()
        .find(|r| {
            super::static_rows::oid_at(r, 0) == oid
                && r[2] == Value::Int4(sub)
                && class.is_none_or(|c| super::static_rows::oid_at(r, 1) == c)
        })
        .filter(|r| builtin_object_exists(cat, oid, super::static_rows::oid_at(r, 1)))
        .map(|r| super::static_rows::text_at(r, 3).to_string());
    found.or_else(|| {
        if sub == 0 {
            extension_descriptions(cat)
                .into_iter()
                .find(|(o, k, _)| *o == oid && class.is_none_or(|c| c == *k))
                .map(|(_, _, t)| t.to_string())
        } else {
            None
        }
    })
}

/// The catalog a built-in object (oid below 16384) is in.
pub fn builtin_class(oid: u32) -> Option<u32> {
    if oid >= super::FIRST_USER_OID {
        return None;
    }
    use super::static_rows::{oid_at, rows};
    if builtin_procs().iter().any(|r| r[0] == o(oid)) {
        return Some(SysTable::PgProc.oid());
    }
    [
        SysTable::PgClass,
        SysTable::PgType,
        SysTable::PgOperator,
        SysTable::PgNamespace,
        SysTable::PgCollation,
        SysTable::PgConversion,
        SysTable::PgAm,
        SysTable::PgOpclass,
        SysTable::PgOpfamily,
        SysTable::PgCast,
        SysTable::PgConstraint,
        SysTable::PgRewrite,
    ]
    .into_iter()
    .find(|t| rows(*t).iter().any(|r| oid_at(r, 0) == oid))
    .map(|t| t.oid())
    .or_else(|| {
        rows(SysTable::PgDescription)
            .iter()
            .find(|r| oid_at(r, 0) == oid)
            .map(|r| oid_at(r, 1))
    })
}

/// A built-in type's (schema, name, whether it is that type's array) from Postgres's pg_type,
/// for the types the engine has no `DataType` of its own for (system relations' row types).
pub fn builtin_type_name(oid: u32) -> Option<(u32, String, bool)> {
    static NAMES: OnceLock<std::collections::HashMap<u32, (u32, String, u32)>> = OnceLock::new();
    use super::static_rows::{oid_at, rows, text_at};
    let names = NAMES.get_or_init(|| {
        // An array type: typelem set, typlen -1 and array subscripting.
        let array =
            |r: &[Value]| oid_at(r, 13) != 0 && r[4] == Value::Int2(-1) && oid_at(r, 12) == 6179;
        rows(SysTable::PgType)
            .iter()
            .map(|r| {
                (
                    oid_at(r, 0),
                    (
                        oid_at(r, 2),
                        text_at(r, 1).to_string(),
                        if array(r) { oid_at(r, 13) } else { 0 },
                    ),
                )
            })
            .collect()
    });
    let (schema, name, elem) = names.get(&oid)?;
    if *elem != 0 {
        let (schema, name, _) = names.get(elem)?;
        return Some((*schema, name.clone(), true));
    }
    Some((*schema, name.clone(), false))
}

/// `format_type` of such a built-in type: pg_catalog's unqualified, information_schema's
/// qualified unless that schema is on the path.
pub fn builtin_type_display(oid: u32, search_path: &[String]) -> Option<String> {
    let (schema, name, array) = builtin_type_name(oid)?;
    let n = if schema == INF && !search_path.iter().any(|s| s == "information_schema") {
        format!("information_schema.{}", quote_ident(&name))
    } else {
        quote_ident(&name)
    };
    Some(if array { format!("{n}[]") } else { n })
}

/// The databases initdb makes, by oid.
fn builtin_database(oid: u32) -> Option<&'static str> {
    [(1, "template1"), (4, "template0"), (5, "postgres")]
        .into_iter()
        .find(|d| d.0 == oid)
        .map(|d| d.1)
}

/// The comment Postgres ships for a built-in shared object (a database), unless changed here.
pub fn builtin_shdescription(
    cat: &Catalog,
    dbs: &[(u32, String, u32)],
    oid: u32,
    class: u32,
) -> Option<String> {
    if cat.comments.iter().any(|(o, _, _)| *o == oid)
        || !dbs
            .iter()
            .any(|d| d.0 == oid && builtin_database(oid) == Some(d.1.as_str()))
    {
        return None;
    }
    super::static_rows::rows(SysTable::PgShdescription)
        .iter()
        .find(|r| {
            super::static_rows::oid_at(r, 0) == oid && super::static_rows::oid_at(r, 1) == class
        })
        .map(|r| super::static_rows::text_at(r, 2).to_string())
}

/// A column default as `pg_get_expr(adbin, adrelid)` prints it.
fn shown_default(ctx: &ExecCtx, t: &Arc<Table>, d: &str, ty: &DataType) -> String {
    stored_expr(ctx, t, d, Stored::Default(ty), false).unwrap_or_else(|| d.to_string())
}

#[allow(clippy::too_many_arguments)]
fn info_column(
    cat: &Catalog,
    db: &str,
    schema: u32,
    table: &str,
    name: &str,
    pos: i64,
    ty: &DataType,
    col: Option<&super::Column>,
    is_table: bool,
    default: Option<String>,
    generated: Option<String>,
) -> Row {
    let (len, prec, scale) = type_lengths(ty);
    let octet = match ty {
        DataType::Varchar(m) | DataType::Bpchar(m) if *m >= 4 => i4((*m as i64 - 4) * 4),
        DataType::Text | DataType::Varchar(_) => i4(1073741824),
        _ => N,
    };
    let radix = match ty {
        DataType::Int2 | DataType::Int4 | DataType::Int8 | DataType::Float4 | DataType::Float8 => {
            i4(2)
        }
        DataType::Numeric(m) if *m >= 4 => i4(10),
        DataType::Numeric(_) => i4(10),
        _ => N,
    };
    let dt_prec = match ty {
        DataType::Date => i4(0),
        DataType::Time(m)
        | DataType::Timetz(m)
        | DataType::Timestamp(m)
        | DataType::Timestamptz(m) => i4(if *m >= 0 { *m as i64 } else { 6 }),
        DataType::Interval(m) => i4(if *m >= 0 && (*m & 0xffff) != 0xffff {
            (*m & 0xffff) as i64
        } else {
            6
        }),
        _ => N,
    };
    let (udt_schema, udt_name) = match ty {
        DataType::Enum(oid, n, _) => (
            cat.enums
                .get(oid)
                .map(|e| cat.schema_name(e.schema).to_string())
                .unwrap_or_default(),
            n.to_string(),
        ),
        DataType::Array(e) if matches!(**e, DataType::Enum(..)) => {
            let DataType::Enum(oid, n, _) = &**e else {
                unreachable!()
            };
            (
                cat.enums
                    .get(oid)
                    .map(|e| cat.schema_name(e.schema).to_string())
                    .unwrap_or_default(),
                format!("_{n}"),
            )
        }
        t => {
            let base = match t {
                DataType::Array(e) => &**e,
                t => t,
            };
            match base {
                DataType::Row(r) if r.oid != 0 => (
                    cat.composites
                        .get(&r.oid)
                        .map(|c| cat.schema_name(c.schema).to_string())
                        .unwrap_or_default(),
                    t.typname(),
                ),
                _ => ("pg_catalog".into(), t.typname()),
            }
        }
    };
    let domain = col.and_then(|c| c.domain).and_then(|d| cat.domains.get(&d));
    let collation = N;
    let identity = col.and_then(|c| c.identity.clone());
    let seq = col.and_then(|c| c.seq).and_then(|s| cat.sequences.get(&s));
    let nullable = !col.is_some_and(|c| c.not_null);
    vec![
        nm(db),
        nm(cat.schema_name(schema)),
        nm(table),
        nm(name),
        i4(pos),
        if identity.is_some() { N } else { opt(default) },
        yn(nullable),
        nm(info_data_type(ty)),
        len,
        octet,
        prec,
        radix,
        scale,
        dt_prec,
        N,
        N,
        N,
        N,
        N,
        N,
        N,
        collation,
        domain.map(|_| nm(db)).unwrap_or(N),
        domain.map(|d| nm(cat.schema_name(d.schema))).unwrap_or(N),
        domain.map(|d| nm(&d.name)).unwrap_or(N),
        nm(db),
        nm(udt_schema),
        nm(udt_name),
        N,
        N,
        N,
        N,
        nm(pos.to_string()),
        yn(false),
        yn(identity.is_some()),
        match &identity {
            Some(super::Identity::Always) => nm("ALWAYS"),
            Some(super::Identity::ByDefault) => nm("BY DEFAULT"),
            None => N,
        },
        seq.filter(|_| identity.is_some())
            .map(|s| nm(s.start.to_string()))
            .unwrap_or(N),
        seq.filter(|_| identity.is_some())
            .map(|s| nm(s.increment.to_string()))
            .unwrap_or(N),
        seq.filter(|_| identity.is_some())
            .map(|s| nm(s.max.to_string()))
            .unwrap_or(N),
        seq.filter(|_| identity.is_some())
            .map(|s| nm(s.min.to_string()))
            .unwrap_or(N),
        yn(identity.is_some() && seq.is_some_and(|s| s.cycle)),
        nm(if col.is_some_and(|c| c.generated.is_some()) {
            "ALWAYS"
        } else {
            "NEVER"
        }),
        opt(generated),
        yn(is_table),
    ]
}

/// Operator classes (as in Postgres 18): (oid, access method, name, family, input type, key type, default).
pub const OPCLASSES: &[(u32, u32, &str, u32, u32, u32, bool)] = &[
    (1978, 403, "int4_ops", 1976, 23, 0, true),
    (1979, 403, "int2_ops", 1976, 21, 0, true),
    (1981, 403, "oid_ops", 1989, 26, 0, true),
    (3122, 403, "date_ops", 434, 1082, 0, true),
    (3123, 403, "float8_ops", 1970, 701, 0, true),
    (3124, 403, "int8_ops", 1976, 20, 0, true),
    (3125, 403, "numeric_ops", 1988, 1700, 0, true),
    (3126, 403, "text_ops", 1994, 25, 0, true),
    (3127, 403, "timestamptz_ops", 434, 1184, 0, true),
    (3128, 403, "timestamp_ops", 434, 1114, 0, true),
    (4217, 403, "text_pattern_ops", 2095, 25, 0, false),
    (4218, 403, "varchar_pattern_ops", 2095, 25, 0, false),
    (4219, 403, "bpchar_pattern_ops", 2097, 1042, 0, false),
    (10000, 403, "array_ops", 397, 2277, 0, true),
    (10002, 403, "bit_ops", 423, 1560, 0, true),
    (10003, 403, "bool_ops", 424, 16, 0, true),
    (10004, 403, "bpchar_ops", 426, 1042, 0, true),
    (10006, 403, "bytea_ops", 428, 17, 0, true),
    (10007, 403, "char_ops", 429, 18, 0, true),
    (10009, 403, "cidr_ops", 1974, 869, 0, false),
    (10012, 403, "float4_ops", 1970, 700, 0, true),
    (10015, 403, "inet_ops", 1974, 869, 0, true),
    (10022, 403, "interval_ops", 1982, 1186, 0, true),
    (10024, 403, "macaddr_ops", 1984, 829, 0, true),
    (10026, 403, "macaddr8_ops", 3371, 774, 0, true),
    (10028, 403, "name_ops", 1994, 19, 2275, true),
    (10032, 403, "oidvector_ops", 1991, 30, 0, true),
    (10034, 403, "record_ops", 2994, 2249, 0, true),
    (10036, 403, "record_image_ops", 3194, 2249, 0, false),
    (10038, 403, "time_ops", 1996, 1083, 0, true),
    (10041, 403, "timetz_ops", 2000, 1266, 0, true),
    (10043, 403, "varbit_ops", 2002, 1562, 0, true),
    (10044, 403, "varchar_ops", 1994, 25, 0, false),
    (10047, 403, "money_ops", 2099, 790, 0, true),
    (10050, 403, "tid_ops", 2789, 27, 0, true),
    (10053, 403, "xid8_ops", 5067, 5069, 0, true),
    (10065, 403, "uuid_ops", 2968, 2950, 0, true),
    (10067, 403, "pg_lsn_ops", 3253, 3220, 0, true),
    (10069, 403, "enum_ops", 3522, 3500, 0, true),
    (10071, 403, "tsvector_ops", 3626, 3614, 0, true),
    (10074, 403, "tsquery_ops", 3683, 3615, 0, true),
    (10076, 403, "range_ops", 3901, 3831, 0, true),
    (10080, 403, "multirange_ops", 4199, 4537, 0, true),
    (10088, 403, "jsonb_ops", 4033, 3802, 0, true),
    (10001, 405, "array_ops", 627, 2277, 0, true),
    (10005, 405, "bpchar_ops", 427, 1042, 0, true),
    (10008, 405, "char_ops", 431, 18, 0, true),
    (10010, 405, "cidr_ops", 1975, 869, 0, false),
    (10011, 405, "date_ops", 435, 1082, 0, true),
    (10013, 405, "float4_ops", 1971, 700, 0, true),
    (10014, 405, "float8_ops", 1971, 701, 0, true),
    (10016, 405, "inet_ops", 1975, 869, 0, true),
    (10019, 405, "int2_ops", 1977, 21, 0, true),
    (10020, 405, "int4_ops", 1977, 23, 0, true),
    (10021, 405, "int8_ops", 1977, 20, 0, true),
    (10023, 405, "interval_ops", 1983, 1186, 0, true),
    (10025, 405, "macaddr_ops", 1985, 829, 0, true),
    (10027, 405, "macaddr8_ops", 3372, 774, 0, true),
    (10029, 405, "name_ops", 1995, 19, 0, true),
    (10030, 405, "numeric_ops", 1998, 1700, 0, true),
    (10031, 405, "oid_ops", 1990, 26, 0, true),
    (10033, 405, "oidvector_ops", 1992, 30, 0, true),
    (10035, 405, "record_ops", 6194, 2249, 0, true),
    (10037, 405, "text_ops", 1995, 25, 0, true),
    (10039, 405, "time_ops", 1997, 1083, 0, true),
    (10040, 405, "timestamptz_ops", 1999, 1184, 0, true),
    (10042, 405, "timetz_ops", 2001, 1266, 0, true),
    (10045, 405, "varchar_ops", 1995, 25, 0, false),
    (10046, 405, "timestamp_ops", 2040, 1114, 0, true),
    (10048, 405, "bool_ops", 2222, 16, 0, true),
    (10049, 405, "bytea_ops", 2223, 17, 0, true),
    (10051, 405, "xid_ops", 2225, 28, 0, true),
    (10052, 405, "xid8_ops", 5032, 5069, 0, true),
    (10054, 405, "cid_ops", 2226, 29, 0, true),
    (10055, 405, "tid_ops", 2227, 27, 0, true),
    (10056, 405, "text_pattern_ops", 2229, 25, 0, false),
    (10057, 405, "varchar_pattern_ops", 2229, 25, 0, false),
    (10058, 405, "bpchar_pattern_ops", 2231, 1042, 0, false),
    (10059, 405, "aclitem_ops", 2235, 1033, 0, true),
    (10066, 405, "uuid_ops", 2969, 2950, 0, true),
    (10068, 405, "pg_lsn_ops", 3254, 3220, 0, true),
    (10070, 405, "enum_ops", 3523, 3500, 0, true),
    (10077, 405, "range_ops", 3903, 3831, 0, true),
    (10081, 405, "multirange_ops", 4225, 4537, 0, true),
    (10089, 405, "jsonb_ops", 4034, 3802, 0, true),
    (10017, 783, "inet_ops", 3550, 869, 0, false),
    (10060, 783, "box_ops", 2593, 603, 0, true),
    (10061, 783, "point_ops", 1029, 600, 603, true),
    (10062, 783, "poly_ops", 2594, 604, 603, true),
    (10063, 783, "circle_ops", 2595, 718, 603, true),
    (10072, 783, "tsvector_ops", 3655, 3614, 3642, true),
    (10075, 783, "tsquery_ops", 3702, 3615, 20, true),
    (10078, 783, "range_ops", 3919, 3831, 0, true),
    (10082, 783, "multirange_ops", 6158, 4537, 3831, true),
    (10064, 2742, "array_ops", 2745, 2277, 2283, true),
    (10073, 2742, "tsvector_ops", 3659, 3614, 25, true),
    (10090, 2742, "jsonb_ops", 4036, 3802, 25, true),
    (10091, 2742, "jsonb_path_ops", 4037, 3802, 23, false),
    (10092, 3580, "bytea_minmax_ops", 4064, 17, 17, true),
    (10093, 3580, "bytea_bloom_ops", 4578, 17, 17, false),
    (10094, 3580, "char_minmax_ops", 4062, 18, 18, true),
    (10095, 3580, "char_bloom_ops", 4577, 18, 18, false),
    (10096, 3580, "name_minmax_ops", 4065, 19, 19, true),
    (10097, 3580, "name_bloom_ops", 4579, 19, 19, false),
    (10098, 3580, "int8_minmax_ops", 4054, 20, 20, true),
    (10099, 3580, "int8_minmax_multi_ops", 4602, 20, 20, false),
    (10100, 3580, "int8_bloom_ops", 4572, 20, 20, false),
    (10101, 3580, "int2_minmax_ops", 4054, 21, 21, true),
    (10102, 3580, "int2_minmax_multi_ops", 4602, 21, 21, false),
    (10103, 3580, "int2_bloom_ops", 4572, 21, 21, false),
    (10104, 3580, "int4_minmax_ops", 4054, 23, 23, true),
    (10105, 3580, "int4_minmax_multi_ops", 4602, 23, 23, false),
    (10106, 3580, "int4_bloom_ops", 4572, 23, 23, false),
    (10107, 3580, "text_minmax_ops", 4056, 25, 25, true),
    (10108, 3580, "text_bloom_ops", 4573, 25, 25, false),
    (10109, 3580, "oid_minmax_ops", 4068, 26, 26, true),
    (10110, 3580, "oid_minmax_multi_ops", 4606, 26, 26, false),
    (10111, 3580, "oid_bloom_ops", 4580, 26, 26, false),
    (10112, 3580, "tid_minmax_ops", 4069, 27, 27, true),
    (10113, 3580, "tid_bloom_ops", 4581, 27, 27, false),
    (10114, 3580, "tid_minmax_multi_ops", 4607, 27, 27, false),
    (10115, 3580, "float4_minmax_ops", 4070, 700, 700, true),
    (
        10116,
        3580,
        "float4_minmax_multi_ops",
        4608,
        700,
        700,
        false,
    ),
    (10117, 3580, "float4_bloom_ops", 4582, 700, 700, false),
    (10118, 3580, "float8_minmax_ops", 4070, 701, 701, true),
    (
        10119,
        3580,
        "float8_minmax_multi_ops",
        4608,
        701,
        701,
        false,
    ),
    (10120, 3580, "float8_bloom_ops", 4582, 701, 701, false),
    (10121, 3580, "macaddr_minmax_ops", 4074, 829, 829, true),
    (
        10122,
        3580,
        "macaddr_minmax_multi_ops",
        4609,
        829,
        829,
        false,
    ),
    (10123, 3580, "macaddr_bloom_ops", 4583, 829, 829, false),
    (10124, 3580, "macaddr8_minmax_ops", 4109, 774, 774, true),
    (
        10125,
        3580,
        "macaddr8_minmax_multi_ops",
        4610,
        774,
        774,
        false,
    ),
    (10126, 3580, "macaddr8_bloom_ops", 4584, 774, 774, false),
    (10127, 3580, "inet_minmax_ops", 4075, 869, 869, false),
    (10128, 3580, "inet_minmax_multi_ops", 4611, 869, 869, false),
    (10129, 3580, "inet_bloom_ops", 4585, 869, 869, false),
    (10130, 3580, "inet_inclusion_ops", 4102, 869, 869, true),
    (10131, 3580, "bpchar_minmax_ops", 4076, 1042, 1042, true),
    (10132, 3580, "bpchar_bloom_ops", 4586, 1042, 1042, false),
    (10133, 3580, "time_minmax_ops", 4077, 1083, 1083, true),
    (
        10134,
        3580,
        "time_minmax_multi_ops",
        4612,
        1083,
        1083,
        false,
    ),
    (10135, 3580, "time_bloom_ops", 4587, 1083, 1083, false),
    (10136, 3580, "date_minmax_ops", 4059, 1082, 1082, true),
    (
        10137,
        3580,
        "date_minmax_multi_ops",
        4605,
        1082,
        1082,
        false,
    ),
    (10138, 3580, "date_bloom_ops", 4576, 1082, 1082, false),
    (10139, 3580, "timestamp_minmax_ops", 4059, 1114, 1114, true),
    (
        10140,
        3580,
        "timestamp_minmax_multi_ops",
        4605,
        1114,
        1114,
        false,
    ),
    (10141, 3580, "timestamp_bloom_ops", 4576, 1114, 1114, false),
    (
        10142,
        3580,
        "timestamptz_minmax_ops",
        4059,
        1184,
        1184,
        true,
    ),
    (
        10143,
        3580,
        "timestamptz_minmax_multi_ops",
        4605,
        1184,
        1184,
        false,
    ),
    (
        10144,
        3580,
        "timestamptz_bloom_ops",
        4576,
        1184,
        1184,
        false,
    ),
    (10145, 3580, "interval_minmax_ops", 4078, 1186, 1186, true),
    (
        10146,
        3580,
        "interval_minmax_multi_ops",
        4613,
        1186,
        1186,
        false,
    ),
    (10147, 3580, "interval_bloom_ops", 4588, 1186, 1186, false),
    (10148, 3580, "timetz_minmax_ops", 4058, 1266, 1266, true),
    (
        10149,
        3580,
        "timetz_minmax_multi_ops",
        4604,
        1266,
        1266,
        false,
    ),
    (10150, 3580, "timetz_bloom_ops", 4575, 1266, 1266, false),
    (10151, 3580, "bit_minmax_ops", 4079, 1560, 1560, true),
    (10152, 3580, "varbit_minmax_ops", 4080, 1562, 1562, true),
    (10153, 3580, "numeric_minmax_ops", 4055, 1700, 1700, true),
    (
        10154,
        3580,
        "numeric_minmax_multi_ops",
        4603,
        1700,
        1700,
        false,
    ),
    (10155, 3580, "numeric_bloom_ops", 4574, 1700, 1700, false),
    (10156, 3580, "uuid_minmax_ops", 4081, 2950, 2950, true),
    (
        10157,
        3580,
        "uuid_minmax_multi_ops",
        4614,
        2950,
        2950,
        false,
    ),
    (10158, 3580, "uuid_bloom_ops", 4589, 2950, 2950, false),
    (10159, 3580, "range_inclusion_ops", 4103, 3831, 3831, true),
    (10160, 3580, "pg_lsn_minmax_ops", 4082, 3220, 3220, true),
    (
        10161,
        3580,
        "pg_lsn_minmax_multi_ops",
        4615,
        3220,
        3220,
        false,
    ),
    (10162, 3580, "pg_lsn_bloom_ops", 4590, 3220, 3220, false),
    (10163, 3580, "box_inclusion_ops", 4104, 603, 603, true),
    (10018, 4000, "inet_ops", 3794, 869, 0, true),
    (10079, 4000, "range_ops", 3474, 3831, 0, true),
    (10083, 4000, "box_ops", 5000, 603, 0, true),
    (10084, 4000, "quad_point_ops", 4015, 600, 0, true),
    (10085, 4000, "kd_point_ops", 4016, 600, 0, false),
    (10086, 4000, "text_ops", 4017, 25, 0, true),
    (10087, 4000, "poly_ops", 5008, 604, 603, true),
];

/// Operator families (as in Postgres 18): (oid, access method, name).
pub const OPFAMILIES: &[(u32, u32, &str)] = &[
    (397, 403, "array_ops"),
    (423, 403, "bit_ops"),
    (424, 403, "bool_ops"),
    (426, 403, "bpchar_ops"),
    (428, 403, "bytea_ops"),
    (429, 403, "char_ops"),
    (434, 403, "datetime_ops"),
    (1970, 403, "float_ops"),
    (1974, 403, "network_ops"),
    (1976, 403, "integer_ops"),
    (1982, 403, "interval_ops"),
    (1984, 403, "macaddr_ops"),
    (1988, 403, "numeric_ops"),
    (1989, 403, "oid_ops"),
    (1991, 403, "oidvector_ops"),
    (1994, 403, "text_ops"),
    (1996, 403, "time_ops"),
    (2000, 403, "timetz_ops"),
    (2002, 403, "varbit_ops"),
    (2095, 403, "text_pattern_ops"),
    (2097, 403, "bpchar_pattern_ops"),
    (2099, 403, "money_ops"),
    (2789, 403, "tid_ops"),
    (2968, 403, "uuid_ops"),
    (2994, 403, "record_ops"),
    (3194, 403, "record_image_ops"),
    (3253, 403, "pg_lsn_ops"),
    (3371, 403, "macaddr8_ops"),
    (3522, 403, "enum_ops"),
    (3626, 403, "tsvector_ops"),
    (3683, 403, "tsquery_ops"),
    (3901, 403, "range_ops"),
    (4033, 403, "jsonb_ops"),
    (4199, 403, "multirange_ops"),
    (5067, 403, "xid8_ops"),
    (427, 405, "bpchar_ops"),
    (431, 405, "char_ops"),
    (435, 405, "date_ops"),
    (627, 405, "array_ops"),
    (1971, 405, "float_ops"),
    (1975, 405, "network_ops"),
    (1977, 405, "integer_ops"),
    (1983, 405, "interval_ops"),
    (1985, 405, "macaddr_ops"),
    (1990, 405, "oid_ops"),
    (1992, 405, "oidvector_ops"),
    (1995, 405, "text_ops"),
    (1997, 405, "time_ops"),
    (1998, 405, "numeric_ops"),
    (1999, 405, "timestamptz_ops"),
    (2001, 405, "timetz_ops"),
    (2040, 405, "timestamp_ops"),
    (2222, 405, "bool_ops"),
    (2223, 405, "bytea_ops"),
    (2225, 405, "xid_ops"),
    (2226, 405, "cid_ops"),
    (2227, 405, "tid_ops"),
    (2229, 405, "text_pattern_ops"),
    (2231, 405, "bpchar_pattern_ops"),
    (2235, 405, "aclitem_ops"),
    (2969, 405, "uuid_ops"),
    (3254, 405, "pg_lsn_ops"),
    (3372, 405, "macaddr8_ops"),
    (3523, 405, "enum_ops"),
    (3903, 405, "range_ops"),
    (4034, 405, "jsonb_ops"),
    (4225, 405, "multirange_ops"),
    (5032, 405, "xid8_ops"),
    (6194, 405, "record_ops"),
    (1029, 783, "point_ops"),
    (2593, 783, "box_ops"),
    (2594, 783, "poly_ops"),
    (2595, 783, "circle_ops"),
    (3550, 783, "network_ops"),
    (3655, 783, "tsvector_ops"),
    (3702, 783, "tsquery_ops"),
    (3919, 783, "range_ops"),
    (6158, 783, "multirange_ops"),
    (2745, 2742, "array_ops"),
    (3659, 2742, "tsvector_ops"),
    (4036, 2742, "jsonb_ops"),
    (4037, 2742, "jsonb_path_ops"),
    (4054, 3580, "integer_minmax_ops"),
    (4055, 3580, "numeric_minmax_ops"),
    (4056, 3580, "text_minmax_ops"),
    (4058, 3580, "timetz_minmax_ops"),
    (4059, 3580, "datetime_minmax_ops"),
    (4062, 3580, "char_minmax_ops"),
    (4064, 3580, "bytea_minmax_ops"),
    (4065, 3580, "name_minmax_ops"),
    (4068, 3580, "oid_minmax_ops"),
    (4069, 3580, "tid_minmax_ops"),
    (4070, 3580, "float_minmax_ops"),
    (4074, 3580, "macaddr_minmax_ops"),
    (4075, 3580, "network_minmax_ops"),
    (4076, 3580, "bpchar_minmax_ops"),
    (4077, 3580, "time_minmax_ops"),
    (4078, 3580, "interval_minmax_ops"),
    (4079, 3580, "bit_minmax_ops"),
    (4080, 3580, "varbit_minmax_ops"),
    (4081, 3580, "uuid_minmax_ops"),
    (4082, 3580, "pg_lsn_minmax_ops"),
    (4102, 3580, "network_inclusion_ops"),
    (4103, 3580, "range_inclusion_ops"),
    (4104, 3580, "box_inclusion_ops"),
    (4109, 3580, "macaddr8_minmax_ops"),
    (4572, 3580, "integer_bloom_ops"),
    (4573, 3580, "text_bloom_ops"),
    (4574, 3580, "numeric_bloom_ops"),
    (4575, 3580, "timetz_bloom_ops"),
    (4576, 3580, "datetime_bloom_ops"),
    (4577, 3580, "char_bloom_ops"),
    (4578, 3580, "bytea_bloom_ops"),
    (4579, 3580, "name_bloom_ops"),
    (4580, 3580, "oid_bloom_ops"),
    (4581, 3580, "tid_bloom_ops"),
    (4582, 3580, "float_bloom_ops"),
    (4583, 3580, "macaddr_bloom_ops"),
    (4584, 3580, "macaddr8_bloom_ops"),
    (4585, 3580, "network_bloom_ops"),
    (4586, 3580, "bpchar_bloom_ops"),
    (4587, 3580, "time_bloom_ops"),
    (4588, 3580, "interval_bloom_ops"),
    (4589, 3580, "uuid_bloom_ops"),
    (4590, 3580, "pg_lsn_bloom_ops"),
    (4602, 3580, "integer_minmax_multi_ops"),
    (4603, 3580, "numeric_minmax_multi_ops"),
    (4604, 3580, "timetz_minmax_multi_ops"),
    (4605, 3580, "datetime_minmax_multi_ops"),
    (4606, 3580, "oid_minmax_multi_ops"),
    (4607, 3580, "tid_minmax_multi_ops"),
    (4608, 3580, "float_minmax_multi_ops"),
    (4609, 3580, "macaddr_minmax_multi_ops"),
    (4610, 3580, "macaddr8_minmax_multi_ops"),
    (4611, 3580, "network_minmax_multi_ops"),
    (4612, 3580, "time_minmax_multi_ops"),
    (4613, 3580, "interval_minmax_multi_ops"),
    (4614, 3580, "uuid_minmax_multi_ops"),
    (4615, 3580, "pg_lsn_minmax_multi_ops"),
    (3474, 4000, "range_ops"),
    (3794, 4000, "network_ops"),
    (4015, 4000, "quad_point_ops"),
    (4016, 4000, "kd_point_ops"),
    (4017, 4000, "text_ops"),
    (5000, 4000, "box_ops"),
    (5008, 4000, "poly_ops"),
];
