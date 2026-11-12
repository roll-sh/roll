<!-- Generated from our own observations of the reference images (see conformance.md for the capture method). Do not edit by hand. -->

# system_schema

Part 1 lists every `system_schema` table with its columns. Part 2 shows the rows each dialect writes for the sample schema below (statements that a dialect rejects are listed with their error). Table ids and schema versions are masked as `<uuid>`. Rows are as the Python driver decodes them.

## Part 1 — tables and columns, ScyllaDB 2026.3.3

### system_schema.aggregates

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | NONE | text |
| aggregate_name | clustering | 0 | ASC | text |
| argument_types | clustering | 1 | ASC | frozen<list<text>> |
| final_func | regular | -1 | NONE | text |
| initcond | regular | -1 | NONE | text |
| return_type | regular | -1 | NONE | text |
| state_func | regular | -1 | NONE | text |
| state_type | regular | -1 | NONE | text |

### system_schema.columns

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | NONE | text |
| table_name | clustering | 0 | ASC | text |
| column_name | clustering | 1 | ASC | text |
| clustering_order | regular | -1 | NONE | text |
| column_name_bytes | regular | -1 | NONE | blob |
| kind | regular | -1 | NONE | text |
| position | regular | -1 | NONE | int |
| type | regular | -1 | NONE | text |

### system_schema.computed_columns

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | NONE | text |
| table_name | clustering | 0 | ASC | text |
| column_name | clustering | 1 | ASC | text |
| computation | regular | -1 | NONE | blob |

### system_schema.dropped_columns

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | NONE | text |
| table_name | clustering | 0 | ASC | text |
| column_name | clustering | 1 | ASC | text |
| dropped_time | regular | -1 | NONE | timestamp |
| type | regular | -1 | NONE | text |

### system_schema.functions

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | NONE | text |
| function_name | clustering | 0 | ASC | text |
| argument_types | clustering | 1 | ASC | frozen<list<text>> |
| argument_names | regular | -1 | NONE | frozen<list<text>> |
| body | regular | -1 | NONE | text |
| called_on_null_input | regular | -1 | NONE | boolean |
| language | regular | -1 | NONE | text |
| return_type | regular | -1 | NONE | text |

### system_schema.indexes

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | NONE | text |
| table_name | clustering | 0 | ASC | text |
| index_name | clustering | 1 | ASC | text |
| kind | regular | -1 | NONE | text |
| options | regular | -1 | NONE | frozen<map<text, text>> |

### system_schema.keyspaces

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | NONE | text |
| durable_writes | regular | -1 | NONE | boolean |
| next_replication | regular | -1 | NONE | frozen<map<text, text>> |
| replication | regular | -1 | NONE | frozen<map<text, text>> |
| replication_v2 | regular | -1 | NONE | frozen<map<text, text>> |

### system_schema.scylla_aggregates

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | NONE | text |
| aggregate_name | clustering | 0 | ASC | text |
| argument_types | clustering | 1 | ASC | frozen<list<text>> |
| reduce_func | regular | -1 | NONE | text |
| state_type | regular | -1 | NONE | text |

### system_schema.scylla_keyspaces

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | NONE | text |
| consistency | regular | -1 | NONE | text |
| initial_tablets | regular | -1 | NONE | int |
| storage_options | regular | -1 | NONE | frozen<map<text, text>> |
| storage_type | regular | -1 | NONE | text |

### system_schema.scylla_tables

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | NONE | text |
| table_name | clustering | 0 | ASC | text |
| cdc | regular | -1 | NONE | frozen<map<text, text>> |
| committed_by_group0 | regular | -1 | NONE | boolean |
| in_memory | regular | -1 | NONE | boolean |
| large_data_guardrails_enabled | regular | -1 | NONE | boolean |
| partitioner | regular | -1 | NONE | text |
| storage_engine | regular | -1 | NONE | text |
| tablets | regular | -1 | NONE | frozen<map<text, text>> |
| version | regular | -1 | NONE | uuid |

### system_schema.tables

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | NONE | text |
| table_name | clustering | 0 | ASC | text |
| bloom_filter_fp_chance | regular | -1 | NONE | double |
| caching | regular | -1 | NONE | frozen<map<text, text>> |
| comment | regular | -1 | NONE | text |
| compaction | regular | -1 | NONE | frozen<map<text, text>> |
| compression | regular | -1 | NONE | frozen<map<text, text>> |
| crc_check_chance | regular | -1 | NONE | double |
| dclocal_read_repair_chance | regular | -1 | NONE | double |
| default_time_to_live | regular | -1 | NONE | int |
| extensions | regular | -1 | NONE | frozen<map<text, blob>> |
| flags | regular | -1 | NONE | frozen<set<text>> |
| gc_grace_seconds | regular | -1 | NONE | int |
| id | regular | -1 | NONE | uuid |
| max_index_interval | regular | -1 | NONE | int |
| memtable_flush_period_in_ms | regular | -1 | NONE | int |
| min_index_interval | regular | -1 | NONE | int |
| read_repair_chance | regular | -1 | NONE | double |
| speculative_retry | regular | -1 | NONE | text |

### system_schema.triggers

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | NONE | text |
| table_name | clustering | 0 | ASC | text |
| trigger_name | clustering | 1 | ASC | text |
| options | regular | -1 | NONE | frozen<map<text, text>> |

### system_schema.types

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | NONE | text |
| type_name | clustering | 0 | ASC | text |
| field_names | regular | -1 | NONE | frozen<list<text>> |
| field_types | regular | -1 | NONE | frozen<list<text>> |

### system_schema.view_virtual_columns

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | NONE | text |
| table_name | clustering | 0 | ASC | text |
| column_name | clustering | 1 | ASC | text |
| clustering_order | regular | -1 | NONE | text |
| column_name_bytes | regular | -1 | NONE | blob |
| kind | regular | -1 | NONE | text |
| position | regular | -1 | NONE | int |
| type | regular | -1 | NONE | text |

### system_schema.views

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | NONE | text |
| view_name | clustering | 0 | ASC | text |
| base_table_id | regular | -1 | NONE | uuid |
| base_table_name | regular | -1 | NONE | text |
| bloom_filter_fp_chance | regular | -1 | NONE | double |
| caching | regular | -1 | NONE | frozen<map<text, text>> |
| comment | regular | -1 | NONE | text |
| compaction | regular | -1 | NONE | frozen<map<text, text>> |
| compression | regular | -1 | NONE | frozen<map<text, text>> |
| crc_check_chance | regular | -1 | NONE | double |
| dclocal_read_repair_chance | regular | -1 | NONE | double |
| default_time_to_live | regular | -1 | NONE | int |
| extensions | regular | -1 | NONE | frozen<map<text, blob>> |
| gc_grace_seconds | regular | -1 | NONE | int |
| id | regular | -1 | NONE | uuid |
| include_all_columns | regular | -1 | NONE | boolean |
| max_index_interval | regular | -1 | NONE | int |
| memtable_flush_period_in_ms | regular | -1 | NONE | int |
| min_index_interval | regular | -1 | NONE | int |
| read_repair_chance | regular | -1 | NONE | double |
| speculative_retry | regular | -1 | NONE | text |
| where_clause | regular | -1 | NONE | text |

## Part 1 — tables and columns, Cassandra 5.0.9

### system_schema.aggregates

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | none | text |
| aggregate_name | clustering | 0 | asc | text |
| argument_types | clustering | 1 | asc | frozen<list<text>> |
| final_func | regular | -1 | none | text |
| initcond | regular | -1 | none | text |
| return_type | regular | -1 | none | text |
| state_func | regular | -1 | none | text |
| state_type | regular | -1 | none | text |

### system_schema.column_masks

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | none | text |
| table_name | clustering | 0 | asc | text |
| column_name | clustering | 1 | asc | text |
| function_argument_nulls | regular | -1 | none | frozen<list<boolean>> |
| function_argument_types | regular | -1 | none | frozen<list<text>> |
| function_argument_values | regular | -1 | none | frozen<list<text>> |
| function_keyspace | regular | -1 | none | text |
| function_name | regular | -1 | none | text |

### system_schema.columns

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | none | text |
| table_name | clustering | 0 | asc | text |
| column_name | clustering | 1 | asc | text |
| clustering_order | regular | -1 | none | text |
| column_name_bytes | regular | -1 | none | blob |
| kind | regular | -1 | none | text |
| position | regular | -1 | none | int |
| type | regular | -1 | none | text |

### system_schema.dropped_columns

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | none | text |
| table_name | clustering | 0 | asc | text |
| column_name | clustering | 1 | asc | text |
| dropped_time | regular | -1 | none | timestamp |
| kind | regular | -1 | none | text |
| type | regular | -1 | none | text |

### system_schema.functions

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | none | text |
| function_name | clustering | 0 | asc | text |
| argument_types | clustering | 1 | asc | frozen<list<text>> |
| argument_names | regular | -1 | none | frozen<list<text>> |
| body | regular | -1 | none | text |
| called_on_null_input | regular | -1 | none | boolean |
| language | regular | -1 | none | text |
| return_type | regular | -1 | none | text |

### system_schema.indexes

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | none | text |
| table_name | clustering | 0 | asc | text |
| index_name | clustering | 1 | asc | text |
| kind | regular | -1 | none | text |
| options | regular | -1 | none | frozen<map<text, text>> |

### system_schema.keyspaces

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | none | text |
| durable_writes | regular | -1 | none | boolean |
| replication | regular | -1 | none | frozen<map<text, text>> |

### system_schema.tables

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | none | text |
| table_name | clustering | 0 | asc | text |
| additional_write_policy | regular | -1 | none | text |
| allow_auto_snapshot | regular | -1 | none | boolean |
| bloom_filter_fp_chance | regular | -1 | none | double |
| caching | regular | -1 | none | frozen<map<text, text>> |
| cdc | regular | -1 | none | boolean |
| comment | regular | -1 | none | text |
| compaction | regular | -1 | none | frozen<map<text, text>> |
| compression | regular | -1 | none | frozen<map<text, text>> |
| crc_check_chance | regular | -1 | none | double |
| dclocal_read_repair_chance | regular | -1 | none | double |
| default_time_to_live | regular | -1 | none | int |
| extensions | regular | -1 | none | frozen<map<text, blob>> |
| flags | regular | -1 | none | frozen<set<text>> |
| gc_grace_seconds | regular | -1 | none | int |
| id | regular | -1 | none | uuid |
| incremental_backups | regular | -1 | none | boolean |
| max_index_interval | regular | -1 | none | int |
| memtable | regular | -1 | none | text |
| memtable_flush_period_in_ms | regular | -1 | none | int |
| min_index_interval | regular | -1 | none | int |
| read_repair | regular | -1 | none | text |
| read_repair_chance | regular | -1 | none | double |
| speculative_retry | regular | -1 | none | text |

### system_schema.triggers

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | none | text |
| table_name | clustering | 0 | asc | text |
| trigger_name | clustering | 1 | asc | text |
| options | regular | -1 | none | frozen<map<text, text>> |

### system_schema.types

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | none | text |
| type_name | clustering | 0 | asc | text |
| field_names | regular | -1 | none | frozen<list<text>> |
| field_types | regular | -1 | none | frozen<list<text>> |

### system_schema.views

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | none | text |
| view_name | clustering | 0 | asc | text |
| additional_write_policy | regular | -1 | none | text |
| allow_auto_snapshot | regular | -1 | none | boolean |
| base_table_id | regular | -1 | none | uuid |
| base_table_name | regular | -1 | none | text |
| bloom_filter_fp_chance | regular | -1 | none | double |
| caching | regular | -1 | none | frozen<map<text, text>> |
| cdc | regular | -1 | none | boolean |
| comment | regular | -1 | none | text |
| compaction | regular | -1 | none | frozen<map<text, text>> |
| compression | regular | -1 | none | frozen<map<text, text>> |
| crc_check_chance | regular | -1 | none | double |
| dclocal_read_repair_chance | regular | -1 | none | double |
| default_time_to_live | regular | -1 | none | int |
| extensions | regular | -1 | none | frozen<map<text, blob>> |
| gc_grace_seconds | regular | -1 | none | int |
| id | regular | -1 | none | uuid |
| include_all_columns | regular | -1 | none | boolean |
| incremental_backups | regular | -1 | none | boolean |
| max_index_interval | regular | -1 | none | int |
| memtable | regular | -1 | none | text |
| memtable_flush_period_in_ms | regular | -1 | none | int |
| min_index_interval | regular | -1 | none | int |
| read_repair | regular | -1 | none | text |
| read_repair_chance | regular | -1 | none | double |
| speculative_retry | regular | -1 | none | text |
| where_clause | regular | -1 | none | text |

## Part 2 — the sample schema

| step | statement | ScyllaDB 2026.3.3 | Cassandra 5.0.9 |
|---|---|---|---|
| ks | `CREATE KEYSPACE ref WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}` | OK; warnings: "Using Replication Factor datacenter1=1 lower than the minimum_replication_factor_warn_threshold=3 is not recommended." | OK |
| ks_simple | `CREATE KEYSPACE ref_simple WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1} AND durable_writes = false` | `0x2300` SimpleStrategy doesn't support tablet replication | OK |
| ks_simple_vnodes_scylla | `CREATE KEYSPACE ref_simple WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1} AND durable_writes = false AND tablets = {'enabled': false}` | OK; warnings: "SimpleStrategy replication class is not recommended, but was used for keyspace ref_simple. You may suppress this warning by delisting SimpleStrategy from replication_strategy_warn_list configuration option, or make it into an error by listing this replication strategy on replication_strategy_fail_list." / "Using Replication Factor replication_factor=1 lower than the minimum_replication_factor_warn_threshold=3 is not recommended." | `0x2000` Unknown property 'tablets' |
| ks_vnodes_scylla | `CREATE KEYSPACE ref_vnodes WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1} AND tablets = {'enabled': false}` | OK; warnings: "Using Replication Factor datacenter1=1 lower than the minimum_replication_factor_warn_threshold=3 is not recommended." | `0x2000` Unknown property 'tablets' |
| ks_vnodes_cassandra_fallback | `CREATE KEYSPACE IF NOT EXISTS ref_vnodes WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}` | OK; warnings: "Using Replication Factor datacenter1=1 lower than the minimum_replication_factor_warn_threshold=3 is not recommended." | OK |
| udt | `CREATE TYPE ref.address (street text, zip int, tags frozen<set<text>>)` | OK | OK |
| udt_nested | `CREATE TYPE ref.person (name text, home frozen<address>, "Quoted Field" int)` | OK | OK |
| t_simple | `CREATE TABLE ref.t_simple (id int PRIMARY KEY, v text)` | OK | OK |
| t_all | `CREATE TABLE ref.t_all ( a int, b text, c1 timestamp, c2 uuid, s text STATIC, asc_ ascii, big bigint, bl blob, bo boolean, da date, de decimal, do double, du duration, fl float, ine inet, sm smallint, ti time, tu timeuuid, tin tinyint, vi varint, txt text, vc varchar, l list<int>, st set<text>, m ma… (691 chars)` | OK | OK |
| t_quoted | `CREATE TABLE ref."QuotedTable" ("Key" int, "Clu Col" text, "select" int, "camelCase" text, PRIMARY KEY ("Key", "Clu Col"))` | OK | OK |
| t_quoted_bad | `CREATE TABLE ref."Quoted Table" (k int PRIMARY KEY)` | `0x2200` "Quoted Table" is not a valid table name (must be alphanumeric character only: [0-9A-Za-z]+) | `0x2300` ref.Quoted Table: Table name must not be empty or not contain non-alphanumeric-underscore characters (got "Quoted Table") |
| t_counter | `CREATE TABLE ref.t_counter (id int, k text, c counter, c2 counter, PRIMARY KEY (id, k))` | OK | OK |
| t_compact | `CREATE TABLE ref.t_compact (id int PRIMARY KEY, v int) WITH COMPACT STORAGE` | `0x2200` Support for the deprecated feature of 'CREATE TABLE WITH COMPACT STORAGE' is disabled and will eventually be removed in a future version.  To enable, set the 'enable_create_table_with_compact_storage' config option to 'true'. | OK |
| t_compact_clustering | `CREATE TABLE ref.t_compact2 (id int, ck int, v int, PRIMARY KEY (id, ck)) WITH COMPACT STORAGE` | `0x2200` Support for the deprecated feature of 'CREATE TABLE WITH COMPACT STORAGE' is disabled and will eventually be removed in a future version.  To enable, set the 'enable_create_table_with_compact_storage' config option to 'true'. | OK |
| t_opts | `CREATE TABLE ref.t_opts (id int PRIMARY KEY, v text) WITH default_time_to_live = 3600 AND gc_grace_seconds = 3600 AND compaction = {'class': 'LeveledCompactionStrategy', 'sstable_size_in_mb': '64'} AND caching = {'keys': 'ALL', 'rows_per_partition': '10'} AND bloom_filter_fp_chance = 0.1 AND specula… (452 chars)` | OK | OK |
| t_twcs | `CREATE TABLE ref.t_twcs (id int, ts timestamp, v text, PRIMARY KEY (id, ts)) WITH compaction = {'class': 'TimeWindowCompactionStrategy', 'compaction_window_unit': 'HOURS', 'compaction_window_size': '6'}` | OK | OK |
| t_compression_class | `CREATE TABLE ref.t_comp1 (id int PRIMARY KEY) WITH compression = {'class': 'ZstdCompressor', 'chunk_length_in_kb': '16'}` | `0x2300` Missing sub-option 'sstable_compression' for the 'compression' option. | OK |
| t_compression_sstable_compression | `CREATE TABLE ref.t_comp2 (id int PRIMARY KEY) WITH compression = {'sstable_compression': 'DeflateCompressor', 'chunk_length_in_kb': '64'}` | OK | `0x2300` Missing sub-option 'class' for the 'compression' option. |
| t_compression_disabled | `CREATE TABLE ref.t_comp3 (id int PRIMARY KEY) WITH compression = {'enabled': 'false'}` | `0x2300` Missing sub-option 'sstable_compression' for the 'compression' option. | OK |
| t_ucs_cassandra | `CREATE TABLE ref.t_ucs (id int PRIMARY KEY) WITH compaction = {'class': 'UnifiedCompactionStrategy'}` | `0x2300` Unable to find compaction strategy class 'UnifiedCompactionStrategy' | OK |
| t_ics_scylla | `CREATE TABLE ref.t_ics (id int PRIMARY KEY) WITH compaction = {'class': 'IncrementalCompactionStrategy'}` | OK | `0x2300` Unable to find compaction strategy class 'org.apache.cassandra.db.compaction.IncrementalCompactionStrategy' |
| t_tombstone_gc_scylla | `CREATE TABLE ref.t_tgc (id int PRIMARY KEY) WITH tombstone_gc = {'mode': 'timeout'}` | OK | `0x2000` Unknown property 'tombstone_gc' |
| t_cdc_scylla | `CREATE TABLE ref.t_cdc (id int PRIMARY KEY, v int) WITH cdc = {'enabled': true}` | OK | `0x2000` Invalid value for property 'cdc'. It should be a string |
| t_cdc_cassandra | `CREATE TABLE ref.t_cdc2 (id int PRIMARY KEY, v int) WITH cdc = true` | `0x0000` Cannot create cdc info from string | OK |
| t_read_repair_cassandra | `CREATE TABLE ref.t_rr (id int PRIMARY KEY) WITH read_repair = 'NONE' AND additional_write_policy = '90p'` | `0x2000` Unknown property 'additional_write_policy' | OK |
| t_memtable_cassandra | `CREATE TABLE ref.t_mt (id int PRIMARY KEY) WITH memtable = 'default'` | `0x2000` Unknown property 'memtable' | OK |
| t_with_id | `CREATE TABLE ref.t_id (id int PRIMARY KEY) WITH ID = 5a1c395e-b41f-11e5-9f22-ba0be0483c18` | OK | OK |
| t_paxos_grace_scylla | `CREATE TABLE ref.t_pgs (id int PRIMARY KEY) WITH paxos_grace_seconds = 100` | OK | `0x2000` Unknown property 'paxos_grace_seconds' |
| alter_add | `ALTER TABLE ref.t_simple ADD dropped_col int` | OK | OK |
| alter_drop | `ALTER TABLE ref.t_simple DROP dropped_col` | OK | OK |
| alter_add_list | `ALTER TABLE ref.t_simple ADD dropped_list list<text>` | OK | OK |
| alter_drop_list | `ALTER TABLE ref.t_simple DROP dropped_list` | OK | OK |
| alter_rename_ck | `ALTER TABLE ref.t_twcs RENAME ts TO at` | OK | OK |
| alter_options | `ALTER TABLE ref.t_simple WITH comment = 'altered' AND gc_grace_seconds = 7200` | OK | OK |
| index | `CREATE INDEX t_all_txt_idx ON ref.t_all (txt)` | OK; warnings: "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | OK |
| index_keys | `CREATE INDEX t_all_m_keys_idx ON ref.t_all (KEYS(m))` | OK; warnings: "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | OK |
| index_values_noname | `CREATE INDEX ON ref.t_all (st)` | OK; warnings: "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | OK |
| index_entries | `CREATE INDEX t_all_m_entries_idx ON ref.t_all (ENTRIES(m))` | OK; warnings: "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | OK |
| index_full | `CREATE INDEX t_all_fl_l_idx ON ref.t_all (FULL(fl_l))` | OK; warnings: "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | OK |
| index_static | `CREATE INDEX t_all_s_idx ON ref.t_all (s)` | OK; warnings: "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | OK |
| index_pk_component | `CREATE INDEX t_all_b_idx ON ref.t_all (b)` | OK; warnings: "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | OK |
| index_clustering | `CREATE INDEX t_all_c2_idx ON ref.t_all (c2)` | OK; warnings: "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | OK |
| index_local_scylla | `CREATE INDEX t_all_local_idx ON ref.t_all ((a, b), vi)` | OK; warnings: "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | `0x2000` line 1:43 mismatched input '(' expecting ')' (... t_all_local_idx ON ref.t_all ([(]...) |
| index_quoted | `CREATE INDEX "Quoted Idx" ON ref."QuotedTable" ("camelCase")` | OK; warnings: "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | `0x2300` Illegal index name Quoted Idx |
| index_quoted_ok | `CREATE INDEX "QuotedIdx" ON ref."QuotedTable" ("camelCase")` | `0x2200` Index QuotedIdx is a duplicate of existing index Quoted Idx | OK |
| index_sai_cassandra | `CREATE INDEX t_simple_v_sai ON ref.t_simple (v) USING 'sai'` | `0x2200` StorageAttachedIndex (SAI) is only supported on vector columns; use a secondary index for non-vector columns | OK |
| index_sai_options_cassandra | `CREATE INDEX t_all_txt_sai ON ref.t_all (vc) USING 'StorageAttachedIndex' WITH OPTIONS = {'case_sensitive': 'false', 'normalize': 'true'}` | `0x2200` StorageAttachedIndex (SAI) is only supported on vector columns; use a secondary index for non-vector columns | OK |
| index_custom_sasi | `CREATE CUSTOM INDEX t_opts_sasi ON ref.t_opts (v) USING 'org.apache.cassandra.index.sasi.SASIIndex'` | `0x2200` Non-supported custom class 'org.apache.cassandra.index.sasi.SASIIndex' provided | `0x2200` SASI indexes are disabled. Enable in cassandra.yaml to use. |
| mv | `CREATE MATERIALIZED VIEW ref.t_simple_by_v AS SELECT id, v FROM ref.t_simple WHERE v IS NOT NULL AND id IS NOT NULL PRIMARY KEY (v, id) WITH comment = 'mv'` | OK; warnings: "Creating a materialized view in a keyspaces that uses tablets requires the keyspace to remain RF-rack-valid while the materialized view exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | `0x2200` Materialized views are disabled. Enable in cassandra.yaml to use. |
| mv_clustering_order | `CREATE MATERIALIZED VIEW ref.t_all_by_txt AS SELECT a, b, c1, c2, txt FROM ref.t_all WHERE txt IS NOT NULL AND a IS NOT NULL AND b IS NOT NULL AND c1 IS NOT NULL AND c2 IS NOT NULL PRIMARY KEY (txt, a, b, c1, c2) WITH CLUSTERING ORDER BY (a DESC, b ASC, c1 ASC, c2 DESC)` | OK; warnings: "Creating a materialized view in a keyspaces that uses tablets requires the keyspace to remain RF-rack-valid while the materialized view exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | `0x2200` Materialized views are disabled. Enable in cassandra.yaml to use. |
| udf_java | `CREATE FUNCTION ref.plus1(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE java AS 'return x + 1;'` | `0x2200` User defined functions are disabled. Set enable_user_defined_functions and experimental_features:udf to enable them | `0x2200` User-defined functions are disabled in cassandra.yaml - set user_defined_functions_enabled=true to enable |
| udf_lua | `CREATE FUNCTION ref.plus1_lua(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x + 1'` | `0x2200` User defined functions are disabled. Set enable_user_defined_functions and experimental_features:udf to enable them | `0x2200` User-defined functions are disabled in cassandra.yaml - set user_defined_functions_enabled=true to enable |
| udf_js | `CREATE FUNCTION ref.plus1_js(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE javascript AS 'x + 1'` | `0x2200` User defined functions are disabled. Set enable_user_defined_functions and experimental_features:udf to enable them | `0x2200` User-defined functions are disabled in cassandra.yaml - set user_defined_functions_enabled=true to enable |
| udf_java_acc | `CREATE FUNCTION ref.acc(s int, x int) CALLED ON NULL INPUT RETURNS int LANGUAGE java AS 'return s + x;'` | `0x2200` User defined functions are disabled. Set enable_user_defined_functions and experimental_features:udf to enable them | `0x2200` User-defined functions are disabled in cassandra.yaml - set user_defined_functions_enabled=true to enable |
| uda_java | `CREATE AGGREGATE ref.sum1(int) SFUNC acc STYPE int INITCOND 0` | `0x2200` User defined functions are disabled. Set enable_user_defined_functions and experimental_features:udf to enable them | `0x2200` State function ref.acc(int, int) doesn't exist |
| udf_lua_acc | `CREATE FUNCTION ref.acc_lua(s int, x int) CALLED ON NULL INPUT RETURNS int LANGUAGE lua AS 'return s + x'` | `0x2200` User defined functions are disabled. Set enable_user_defined_functions and experimental_features:udf to enable them | `0x2200` User-defined functions are disabled in cassandra.yaml - set user_defined_functions_enabled=true to enable |
| uda_lua | `CREATE AGGREGATE ref.sum1_lua(int) SFUNC acc_lua STYPE int INITCOND 0` | `0x2200` User defined functions are disabled. Set enable_user_defined_functions and experimental_features:udf to enable them | `0x2200` State function ref.acc_lua(int, int) doesn't exist |
| vector_table | `CREATE TABLE ref.t_vec (id int PRIMARY KEY, emb vector<float, 3>, tags vector<text, 2>)` | OK | OK |
| vector_sai | `CREATE INDEX t_vec_emb_idx ON ref.t_vec (emb) USING 'sai' WITH OPTIONS = {'similarity_function': 'COSINE'}` | OK | OK; warnings: "SAI ANN indexes on vector columns are experimental and are not recommended for production use.\nThey don't yet support SELECT queries with:\n * Consistency level higher than ONE/LOCAL_ONE.\n * Paging.\n * No LIMIT clauses.\n * PER PARTITION LIMIT clauses.\n * GROUP BY clauses.\n * Aggregation functions.\n * Filters on columns without a SAI index." |
| vector_index_scylla | `CREATE CUSTOM INDEX t_vec_emb_vs ON ref.t_vec (emb) USING 'vector_index'` | OK | `0x2300` Unable to find custom indexer class 'vector_index' |
| masked_cassandra | `CREATE TABLE ref.t_mask (id int PRIMARY KEY, email text MASKED WITH mask_inner(1, 1), ssn text MASKED WITH DEFAULT)` | `0x2000` line 1:51 no viable alternative at input 'text' | `0x2200` Cannot mask columns because dynamic data masking is not enabled. You can enable it with the dynamic_data_masking_enabled property on cassandra.yaml |
| trigger | `CREATE TRIGGER trg ON ref.t_simple USING 'org.apache.cassandra.triggers.AuditTrigger'` | `0x2000` line 1:0 no viable alternative at input 'CREATE' | `0x2200` Trigger class 'org.apache.cassandra.triggers.AuditTrigger' couldn't be loaded |
| mv_vnodes_base | `CREATE TABLE ref_vnodes.base (id int PRIMARY KEY, v text)` | OK | OK |
| mv_vnodes_view | `CREATE MATERIALIZED VIEW ref_vnodes.base_by_v AS SELECT * FROM ref_vnodes.base WHERE v IS NOT NULL AND id IS NOT NULL PRIMARY KEY (v, id)` | OK | `0x2200` Materialized views are disabled. Enable in cassandra.yaml to use. |
| index_vnodes | `CREATE INDEX base_v_idx ON ref_vnodes.base (v)` | OK | OK |

The full `CREATE TABLE ref.t_all` statement:

```cql
CREATE TABLE ref.t_all (
    a int, b text, c1 timestamp, c2 uuid,
    s text STATIC,
    asc_ ascii, big bigint, bl blob, bo boolean, da date, de decimal, do double, du duration,
    fl float, ine inet, sm smallint, ti time, tu timeuuid, tin tinyint, vi varint, txt text, vc varchar,
    l list<int>, st set<text>, m map<text, int>,
    fl_l frozen<list<int>>, fl_s frozen<set<text>>, fl_m frozen<map<text, int>>,
    nested map<text, frozen<list<int>>>,
    addr address, faddr frozen<address>, addrs list<frozen<address>>, who frozen<person>,
    tup tuple<int, text, float>, ftup frozen<tuple<int, tuple<text, bigint>>>,
    PRIMARY KEY ((a, b), c1, c2)
 ) WITH CLUSTERING ORDER BY (c1 DESC, c2 ASC) AND comment = 'sample table'
```
### Rows, ScyllaDB 2026.3.3

#### `ref aggregates`

columns: keyspace_name, aggregate_name, argument_types, final_func, initcond, return_type, state_func, state_type — no rows

#### `ref column_masks`

`0x2200` unconfigured table column_masks

#### `ref columns`

```
keyspace_name | table_name | column_name | clustering_order | column_name_bytes | kind | position | type
ref | Quoted Idx_index | Clu Col | ASC | b'Clu Col' | clustering | 2 | text
ref | Quoted Idx_index | Key | ASC | b'Key' | clustering | 1 | int
ref | Quoted Idx_index | camelCase | NONE | b'camelCase' | partition_key | 0 | text
ref | Quoted Idx_index | idx_token | ASC | b'idx_token' | clustering | 0 | bigint
ref | QuotedTable | Clu Col | ASC | b'Clu Col' | clustering | 0 | text
ref | QuotedTable | Key | NONE | b'Key' | partition_key | 0 | int
ref | QuotedTable | camelCase | NONE | b'camelCase' | regular | -1 | text
ref | QuotedTable | select | NONE | b'select' | regular | -1 | int
ref | t_all | a | NONE | b'a' | partition_key | 0 | int
ref | t_all | addr | NONE | b'addr' | regular | -1 | address
ref | t_all | addrs | NONE | b'addrs' | regular | -1 | list<frozen<address>>
ref | t_all | asc_ | NONE | b'asc_' | regular | -1 | ascii
ref | t_all | b | NONE | b'b' | partition_key | 1 | text
ref | t_all | big | NONE | b'big' | regular | -1 | bigint
ref | t_all | bl | NONE | b'bl' | regular | -1 | blob
ref | t_all | bo | NONE | b'bo' | regular | -1 | boolean
ref | t_all | c1 | DESC | b'c1' | clustering | 0 | timestamp
ref | t_all | c2 | ASC | b'c2' | clustering | 1 | uuid
ref | t_all | da | NONE | b'da' | regular | -1 | date
ref | t_all | de | NONE | b'de' | regular | -1 | decimal
ref | t_all | do | NONE | b'do' | regular | -1 | double
ref | t_all | du | NONE | b'du' | regular | -1 | duration
ref | t_all | faddr | NONE | b'faddr' | regular | -1 | frozen<address>
ref | t_all | fl | NONE | b'fl' | regular | -1 | float
ref | t_all | fl_l | NONE | b'fl_l' | regular | -1 | frozen<list<int>>
ref | t_all | fl_m | NONE | b'fl_m' | regular | -1 | frozen<map<text, int>>
ref | t_all | fl_s | NONE | b'fl_s' | regular | -1 | frozen<set<text>>
ref | t_all | ftup | NONE | b'ftup' | regular | -1 | frozen<tuple<int, frozen<tuple<text, bigint>>>>
ref | t_all | ine | NONE | b'ine' | regular | -1 | inet
ref | t_all | l | NONE | b'l' | regular | -1 | list<int>
ref | t_all | m | NONE | b'm' | regular | -1 | map<text, int>
ref | t_all | nested | NONE | b'nested' | regular | -1 | map<text, frozen<list<int>>>
ref | t_all | s | NONE | b's' | static | -1 | text
ref | t_all | sm | NONE | b'sm' | regular | -1 | smallint
ref | t_all | st | NONE | b'st' | regular | -1 | set<text>
ref | t_all | ti | NONE | b'ti' | regular | -1 | time
ref | t_all | tin | NONE | b'tin' | regular | -1 | tinyint
ref | t_all | tu | NONE | b'tu' | regular | -1 | timeuuid
ref | t_all | tup | NONE | b'tup' | regular | -1 | frozen<tuple<int, text, float>>
ref | t_all | txt | NONE | b'txt' | regular | -1 | text
ref | t_all | vc | NONE | b'vc' | regular | -1 | text
ref | t_all | vi | NONE | b'vi' | regular | -1 | varint
ref | t_all | who | NONE | b'who' | regular | -1 | frozen<person>
ref | t_all_b_idx_index | a | ASC | b'a' | clustering | 1 | int
ref | t_all_b_idx_index | b | NONE | b'b' | partition_key | 0 | text
ref | t_all_b_idx_index | c1 | DESC | b'c1' | clustering | 2 | timestamp
ref | t_all_b_idx_index | c2 | ASC | b'c2' | clustering | 3 | uuid
ref | t_all_b_idx_index | idx_token | ASC | b'idx_token' | clustering | 0 | bigint
ref | t_all_by_txt | a | DESC | b'a' | clustering | 0 | int
ref | t_all_by_txt | b | ASC | b'b' | clustering | 1 | text
ref | t_all_by_txt | c1 | ASC | b'c1' | clustering | 2 | timestamp
ref | t_all_by_txt | c2 | DESC | b'c2' | clustering | 3 | uuid
ref | t_all_by_txt | txt | NONE | b'txt' | partition_key | 0 | text
ref | t_all_c2_idx_index | a | ASC | b'a' | clustering | 1 | int
ref | t_all_c2_idx_index | b | ASC | b'b' | clustering | 2 | text
ref | t_all_c2_idx_index | c1 | DESC | b'c1' | clustering | 3 | timestamp
ref | t_all_c2_idx_index | c2 | NONE | b'c2' | partition_key | 0 | uuid
ref | t_all_c2_idx_index | idx_token | ASC | b'idx_token' | clustering | 0 | bigint
ref | t_all_fl_l_idx_index | a | ASC | b'a' | clustering | 1 | int
ref | t_all_fl_l_idx_index | b | ASC | b'b' | clustering | 2 | text
ref | t_all_fl_l_idx_index | c1 | DESC | b'c1' | clustering | 3 | timestamp
ref | t_all_fl_l_idx_index | c2 | ASC | b'c2' | clustering | 4 | uuid
ref | t_all_fl_l_idx_index | fl_l | NONE | b'fl_l' | partition_key | 0 | frozen<list<int>>
ref | t_all_fl_l_idx_index | idx_token | ASC | b'idx_token' | clustering | 0 | bigint
ref | t_all_local_idx_index | a | NONE | b'a' | partition_key | 0 | int
ref | t_all_local_idx_index | b | NONE | b'b' | partition_key | 1 | text
ref | t_all_local_idx_index | c1 | DESC | b'c1' | clustering | 1 | timestamp
ref | t_all_local_idx_index | c2 | ASC | b'c2' | clustering | 2 | uuid
ref | t_all_local_idx_index | vi | ASC | b'vi' | clustering | 0 | varint
ref | t_all_m_entries_idx_index | a | ASC | b'a' | clustering | 1 | int
ref | t_all_m_entries_idx_index | b | ASC | b'b' | clustering | 2 | text
ref | t_all_m_entries_idx_index | c1 | DESC | b'c1' | clustering | 3 | timestamp
ref | t_all_m_entries_idx_index | c2 | ASC | b'c2' | clustering | 4 | uuid
ref | t_all_m_entries_idx_index | coll_value | NONE | b'coll_value' | partition_key | 0 | frozen<tuple<text, int>>
ref | t_all_m_entries_idx_index | idx_token | ASC | b'idx_token' | clustering | 0 | bigint
ref | t_all_m_keys_idx_index | a | ASC | b'a' | clustering | 1 | int
ref | t_all_m_keys_idx_index | b | ASC | b'b' | clustering | 2 | text
ref | t_all_m_keys_idx_index | c1 | DESC | b'c1' | clustering | 3 | timestamp
ref | t_all_m_keys_idx_index | c2 | ASC | b'c2' | clustering | 4 | uuid
ref | t_all_m_keys_idx_index | coll_value | NONE | b'coll_value' | partition_key | 0 | text
ref | t_all_m_keys_idx_index | idx_token | ASC | b'idx_token' | clustering | 0 | bigint
ref | t_all_s_idx_index | a | ASC | b'a' | clustering | 1 | int
ref | t_all_s_idx_index | b | ASC | b'b' | clustering | 2 | text
ref | t_all_s_idx_index | idx_token | ASC | b'idx_token' | clustering | 0 | bigint
ref | t_all_s_idx_index | s | NONE | b's' | partition_key | 0 | text
ref | t_all_st_idx_index | a | ASC | b'a' | clustering | 1 | int
ref | t_all_st_idx_index | b | ASC | b'b' | clustering | 2 | text
ref | t_all_st_idx_index | c1 | DESC | b'c1' | clustering | 3 | timestamp
ref | t_all_st_idx_index | c2 | ASC | b'c2' | clustering | 4 | uuid
ref | t_all_st_idx_index | coll_value | NONE | b'coll_value' | partition_key | 0 | text
ref | t_all_st_idx_index | idx_token | ASC | b'idx_token' | clustering | 0 | bigint
ref | t_all_txt_idx_index | a | ASC | b'a' | clustering | 1 | int
ref | t_all_txt_idx_index | b | ASC | b'b' | clustering | 2 | text
ref | t_all_txt_idx_index | c1 | DESC | b'c1' | clustering | 3 | timestamp
ref | t_all_txt_idx_index | c2 | ASC | b'c2' | clustering | 4 | uuid
ref | t_all_txt_idx_index | idx_token | ASC | b'idx_token' | clustering | 0 | bigint
ref | t_all_txt_idx_index | txt | NONE | b'txt' | partition_key | 0 | text
ref | t_cdc | id | NONE | b'id' | partition_key | 0 | int
ref | t_cdc | v | NONE | b'v' | regular | -1 | int
ref | t_cdc_scylla_cdc_log | cdc$batch_seq_no | ASC | b'cdc$batch_seq_no' | clustering | 1 | int
ref | t_cdc_scylla_cdc_log | cdc$deleted_v | NONE | b'cdc$deleted_v' | regular | -1 | boolean
ref | t_cdc_scylla_cdc_log | cdc$end_of_batch | NONE | b'cdc$end_of_batch' | regular | -1 | boolean
ref | t_cdc_scylla_cdc_log | cdc$operation | NONE | b'cdc$operation' | regular | -1 | tinyint
ref | t_cdc_scylla_cdc_log | cdc$stream_id | NONE | b'cdc$stream_id' | partition_key | 0 | blob
ref | t_cdc_scylla_cdc_log | cdc$time | ASC | b'cdc$time' | clustering | 0 | timeuuid
ref | t_cdc_scylla_cdc_log | cdc$ttl | NONE | b'cdc$ttl' | regular | -1 | bigint
ref | t_cdc_scylla_cdc_log | id | NONE | b'id' | regular | -1 | int
ref | t_cdc_scylla_cdc_log | v | NONE | b'v' | regular | -1 | int
ref | t_comp2 | id | NONE | b'id' | partition_key | 0 | int
ref | t_counter | c | NONE | b'c' | regular | -1 | counter
ref | t_counter | c2 | NONE | b'c2' | regular | -1 | counter
ref | t_counter | id | NONE | b'id' | partition_key | 0 | int
ref | t_counter | k | ASC | b'k' | clustering | 0 | text
ref | t_ics | id | NONE | b'id' | partition_key | 0 | int
ref | t_id | id | NONE | b'id' | partition_key | 0 | int
ref | t_opts | id | NONE | b'id' | partition_key | 0 | int
ref | t_opts | v | NONE | b'v' | regular | -1 | text
ref | t_pgs | id | NONE | b'id' | partition_key | 0 | int
ref | t_simple | id | NONE | b'id' | partition_key | 0 | int
ref | t_simple | v | NONE | b'v' | regular | -1 | text
ref | t_simple_by_v | id | ASC | b'id' | clustering | 0 | int
ref | t_simple_by_v | v | NONE | b'v' | partition_key | 0 | text
ref | t_tgc | id | NONE | b'id' | partition_key | 0 | int
ref | t_twcs | at | ASC | b'at' | clustering | 0 | timestamp
ref | t_twcs | id | NONE | b'id' | partition_key | 0 | int
ref | t_twcs | v | NONE | b'v' | regular | -1 | text
ref | t_vec | emb | NONE | b'emb' | regular | -1 | vector<float, 3>
ref | t_vec | id | NONE | b'id' | partition_key | 0 | int
ref | t_vec | tags | NONE | b'tags' | regular | -1 | vector<text, 2>
ref | t_vec_scylla_cdc_log | cdc$batch_seq_no | ASC | b'cdc$batch_seq_no' | clustering | 1 | int
ref | t_vec_scylla_cdc_log | cdc$deleted_emb | NONE | b'cdc$deleted_emb' | regular | -1 | boolean
ref | t_vec_scylla_cdc_log | cdc$deleted_tags | NONE | b'cdc$deleted_tags' | regular | -1 | boolean
ref | t_vec_scylla_cdc_log | cdc$end_of_batch | NONE | b'cdc$end_of_batch' | regular | -1 | boolean
ref | t_vec_scylla_cdc_log | cdc$operation | NONE | b'cdc$operation' | regular | -1 | tinyint
ref | t_vec_scylla_cdc_log | cdc$stream_id | NONE | b'cdc$stream_id' | partition_key | 0 | blob
ref | t_vec_scylla_cdc_log | cdc$time | ASC | b'cdc$time' | clustering | 0 | timeuuid
ref | t_vec_scylla_cdc_log | cdc$ttl | NONE | b'cdc$ttl' | regular | -1 | bigint
ref | t_vec_scylla_cdc_log | emb | NONE | b'emb' | regular | -1 | vector<float, 3>
ref | t_vec_scylla_cdc_log | id | NONE | b'id' | regular | -1 | int
ref | t_vec_scylla_cdc_log | tags | NONE | b'tags' | regular | -1 | vector<text, 2>
```

#### `ref computed_columns`

```
keyspace_name | table_name | column_name | computation
ref | Quoted Idx_index | idx_token | b'{"type":"token_v2"}'
ref | t_all_b_idx_index | idx_token | b'{"type":"token_v2"}'
ref | t_all_c2_idx_index | idx_token | b'{"type":"token_v2"}'
ref | t_all_fl_l_idx_index | idx_token | b'{"type":"token_v2"}'
ref | t_all_m_entries_idx_index | coll_value | b'{"type":"collection_entries","collection_name":"m"}'
ref | t_all_m_entries_idx_index | idx_token | b'{"type":"token_v2"}'
ref | t_all_m_keys_idx_index | coll_value | b'{"type":"collection_keys","collection_name":"m"}'
ref | t_all_m_keys_idx_index | idx_token | b'{"type":"token_v2"}'
ref | t_all_s_idx_index | idx_token | b'{"type":"token_v2"}'
ref | t_all_st_idx_index | coll_value | b'{"type":"collection_keys","collection_name":"st"}'
ref | t_all_st_idx_index | idx_token | b'{"type":"token_v2"}'
ref | t_all_txt_idx_index | idx_token | b'{"type":"token_v2"}'
```

#### `ref dropped_columns`

error: `None` Failed decoding result column "dropped_time" of type timestamp: date value out of range

#### `ref dropped_columns raw`

```
keyspace_name | table_name | column_name | dropped_time_raw | type
ref | t_simple | dropped_col | 1792521357407993 | int
ref | t_simple | dropped_list | 1792521357525283 | list<text>
```

#### `ref functions`

columns: keyspace_name, function_name, argument_types, argument_names, body, called_on_null_input, language, return_type — no rows

#### `ref indexes`

```
keyspace_name | table_name | index_name | kind | options
ref | QuotedTable | Quoted Idx | COMPOSITES | {'target': '"camelCase"'}
ref | t_all | t_all_b_idx | COMPOSITES | {'target': 'b'}
ref | t_all | t_all_c2_idx | COMPOSITES | {'target': 'c2'}
ref | t_all | t_all_fl_l_idx | COMPOSITES | {'target': 'fl_l'}
ref | t_all | t_all_local_idx | COMPOSITES | {'target': '{"pk":["a","b"],"ck":["vi"]}'}
ref | t_all | t_all_m_entries_idx | COMPOSITES | {'target': 'entries(m)'}
ref | t_all | t_all_m_keys_idx | COMPOSITES | {'target': 'keys(m)'}
ref | t_all | t_all_s_idx | COMPOSITES | {'target': 's'}
ref | t_all | t_all_st_idx | COMPOSITES | {'target': 'keys(st)'}
ref | t_all | t_all_txt_idx | COMPOSITES | {'target': 'txt'}
ref | t_vec | t_vec_emb_idx | CUSTOM | {'index_version': '1848d810-ccb5-11f1-974f-a4b4b432c093', 'class_name': 'vector_index', 'target': 'emb', 'similarity_function': 'COSINE'}
ref | t_vec | t_vec_emb_vs | CUSTOM | {'target': 'emb', 'index_version': '185b4ea0-ccb5-11f1-974f-a4b4b432c093', 'class_name': 'vector_index'}
```

#### `ref keyspaces`

```
keyspace_name | durable_writes | next_replication | replication | replication_v2
ref | True | None | {'class': 'org.apache.cassandra.locator.NetworkTopologyStrategy', 'datacenter1': '1'} | None
```

#### `ref scylla_keyspaces`

```
keyspace_name | consistency | initial_tablets | storage_options | storage_type
ref | None | 0 | None | None
```

#### `ref scylla_tables`

```
keyspace_name | table_name | cdc | committed_by_group0 | in_memory | large_data_guardrails_enabled | partitioner | storage_engine | tablets | version
ref | Quoted Idx_index | None | True | None | None | None | None | None | <uuid>
ref | QuotedTable | None | True | None | None | None | None | None | <uuid>
ref | t_all | None | True | None | None | None | None | None | <uuid>
ref | t_all_b_idx_index | None | True | None | None | None | None | None | <uuid>
ref | t_all_by_txt | None | True | None | None | None | None | None | <uuid>
ref | t_all_c2_idx_index | None | True | None | None | None | None | None | <uuid>
ref | t_all_fl_l_idx_index | None | True | None | None | None | None | None | <uuid>
ref | t_all_local_idx_index | None | True | None | None | None | None | None | <uuid>
ref | t_all_m_entries_idx_index | None | True | None | None | None | None | None | <uuid>
ref | t_all_m_keys_idx_index | None | True | None | None | None | None | None | <uuid>
ref | t_all_s_idx_index | None | True | None | None | None | None | None | <uuid>
ref | t_all_st_idx_index | None | True | None | None | None | None | None | <uuid>
ref | t_all_txt_idx_index | None | True | None | None | None | None | None | <uuid>
ref | t_cdc | None | True | None | None | None | None | None | <uuid>
ref | t_cdc_scylla_cdc_log | None | True | None | None | com.scylladb.dht.CDCPartitioner | None | None | <uuid>
ref | t_comp2 | None | True | None | None | None | None | None | <uuid>
ref | t_counter | None | True | None | None | None | None | None | <uuid>
ref | t_ics | None | True | None | None | None | None | None | <uuid>
ref | t_id | None | True | None | None | None | None | None | <uuid>
ref | t_opts | None | True | None | None | None | None | None | <uuid>
ref | t_pgs | None | True | None | None | None | None | None | <uuid>
ref | t_simple | None | True | None | None | None | None | None | <uuid>
ref | t_simple_by_v | None | True | None | None | None | None | None | <uuid>
ref | t_tgc | None | True | None | None | None | None | None | <uuid>
ref | t_twcs | None | True | None | None | None | None | None | <uuid>
ref | t_vec | None | True | None | None | None | None | None | <uuid>
ref | t_vec_scylla_cdc_log | None | True | None | None | com.scylladb.dht.CDCPartitioner | None | None | <uuid>
```

#### `ref tables`

```
keyspace_name | table_name | bloom_filter_fp_chance | caching | comment | compaction | compression | crc_check_chance | dclocal_read_repair_chance | default_time_to_live | extensions | flags | gc_grace_seconds | id | max_index_interval | memtable_flush_period_in_ms | min_index_interval | read_repair_chance | speculative_retry
ref | QuotedTable | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | SortedSet(['compound']) | 864000 | <uuid> | 2048 | 0 | 128 | None | 99.0PERCENTILE
ref | t_all | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} | sample table | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | SortedSet(['compound']) | 864000 | <uuid> | 2048 | 0 | 128 | None | 99.0PERCENTILE
ref | t_cdc | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'cdc': b'\x05\x00\x00\x00\x05\x00\x00\x00delta\x04\x00\x00\x00full\x07\x00\x00\x00enabled\x04\x00\x00\x00true\t\x00\x00\x00postimage\x05\x00\x00\x00false\x08\x00\x00\x00preimage\x05\x00\x00\x00false\x03\x00\x00\x00ttl\x05\x00\x00\x0086400', 'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | SortedSet(['compound']) | 864000 | <uuid> | 2048 | 0 | 128 | None | 99.0PERCENTILE
ref | t_cdc_scylla_cdc_log | 0.01 | {'enabled': 'false', 'keys': 'NONE', 'rows_per_partition': 'NONE'} | CDC log for ref.t_cdc | {'class': 'TimeWindowCompactionStrategy', 'compaction_window_size': '60', 'compaction_window_unit': 'MINUTES', 'expired_sstable_check_frequency_seconds': '1800'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x07\x00\x00\x00timeout\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | SortedSet(['compound']) | 0 | <uuid> | 2048 | 0 | 128 | None | 99.0PERCENTILE
ref | t_comp2 | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'chunk_length_in_kb': '64', 'sstable_compression': 'org.apache.cassandra.io.compress.DeflateCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | SortedSet(['compound']) | 864000 | <uuid> | 2048 | 0 | 128 | None | 99.0PERCENTILE
ref | t_counter | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | SortedSet(['compound', 'counter']) | 864000 | <uuid> | 2048 | 0 | 128 | None | 99.0PERCENTILE
ref | t_ics | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | SortedSet(['compound']) | 864000 | <uuid> | 2048 | 0 | 128 | None | 99.0PERCENTILE
ref | t_id | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | SortedSet(['compound']) | 864000 | 5a1c395e-b41f-11e5-9f22-ba0be0483c18 | 2048 | 0 | 128 | None | 99.0PERCENTILE
ref | t_opts | 0.1 | {'keys': 'ALL', 'rows_per_partition': '10'} |  | {'class': 'LeveledCompactionStrategy', 'sstable_size_in_mb': '64'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 3600 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | SortedSet(['compound']) | 3600 | <uuid> | 1024 | 60000 | 64 | None | 99.0PERCENTILE
ref | t_pgs | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'paxos_grace_seconds': b'd\x00\x00\x00', 'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | SortedSet(['compound']) | 864000 | <uuid> | 2048 | 0 | 128 | None | 99.0PERCENTILE
ref | t_simple | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} | altered | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | SortedSet(['compound']) | 7200 | <uuid> | 2048 | 0 | 128 | None | 99.0PERCENTILE
ref | t_tgc | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x07\x00\x00\x00timeout\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | SortedSet(['compound']) | 864000 | <uuid> | 2048 | 0 | 128 | None | 99.0PERCENTILE
ref | t_twcs | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'TimeWindowCompactionStrategy', 'compaction_window_size': '6', 'compaction_window_unit': 'HOURS'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | SortedSet(['compound']) | 864000 | <uuid> | 2048 | 0 | 128 | None | 99.0PERCENTILE
ref | t_vec | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | SortedSet(['compound']) | 864000 | <uuid> | 2048 | 0 | 128 | None | 99.0PERCENTILE
ref | t_vec_scylla_cdc_log | 0.01 | {'enabled': 'false', 'keys': 'NONE', 'rows_per_partition': 'NONE'} | CDC log for ref.t_vec | {'class': 'TimeWindowCompactionStrategy', 'compaction_window_size': '60', 'compaction_window_unit': 'MINUTES', 'expired_sstable_check_frequency_seconds': '1800'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x07\x00\x00\x00timeout\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | SortedSet(['compound']) | 0 | <uuid> | 2048 | 0 | 128 | None | 99.0PERCENTILE
```

#### `ref triggers`

columns: keyspace_name, table_name, trigger_name, options — no rows

#### `ref types`

```
keyspace_name | type_name | field_names | field_types
ref | address | ['street', 'zip', 'tags'] | ['text', 'int', 'frozen<set<text>>']
ref | person | ['name', 'home', 'Quoted Field'] | ['text', 'frozen<address>', 'int']
```

#### `ref view_virtual_columns`

```
keyspace_name | table_name | column_name | clustering_order | column_name_bytes | kind | position | type
ref | t_all_b_idx_index | addr | NONE | b'addr' | regular | -1 | address
ref | t_all_b_idx_index | addrs | NONE | b'addrs' | regular | -1 | list<empty>
ref | t_all_b_idx_index | asc_ | NONE | b'asc_' | regular | -1 | empty
ref | t_all_b_idx_index | big | NONE | b'big' | regular | -1 | empty
ref | t_all_b_idx_index | bl | NONE | b'bl' | regular | -1 | empty
ref | t_all_b_idx_index | bo | NONE | b'bo' | regular | -1 | empty
ref | t_all_b_idx_index | da | NONE | b'da' | regular | -1 | empty
ref | t_all_b_idx_index | de | NONE | b'de' | regular | -1 | empty
ref | t_all_b_idx_index | do | NONE | b'do' | regular | -1 | empty
ref | t_all_b_idx_index | du | NONE | b'du' | regular | -1 | empty
ref | t_all_b_idx_index | faddr | NONE | b'faddr' | regular | -1 | empty
ref | t_all_b_idx_index | fl | NONE | b'fl' | regular | -1 | empty
ref | t_all_b_idx_index | fl_l | NONE | b'fl_l' | regular | -1 | empty
ref | t_all_b_idx_index | fl_m | NONE | b'fl_m' | regular | -1 | empty
ref | t_all_b_idx_index | fl_s | NONE | b'fl_s' | regular | -1 | empty
ref | t_all_b_idx_index | ftup | NONE | b'ftup' | regular | -1 | empty
ref | t_all_b_idx_index | ine | NONE | b'ine' | regular | -1 | empty
ref | t_all_b_idx_index | l | NONE | b'l' | regular | -1 | list<empty>
ref | t_all_b_idx_index | m | NONE | b'm' | regular | -1 | map<text, empty>
ref | t_all_b_idx_index | nested | NONE | b'nested' | regular | -1 | map<text, empty>
ref | t_all_b_idx_index | sm | NONE | b'sm' | regular | -1 | empty
ref | t_all_b_idx_index | st | NONE | b'st' | regular | -1 | set<text>
ref | t_all_b_idx_index | ti | NONE | b'ti' | regular | -1 | empty
ref | t_all_b_idx_index | tin | NONE | b'tin' | regular | -1 | empty
ref | t_all_b_idx_index | tu | NONE | b'tu' | regular | -1 | empty
ref | t_all_b_idx_index | tup | NONE | b'tup' | regular | -1 | empty
ref | t_all_b_idx_index | txt | NONE | b'txt' | regular | -1 | empty
ref | t_all_b_idx_index | vc | NONE | b'vc' | regular | -1 | empty
ref | t_all_b_idx_index | vi | NONE | b'vi' | regular | -1 | empty
ref | t_all_b_idx_index | who | NONE | b'who' | regular | -1 | empty
ref | t_all_c2_idx_index | addr | NONE | b'addr' | regular | -1 | address
ref | t_all_c2_idx_index | addrs | NONE | b'addrs' | regular | -1 | list<empty>
ref | t_all_c2_idx_index | asc_ | NONE | b'asc_' | regular | -1 | empty
ref | t_all_c2_idx_index | big | NONE | b'big' | regular | -1 | empty
ref | t_all_c2_idx_index | bl | NONE | b'bl' | regular | -1 | empty
ref | t_all_c2_idx_index | bo | NONE | b'bo' | regular | -1 | empty
ref | t_all_c2_idx_index | da | NONE | b'da' | regular | -1 | empty
ref | t_all_c2_idx_index | de | NONE | b'de' | regular | -1 | empty
ref | t_all_c2_idx_index | do | NONE | b'do' | regular | -1 | empty
ref | t_all_c2_idx_index | du | NONE | b'du' | regular | -1 | empty
ref | t_all_c2_idx_index | faddr | NONE | b'faddr' | regular | -1 | empty
ref | t_all_c2_idx_index | fl | NONE | b'fl' | regular | -1 | empty
ref | t_all_c2_idx_index | fl_l | NONE | b'fl_l' | regular | -1 | empty
ref | t_all_c2_idx_index | fl_m | NONE | b'fl_m' | regular | -1 | empty
ref | t_all_c2_idx_index | fl_s | NONE | b'fl_s' | regular | -1 | empty
ref | t_all_c2_idx_index | ftup | NONE | b'ftup' | regular | -1 | empty
ref | t_all_c2_idx_index | ine | NONE | b'ine' | regular | -1 | empty
ref | t_all_c2_idx_index | l | NONE | b'l' | regular | -1 | list<empty>
ref | t_all_c2_idx_index | m | NONE | b'm' | regular | -1 | map<text, empty>
ref | t_all_c2_idx_index | nested | NONE | b'nested' | regular | -1 | map<text, empty>
ref | t_all_c2_idx_index | sm | NONE | b'sm' | regular | -1 | empty
ref | t_all_c2_idx_index | st | NONE | b'st' | regular | -1 | set<text>
ref | t_all_c2_idx_index | ti | NONE | b'ti' | regular | -1 | empty
ref | t_all_c2_idx_index | tin | NONE | b'tin' | regular | -1 | empty
ref | t_all_c2_idx_index | tu | NONE | b'tu' | regular | -1 | empty
ref | t_all_c2_idx_index | tup | NONE | b'tup' | regular | -1 | empty
ref | t_all_c2_idx_index | txt | NONE | b'txt' | regular | -1 | empty
ref | t_all_c2_idx_index | vc | NONE | b'vc' | regular | -1 | empty
ref | t_all_c2_idx_index | vi | NONE | b'vi' | regular | -1 | empty
ref | t_all_c2_idx_index | who | NONE | b'who' | regular | -1 | empty
```

#### `ref views`

```
keyspace_name | view_name | base_table_id | base_table_name | bloom_filter_fp_chance | caching | comment | compaction | compression | crc_check_chance | dclocal_read_repair_chance | default_time_to_live | extensions | gc_grace_seconds | id | include_all_columns | max_index_interval | memtable_flush_period_in_ms | min_index_interval | read_repair_chance | speculative_retry | where_clause
ref | Quoted Idx_index | 16b084d0-ccb5-11f1-974f-a4b4b432c093 | QuotedTable | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | False | 2048 | 0 | 128 | None | 99.0PERCENTILE | "camelCase" IS NOT NULL
ref | t_all_b_idx_index | 16ac6620-ccb5-11f1-974f-a4b4b432c093 | t_all | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | False | 2048 | 0 | 128 | None | 99.0PERCENTILE | b IS NOT NULL
ref | t_all_by_txt | 16ac6620-ccb5-11f1-974f-a4b4b432c093 | t_all | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | False | 2048 | 0 | 128 | None | 99.0PERCENTILE | txt IS NOT null AND a IS NOT null AND b IS NOT null AND c1 IS NOT null AND c2 IS NOT null
ref | t_all_c2_idx_index | 16ac6620-ccb5-11f1-974f-a4b4b432c093 | t_all | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | False | 2048 | 0 | 128 | None | 99.0PERCENTILE | c2 IS NOT NULL
ref | t_all_fl_l_idx_index | 16ac6620-ccb5-11f1-974f-a4b4b432c093 | t_all | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | False | 2048 | 0 | 128 | None | 99.0PERCENTILE | fl_l IS NOT NULL
ref | t_all_local_idx_index | 16ac6620-ccb5-11f1-974f-a4b4b432c093 | t_all | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'scylla_tags': b'\x01\x00\x00\x00\x1f\x00\x00\x00system:synchronous_view_updates\x04\x00\x00\x00true', 'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x07\x00\x00\x00timeout\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | False | 2048 | 0 | 128 | None | 99.0PERCENTILE | vi IS NOT NULL
ref | t_all_m_entries_idx_index | 16ac6620-ccb5-11f1-974f-a4b4b432c093 | t_all | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | False | 2048 | 0 | 128 | None | 99.0PERCENTILE | 
ref | t_all_m_keys_idx_index | 16ac6620-ccb5-11f1-974f-a4b4b432c093 | t_all | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | False | 2048 | 0 | 128 | None | 99.0PERCENTILE | 
ref | t_all_s_idx_index | 16ac6620-ccb5-11f1-974f-a4b4b432c093 | t_all | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | False | 2048 | 0 | 128 | None | 99.0PERCENTILE | s IS NOT NULL
ref | t_all_st_idx_index | 16ac6620-ccb5-11f1-974f-a4b4b432c093 | t_all | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | False | 2048 | 0 | 128 | None | 99.0PERCENTILE | 
ref | t_all_txt_idx_index | 16ac6620-ccb5-11f1-974f-a4b4b432c093 | t_all | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | False | 2048 | 0 | 128 | None | 99.0PERCENTILE | txt IS NOT NULL
ref | t_simple_by_v | 16a84770-ccb5-11f1-974f-a4b4b432c093 | t_simple | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} | mv | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | False | 2048 | 0 | 128 | None | 99.0PERCENTILE | v IS NOT null AND id IS NOT null
```

#### `ref_simple aggregates`

columns: keyspace_name, aggregate_name, argument_types, final_func, initcond, return_type, state_func, state_type — no rows

#### `ref_simple column_masks`

`0x2200` unconfigured table column_masks

#### `ref_simple columns`

columns: keyspace_name, table_name, column_name, clustering_order, column_name_bytes, kind, position, type — no rows

#### `ref_simple computed_columns`

columns: keyspace_name, table_name, column_name, computation — no rows

#### `ref_simple dropped_columns`

columns: keyspace_name, table_name, column_name, dropped_time, type — no rows

#### `ref_simple dropped_columns raw`

columns: keyspace_name, table_name, column_name, dropped_time_raw, type — no rows

#### `ref_simple functions`

columns: keyspace_name, function_name, argument_types, argument_names, body, called_on_null_input, language, return_type — no rows

#### `ref_simple indexes`

columns: keyspace_name, table_name, index_name, kind, options — no rows

#### `ref_simple keyspaces`

```
keyspace_name | durable_writes | next_replication | replication | replication_v2
ref_simple | False | None | {'class': 'org.apache.cassandra.locator.SimpleStrategy', 'replication_factor': '1'} | None
```

#### `ref_simple scylla_keyspaces`

columns: keyspace_name, consistency, initial_tablets, storage_options, storage_type — no rows

#### `ref_simple scylla_tables`

columns: keyspace_name, table_name, cdc, committed_by_group0, in_memory, large_data_guardrails_enabled, partitioner, storage_engine, tablets, version — no rows

#### `ref_simple tables`

columns: keyspace_name, table_name, bloom_filter_fp_chance, caching, comment, compaction, compression, crc_check_chance, dclocal_read_repair_chance, default_time_to_live, extensions, flags, gc_grace_seconds, id, max_index_interval, memtable_flush_period_in_ms, min_index_interval, read_repair_chance, speculative_retry — no rows

#### `ref_simple triggers`

columns: keyspace_name, table_name, trigger_name, options — no rows

#### `ref_simple types`

columns: keyspace_name, type_name, field_names, field_types — no rows

#### `ref_simple view_virtual_columns`

columns: keyspace_name, table_name, column_name, clustering_order, column_name_bytes, kind, position, type — no rows

#### `ref_simple views`

columns: keyspace_name, view_name, base_table_id, base_table_name, bloom_filter_fp_chance, caching, comment, compaction, compression, crc_check_chance, dclocal_read_repair_chance, default_time_to_live, extensions, gc_grace_seconds, id, include_all_columns, max_index_interval, memtable_flush_period_in_ms, min_index_interval, read_repair_chance, speculative_retry, where_clause — no rows

#### `ref_vnodes aggregates`

columns: keyspace_name, aggregate_name, argument_types, final_func, initcond, return_type, state_func, state_type — no rows

#### `ref_vnodes column_masks`

`0x2200` unconfigured table column_masks

#### `ref_vnodes columns`

```
keyspace_name | table_name | column_name | clustering_order | column_name_bytes | kind | position | type
ref_vnodes | base | id | NONE | b'id' | partition_key | 0 | int
ref_vnodes | base | v | NONE | b'v' | regular | -1 | text
ref_vnodes | base_by_v | id | ASC | b'id' | clustering | 0 | int
ref_vnodes | base_by_v | v | NONE | b'v' | partition_key | 0 | text
ref_vnodes | base_v_idx_index | id | ASC | b'id' | clustering | 1 | int
ref_vnodes | base_v_idx_index | idx_token | ASC | b'idx_token' | clustering | 0 | bigint
ref_vnodes | base_v_idx_index | v | NONE | b'v' | partition_key | 0 | text
```

#### `ref_vnodes computed_columns`

```
keyspace_name | table_name | column_name | computation
ref_vnodes | base_v_idx_index | idx_token | b'{"type":"token_v2"}'
```

#### `ref_vnodes dropped_columns`

columns: keyspace_name, table_name, column_name, dropped_time, type — no rows

#### `ref_vnodes dropped_columns raw`

columns: keyspace_name, table_name, column_name, dropped_time_raw, type — no rows

#### `ref_vnodes functions`

columns: keyspace_name, function_name, argument_types, argument_names, body, called_on_null_input, language, return_type — no rows

#### `ref_vnodes indexes`

```
keyspace_name | table_name | index_name | kind | options
ref_vnodes | base | base_v_idx | COMPOSITES | {'target': 'v'}
```

#### `ref_vnodes keyspaces`

```
keyspace_name | durable_writes | next_replication | replication | replication_v2
ref_vnodes | True | None | {'class': 'org.apache.cassandra.locator.NetworkTopologyStrategy', 'datacenter1': '1'} | None
```

#### `ref_vnodes scylla_keyspaces`

columns: keyspace_name, consistency, initial_tablets, storage_options, storage_type — no rows

#### `ref_vnodes scylla_tables`

```
keyspace_name | table_name | cdc | committed_by_group0 | in_memory | large_data_guardrails_enabled | partitioner | storage_engine | tablets | version
ref_vnodes | base | None | True | None | None | None | None | None | <uuid>
ref_vnodes | base_by_v | None | True | None | None | None | None | None | <uuid>
ref_vnodes | base_v_idx_index | None | True | None | None | None | None | None | <uuid>
```

#### `ref_vnodes tables`

```
keyspace_name | table_name | bloom_filter_fp_chance | caching | comment | compaction | compression | crc_check_chance | dclocal_read_repair_chance | default_time_to_live | extensions | flags | gc_grace_seconds | id | max_index_interval | memtable_flush_period_in_ms | min_index_interval | read_repair_chance | speculative_retry
ref_vnodes | base | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | SortedSet(['compound']) | 864000 | <uuid> | 2048 | 0 | 128 | None | 99.0PERCENTILE
```

#### `ref_vnodes triggers`

columns: keyspace_name, table_name, trigger_name, options — no rows

#### `ref_vnodes types`

columns: keyspace_name, type_name, field_names, field_types — no rows

#### `ref_vnodes view_virtual_columns`

columns: keyspace_name, table_name, column_name, clustering_order, column_name_bytes, kind, position, type — no rows

#### `ref_vnodes views`

```
keyspace_name | view_name | base_table_id | base_table_name | bloom_filter_fp_chance | caching | comment | compaction | compression | crc_check_chance | dclocal_read_repair_chance | default_time_to_live | extensions | gc_grace_seconds | id | include_all_columns | max_index_interval | memtable_flush_period_in_ms | min_index_interval | read_repair_chance | speculative_retry | where_clause
ref_vnodes | base_by_v | 186a69d0-ccb5-11f1-974f-a4b4b432c093 | base | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | True | 2048 | 0 | 128 | None | 99.0PERCENTILE | v IS NOT null AND id IS NOT null
ref_vnodes | base_v_idx_index | 186a69d0-ccb5-11f1-974f-a4b4b432c093 | base | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | False | 2048 | 0 | 128 | None | 99.0PERCENTILE | v IS NOT NULL
```

Note: `dropped_columns raw` re-reads `dropped_time` as `blobAsBigint(timestampAsBlob(dropped_time))`.

### Rows, Cassandra 5.0.9

#### `ref aggregates`

columns: keyspace_name, aggregate_name, argument_types, final_func, initcond, return_type, state_func, state_type — no rows

#### `ref column_masks`

columns: keyspace_name, table_name, column_name, function_argument_nulls, function_argument_types, function_argument_values, function_keyspace, function_name — no rows

#### `ref columns`

```
keyspace_name | table_name | column_name | clustering_order | column_name_bytes | kind | position | type
ref | QuotedTable | Clu Col | asc | b'Clu Col' | clustering | 0 | text
ref | QuotedTable | Key | none | b'Key' | partition_key | 0 | int
ref | QuotedTable | camelCase | none | b'camelCase' | regular | -1 | text
ref | QuotedTable | select | none | b'select' | regular | -1 | int
ref | t_all | a | none | b'a' | partition_key | 0 | int
ref | t_all | addr | none | b'addr' | regular | -1 | address
ref | t_all | addrs | none | b'addrs' | regular | -1 | list<frozen<address>>
ref | t_all | asc_ | none | b'asc_' | regular | -1 | ascii
ref | t_all | b | none | b'b' | partition_key | 1 | text
ref | t_all | big | none | b'big' | regular | -1 | bigint
ref | t_all | bl | none | b'bl' | regular | -1 | blob
ref | t_all | bo | none | b'bo' | regular | -1 | boolean
ref | t_all | c1 | desc | b'c1' | clustering | 0 | timestamp
ref | t_all | c2 | asc | b'c2' | clustering | 1 | uuid
ref | t_all | da | none | b'da' | regular | -1 | date
ref | t_all | de | none | b'de' | regular | -1 | decimal
ref | t_all | do | none | b'do' | regular | -1 | double
ref | t_all | du | none | b'du' | regular | -1 | duration
ref | t_all | faddr | none | b'faddr' | regular | -1 | frozen<address>
ref | t_all | fl | none | b'fl' | regular | -1 | float
ref | t_all | fl_l | none | b'fl_l' | regular | -1 | frozen<list<int>>
ref | t_all | fl_m | none | b'fl_m' | regular | -1 | frozen<map<text, int>>
ref | t_all | fl_s | none | b'fl_s' | regular | -1 | frozen<set<text>>
ref | t_all | ftup | none | b'ftup' | regular | -1 | frozen<tuple<int, frozen<tuple<text, bigint>>>>
ref | t_all | ine | none | b'ine' | regular | -1 | inet
ref | t_all | l | none | b'l' | regular | -1 | list<int>
ref | t_all | m | none | b'm' | regular | -1 | map<text, int>
ref | t_all | nested | none | b'nested' | regular | -1 | map<text, frozen<list<int>>>
ref | t_all | s | none | b's' | static | -1 | text
ref | t_all | sm | none | b'sm' | regular | -1 | smallint
ref | t_all | st | none | b'st' | regular | -1 | set<text>
ref | t_all | ti | none | b'ti' | regular | -1 | time
ref | t_all | tin | none | b'tin' | regular | -1 | tinyint
ref | t_all | tu | none | b'tu' | regular | -1 | timeuuid
ref | t_all | tup | none | b'tup' | regular | -1 | frozen<tuple<int, text, float>>
ref | t_all | txt | none | b'txt' | regular | -1 | text
ref | t_all | vc | none | b'vc' | regular | -1 | text
ref | t_all | vi | none | b'vi' | regular | -1 | varint
ref | t_all | who | none | b'who' | regular | -1 | frozen<person>
ref | t_cdc2 | id | none | b'id' | partition_key | 0 | int
ref | t_cdc2 | v | none | b'v' | regular | -1 | int
ref | t_comp1 | id | none | b'id' | partition_key | 0 | int
ref | t_comp3 | id | none | b'id' | partition_key | 0 | int
ref | t_compact | column1 | asc | b'column1' | clustering | 0 | text
ref | t_compact | id | none | b'id' | partition_key | 0 | int
ref | t_compact | v | none | b'v' | static | -1 | int
ref | t_compact | value | none | b'value' | regular | -1 | blob
ref | t_compact2 | ck | asc | b'ck' | clustering | 0 | int
ref | t_compact2 | id | none | b'id' | partition_key | 0 | int
ref | t_compact2 | v | none | b'v' | regular | -1 | int
ref | t_counter | c | none | b'c' | regular | -1 | counter
ref | t_counter | c2 | none | b'c2' | regular | -1 | counter
ref | t_counter | id | none | b'id' | partition_key | 0 | int
ref | t_counter | k | asc | b'k' | clustering | 0 | text
ref | t_id | id | none | b'id' | partition_key | 0 | int
ref | t_mt | id | none | b'id' | partition_key | 0 | int
ref | t_opts | id | none | b'id' | partition_key | 0 | int
ref | t_opts | v | none | b'v' | regular | -1 | text
ref | t_rr | id | none | b'id' | partition_key | 0 | int
ref | t_simple | id | none | b'id' | partition_key | 0 | int
ref | t_simple | v | none | b'v' | regular | -1 | text
ref | t_twcs | at | asc | b'at' | clustering | 0 | timestamp
ref | t_twcs | id | none | b'id' | partition_key | 0 | int
ref | t_twcs | v | none | b'v' | regular | -1 | text
ref | t_ucs | id | none | b'id' | partition_key | 0 | int
ref | t_vec | emb | none | b'emb' | regular | -1 | vector<float, 3>
ref | t_vec | id | none | b'id' | partition_key | 0 | int
ref | t_vec | tags | none | b'tags' | regular | -1 | vector<text, 2>
```

#### `ref computed_columns`

`0x2200` table computed_columns does not exist

#### `ref dropped_columns`

```
keyspace_name | table_name | column_name | dropped_time | kind | type
ref | t_simple | dropped_col | 2026-10-20 18:38:03.501000 | regular | int
ref | t_simple | dropped_list | 2026-10-20 18:38:04.005000 | regular | list<text>
```

#### `ref dropped_columns raw`

```
keyspace_name | table_name | column_name | dropped_time_raw | type
ref | t_simple | dropped_col | 1792521483501 | int
ref | t_simple | dropped_list | 1792521484005 | list<text>
```

#### `ref functions`

columns: keyspace_name, function_name, argument_types, argument_names, body, called_on_null_input, language, return_type — no rows

#### `ref indexes`

```
keyspace_name | table_name | index_name | kind | options
ref | QuotedTable | QuotedIdx | COMPOSITES | {'target': '"camelCase"'}
ref | t_all | t_all_b_idx | COMPOSITES | {'target': 'b'}
ref | t_all | t_all_c2_idx | COMPOSITES | {'target': 'c2'}
ref | t_all | t_all_fl_l_idx | COMPOSITES | {'target': 'full(fl_l)'}
ref | t_all | t_all_m_entries_idx | COMPOSITES | {'target': 'entries(m)'}
ref | t_all | t_all_m_keys_idx | COMPOSITES | {'target': 'keys(m)'}
ref | t_all | t_all_s_idx | COMPOSITES | {'target': 's'}
ref | t_all | t_all_st_idx | COMPOSITES | {'target': 'values(st)'}
ref | t_all | t_all_txt_idx | COMPOSITES | {'target': 'txt'}
ref | t_all | t_all_txt_sai | CUSTOM | {'case_sensitive': 'false', 'class_name': 'StorageAttachedIndex', 'normalize': 'true', 'target': 'vc'}
ref | t_simple | t_simple_v_sai | CUSTOM | {'class_name': 'sai', 'target': 'v'}
ref | t_vec | t_vec_emb_idx | CUSTOM | {'class_name': 'sai', 'similarity_function': 'COSINE', 'target': 'emb'}
```

#### `ref keyspaces`

```
keyspace_name | durable_writes | replication
ref | True | {'class': 'org.apache.cassandra.locator.NetworkTopologyStrategy', 'datacenter1': '1'}
```

#### `ref scylla_keyspaces`

`0x2200` table scylla_keyspaces does not exist

#### `ref scylla_tables`

`0x2200` table scylla_tables does not exist

#### `ref tables`

```
keyspace_name | table_name | additional_write_policy | allow_auto_snapshot | bloom_filter_fp_chance | caching | cdc | comment | compaction | compression | crc_check_chance | dclocal_read_repair_chance | default_time_to_live | extensions | flags | gc_grace_seconds | id | incremental_backups | max_index_interval | memtable | memtable_flush_period_in_ms | min_index_interval | read_repair | read_repair_chance | speculative_retry
ref | QuotedTable | 99p | None | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'NONE'} | None |  | {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'} | {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'} | 1.0 | 0.0 | 0 | {} | SortedSet(['compound']) | 864000 | <uuid> | None | 2048 | None | 0 | 128 | BLOCKING | 0.0 | 99p
ref | t_all | 99p | None | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'NONE'} | None | sample table | {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'} | {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'} | 1.0 | 0.0 | 0 | {} | SortedSet(['compound']) | 864000 | <uuid> | None | 2048 | None | 0 | 128 | BLOCKING | 0.0 | 99p
ref | t_cdc2 | 99p | None | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'NONE'} | None |  | {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'} | {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'} | 1.0 | 0.0 | 0 | {} | SortedSet(['compound']) | 864000 | <uuid> | None | 2048 | None | 0 | 128 | BLOCKING | 0.0 | 99p
ref | t_comp1 | 99p | None | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'NONE'} | None |  | {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'} | {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.ZstdCompressor'} | 1.0 | 0.0 | 0 | {} | SortedSet(['compound']) | 864000 | <uuid> | None | 2048 | None | 0 | 128 | BLOCKING | 0.0 | 99p
ref | t_comp3 | 99p | None | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'NONE'} | None |  | {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'} | {'enabled': 'false'} | 1.0 | 0.0 | 0 | {} | SortedSet(['compound']) | 864000 | <uuid> | None | 2048 | None | 0 | 128 | BLOCKING | 0.0 | 99p
ref | t_compact | 99p | None | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'NONE'} | None |  | {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'} | {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'} | 1.0 | 0.0 | 0 | {} | SortedSet([]) | 864000 | <uuid> | None | 2048 | None | 0 | 128 | BLOCKING | 0.0 | 99p
ref | t_compact2 | 99p | None | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'NONE'} | None |  | {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'} | {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'} | 1.0 | 0.0 | 0 | {} | SortedSet(['dense']) | 864000 | <uuid> | None | 2048 | None | 0 | 128 | BLOCKING | 0.0 | 99p
ref | t_counter | 99p | None | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'NONE'} | None |  | {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'} | {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'} | 1.0 | 0.0 | 0 | {} | SortedSet(['compound', 'counter']) | 864000 | <uuid> | None | 2048 | None | 0 | 128 | BLOCKING | 0.0 | 99p
ref | t_id | 99p | None | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'NONE'} | None |  | {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'} | {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'} | 1.0 | 0.0 | 0 | {} | SortedSet(['compound']) | 864000 | 5a1c395e-b41f-11e5-9f22-ba0be0483c18 | None | 2048 | None | 0 | 128 | BLOCKING | 0.0 | 99p
ref | t_mt | 99p | None | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'NONE'} | None |  | {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'} | {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'} | 1.0 | 0.0 | 0 | {} | SortedSet(['compound']) | 864000 | <uuid> | None | 2048 | None | 0 | 128 | BLOCKING | 0.0 | 99p
ref | t_opts | 99p | None | 0.1 | {'keys': 'ALL', 'rows_per_partition': '10'} | None |  | {'class': 'org.apache.cassandra.db.compaction.LeveledCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4', 'sstable_size_in_mb': '64'} | {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'} | 0.5 | 0.0 | 3600 | {} | SortedSet(['compound']) | 3600 | <uuid> | None | 1024 | None | 60000 | 64 | BLOCKING | 0.0 | 99p
ref | t_rr | 90p | None | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'NONE'} | None |  | {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'} | {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'} | 1.0 | 0.0 | 0 | {} | SortedSet(['compound']) | 864000 | <uuid> | None | 2048 | None | 0 | 128 | NONE | 0.0 | 99p
ref | t_simple | 99p | None | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'NONE'} | None | altered | {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'} | {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'} | 1.0 | 0.0 | 0 | {} | SortedSet(['compound']) | 7200 | <uuid> | None | 2048 | None | 0 | 128 | BLOCKING | 0.0 | 99p
ref | t_twcs | 99p | None | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'NONE'} | None |  | {'class': 'org.apache.cassandra.db.compaction.TimeWindowCompactionStrategy', 'compaction_window_size': '6', 'compaction_window_unit': 'HOURS', 'max_threshold': '32', 'min_threshold': '4'} | {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'} | 1.0 | 0.0 | 0 | {} | SortedSet(['compound']) | 864000 | <uuid> | None | 2048 | None | 0 | 128 | BLOCKING | 0.0 | 99p
ref | t_ucs | 99p | None | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'NONE'} | None |  | {'class': 'org.apache.cassandra.db.compaction.UnifiedCompactionStrategy'} | {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'} | 1.0 | 0.0 | 0 | {} | SortedSet(['compound']) | 864000 | <uuid> | None | 2048 | None | 0 | 128 | BLOCKING | 0.0 | 99p
ref | t_vec | 99p | None | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'NONE'} | None |  | {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'} | {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'} | 1.0 | 0.0 | 0 | {} | SortedSet(['compound']) | 864000 | <uuid> | None | 2048 | None | 0 | 128 | BLOCKING | 0.0 | 99p
```

#### `ref triggers`

columns: keyspace_name, table_name, trigger_name, options — no rows

#### `ref types`

```
keyspace_name | type_name | field_names | field_types
ref | address | ['street', 'zip', 'tags'] | ['text', 'int', 'frozen<set<text>>']
ref | person | ['name', 'home', 'Quoted Field'] | ['text', 'frozen<address>', 'int']
```

#### `ref view_virtual_columns`

`0x2200` table view_virtual_columns does not exist

#### `ref views`

columns: keyspace_name, view_name, additional_write_policy, allow_auto_snapshot, base_table_id, base_table_name, bloom_filter_fp_chance, caching, cdc, comment, compaction, compression, crc_check_chance, dclocal_read_repair_chance, default_time_to_live, extensions, gc_grace_seconds, id, include_all_columns, incremental_backups, max_index_interval, memtable, memtable_flush_period_in_ms, min_index_interval, read_repair, read_repair_chance, speculative_retry, where_clause — no rows

#### `ref_simple aggregates`

columns: keyspace_name, aggregate_name, argument_types, final_func, initcond, return_type, state_func, state_type — no rows

#### `ref_simple column_masks`

columns: keyspace_name, table_name, column_name, function_argument_nulls, function_argument_types, function_argument_values, function_keyspace, function_name — no rows

#### `ref_simple columns`

columns: keyspace_name, table_name, column_name, clustering_order, column_name_bytes, kind, position, type — no rows

#### `ref_simple computed_columns`

`0x2200` table computed_columns does not exist

#### `ref_simple dropped_columns`

columns: keyspace_name, table_name, column_name, dropped_time, kind, type — no rows

#### `ref_simple dropped_columns raw`

columns: keyspace_name, table_name, column_name, dropped_time_raw, type — no rows

#### `ref_simple functions`

columns: keyspace_name, function_name, argument_types, argument_names, body, called_on_null_input, language, return_type — no rows

#### `ref_simple indexes`

columns: keyspace_name, table_name, index_name, kind, options — no rows

#### `ref_simple keyspaces`

```
keyspace_name | durable_writes | replication
ref_simple | False | {'class': 'org.apache.cassandra.locator.SimpleStrategy', 'replication_factor': '1'}
```

#### `ref_simple scylla_keyspaces`

`0x2200` table scylla_keyspaces does not exist

#### `ref_simple scylla_tables`

`0x2200` table scylla_tables does not exist

#### `ref_simple tables`

columns: keyspace_name, table_name, additional_write_policy, allow_auto_snapshot, bloom_filter_fp_chance, caching, cdc, comment, compaction, compression, crc_check_chance, dclocal_read_repair_chance, default_time_to_live, extensions, flags, gc_grace_seconds, id, incremental_backups, max_index_interval, memtable, memtable_flush_period_in_ms, min_index_interval, read_repair, read_repair_chance, speculative_retry — no rows

#### `ref_simple triggers`

columns: keyspace_name, table_name, trigger_name, options — no rows

#### `ref_simple types`

columns: keyspace_name, type_name, field_names, field_types — no rows

#### `ref_simple view_virtual_columns`

`0x2200` table view_virtual_columns does not exist

#### `ref_simple views`

columns: keyspace_name, view_name, additional_write_policy, allow_auto_snapshot, base_table_id, base_table_name, bloom_filter_fp_chance, caching, cdc, comment, compaction, compression, crc_check_chance, dclocal_read_repair_chance, default_time_to_live, extensions, gc_grace_seconds, id, include_all_columns, incremental_backups, max_index_interval, memtable, memtable_flush_period_in_ms, min_index_interval, read_repair, read_repair_chance, speculative_retry, where_clause — no rows

#### `ref_vnodes aggregates`

columns: keyspace_name, aggregate_name, argument_types, final_func, initcond, return_type, state_func, state_type — no rows

#### `ref_vnodes column_masks`

columns: keyspace_name, table_name, column_name, function_argument_nulls, function_argument_types, function_argument_values, function_keyspace, function_name — no rows

#### `ref_vnodes columns`

```
keyspace_name | table_name | column_name | clustering_order | column_name_bytes | kind | position | type
ref_vnodes | base | id | none | b'id' | partition_key | 0 | int
ref_vnodes | base | v | none | b'v' | regular | -1 | text
```

#### `ref_vnodes computed_columns`

`0x2200` table computed_columns does not exist

#### `ref_vnodes dropped_columns`

columns: keyspace_name, table_name, column_name, dropped_time, kind, type — no rows

#### `ref_vnodes dropped_columns raw`

columns: keyspace_name, table_name, column_name, dropped_time_raw, type — no rows

#### `ref_vnodes functions`

columns: keyspace_name, function_name, argument_types, argument_names, body, called_on_null_input, language, return_type — no rows

#### `ref_vnodes indexes`

```
keyspace_name | table_name | index_name | kind | options
ref_vnodes | base | base_v_idx | COMPOSITES | {'target': 'v'}
```

#### `ref_vnodes keyspaces`

```
keyspace_name | durable_writes | replication
ref_vnodes | True | {'class': 'org.apache.cassandra.locator.NetworkTopologyStrategy', 'datacenter1': '1'}
```

#### `ref_vnodes scylla_keyspaces`

`0x2200` table scylla_keyspaces does not exist

#### `ref_vnodes scylla_tables`

`0x2200` table scylla_tables does not exist

#### `ref_vnodes tables`

```
keyspace_name | table_name | additional_write_policy | allow_auto_snapshot | bloom_filter_fp_chance | caching | cdc | comment | compaction | compression | crc_check_chance | dclocal_read_repair_chance | default_time_to_live | extensions | flags | gc_grace_seconds | id | incremental_backups | max_index_interval | memtable | memtable_flush_period_in_ms | min_index_interval | read_repair | read_repair_chance | speculative_retry
ref_vnodes | base | 99p | None | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'NONE'} | None |  | {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'} | {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'} | 1.0 | 0.0 | 0 | {} | SortedSet(['compound']) | 864000 | <uuid> | None | 2048 | None | 0 | 128 | BLOCKING | 0.0 | 99p
```

#### `ref_vnodes triggers`

columns: keyspace_name, table_name, trigger_name, options — no rows

#### `ref_vnodes types`

columns: keyspace_name, type_name, field_names, field_types — no rows

#### `ref_vnodes view_virtual_columns`

`0x2200` table view_virtual_columns does not exist

#### `ref_vnodes views`

columns: keyspace_name, view_name, additional_write_policy, allow_auto_snapshot, base_table_id, base_table_name, bloom_filter_fp_chance, caching, cdc, comment, compaction, compression, crc_check_chance, dclocal_read_repair_chance, default_time_to_live, extensions, gc_grace_seconds, id, include_all_columns, incremental_backups, max_index_interval, memtable, memtable_flush_period_in_ms, min_index_interval, read_repair, read_repair_chance, speculative_retry, where_clause — no rows

Note: `dropped_columns raw` re-reads `dropped_time` as `blobAsBigint(timestampAsBlob(dropped_time))`.

## Part 3 — the sample schema with features enabled

The same statements on a node started with the suites' settings (see conformance.md): PasswordAuthenticator, UDFs enabled (ScyllaDB `--enable-user-defined-functions 1 --experimental-features=udf`; Cassandra `user_defined_functions_enabled`), materialized views and SASI enabled on Cassandra. Only the rows that differ in kind from Part 2 are shown.

| step | ScyllaDB 2026.3.3 | Cassandra 5.0.9 |
|---|---|---|
| ks | OK; warnings: "Using Replication Factor datacenter1=1 lower than the minimum_replication_factor_warn_threshold=3 is not recommended." | OK |
| ks_simple | `0x2300` SimpleStrategy doesn't support tablet replication | OK |
| ks_simple_vnodes_scylla | OK; warnings: "SimpleStrategy replication class is not recommended, but was used for keyspace ref_simple. You may suppress this warning by delisting SimpleStrategy from replication_strategy_warn_list configuration option, or make it into an error by listing this replication strategy on replication_strategy_fail_list." / "Using Replication Factor replication_factor=1 lower than the minimum_replication_factor_warn_threshold=3 is not recommended." | `0x2000` Unknown property 'tablets' |
| ks_vnodes_scylla | OK; warnings: "Using Replication Factor datacenter1=1 lower than the minimum_replication_factor_warn_threshold=3 is not recommended." | `0x2000` Unknown property 'tablets' |
| ks_vnodes_cassandra_fallback | OK; warnings: "Using Replication Factor datacenter1=1 lower than the minimum_replication_factor_warn_threshold=3 is not recommended." | OK |
| udt | OK | OK |
| udt_nested | OK | OK |
| t_simple | OK | OK |
| t_all | OK | OK |
| t_quoted | OK | OK |
| t_quoted_bad | `0x2200` "Quoted Table" is not a valid table name (must be alphanumeric character only: [0-9A-Za-z]+) | `0x2300` ref.Quoted Table: Table name must not be empty or not contain non-alphanumeric-underscore characters (got "Quoted Table") |
| t_counter | OK | OK |
| t_compact | `0x2200` Support for the deprecated feature of 'CREATE TABLE WITH COMPACT STORAGE' is disabled and will eventually be removed in a future version.  To enable, set the 'enable_create_table_with_compact_storage' config option to 'true'. | OK |
| t_compact_clustering | `0x2200` Support for the deprecated feature of 'CREATE TABLE WITH COMPACT STORAGE' is disabled and will eventually be removed in a future version.  To enable, set the 'enable_create_table_with_compact_storage' config option to 'true'. | OK |
| t_opts | OK | OK |
| t_twcs | OK | OK |
| t_compression_class | `0x2300` Missing sub-option 'sstable_compression' for the 'compression' option. | OK |
| t_compression_sstable_compression | OK | `0x2300` Missing sub-option 'class' for the 'compression' option. |
| t_compression_disabled | `0x2300` Missing sub-option 'sstable_compression' for the 'compression' option. | OK |
| t_ucs_cassandra | `0x2300` Unable to find compaction strategy class 'UnifiedCompactionStrategy' | OK |
| t_ics_scylla | OK | `0x2300` Unable to find compaction strategy class 'org.apache.cassandra.db.compaction.IncrementalCompactionStrategy' |
| t_tombstone_gc_scylla | OK | `0x2000` Unknown property 'tombstone_gc' |
| t_cdc_scylla | OK | `0x2000` Invalid value for property 'cdc'. It should be a string |
| t_cdc_cassandra | `0x0000` Cannot create cdc info from string | OK |
| t_read_repair_cassandra | `0x2000` Unknown property 'additional_write_policy' | OK |
| t_memtable_cassandra | `0x2000` Unknown property 'memtable' | OK |
| t_with_id | OK | OK |
| t_paxos_grace_scylla | OK | `0x2000` Unknown property 'paxos_grace_seconds' |
| alter_add | OK | OK |
| alter_drop | OK | OK |
| alter_add_list | OK | OK |
| alter_drop_list | OK | OK |
| alter_rename_ck | OK | OK |
| alter_options | OK | OK |
| index | OK; warnings: "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | OK |
| index_keys | OK; warnings: "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | OK |
| index_values_noname | OK; warnings: "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | OK |
| index_entries | OK; warnings: "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | OK |
| index_full | OK; warnings: "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | OK |
| index_static | OK; warnings: "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | OK |
| index_pk_component | OK; warnings: "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | OK |
| index_clustering | OK; warnings: "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | OK |
| index_local_scylla | OK; warnings: "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | `0x2000` line 1:43 mismatched input '(' expecting ')' (... t_all_local_idx ON ref.t_all ([(]...) |
| index_quoted | OK; warnings: "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | `0x2300` Illegal index name Quoted Idx |
| index_quoted_ok | `0x2200` Index QuotedIdx is a duplicate of existing index Quoted Idx | OK |
| index_sai_cassandra | `0x2200` StorageAttachedIndex (SAI) is only supported on vector columns; use a secondary index for non-vector columns | OK |
| index_sai_options_cassandra | `0x2200` StorageAttachedIndex (SAI) is only supported on vector columns; use a secondary index for non-vector columns | OK |
| index_custom_sasi | `0x2200` Non-supported custom class 'org.apache.cassandra.index.sasi.SASIIndex' provided | OK; warnings: "SASI indexes are experimental and are not recommended for production use." |
| mv | OK; warnings: "Creating a materialized view in a keyspaces that uses tablets requires the keyspace to remain RF-rack-valid while the materialized view exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | OK; warnings: "Materialized views are experimental and are not recommended for production use." |
| mv_clustering_order | OK; warnings: "Creating a materialized view in a keyspaces that uses tablets requires the keyspace to remain RF-rack-valid while the materialized view exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | OK; warnings: "Materialized views are experimental and are not recommended for production use." |
| udf_java | `0x2200` Language 'java' is not supported | OK |
| udf_lua | OK | `0x2200` Currently only Java UDFs are available in Cassandra. For more information - CASSANDRA-18252 and CASSANDRA-17281 |
| udf_js | `0x2200` Language 'javascript' is not supported | `0x2200` Currently only Java UDFs are available in Cassandra. For more information - CASSANDRA-18252 and CASSANDRA-17281 |
| udf_java_acc | `0x2200` Language 'java' is not supported | OK |
| uda_java | `0x2200` State function acc(int, int) not found | OK |
| udf_lua_acc | OK | `0x2200` Currently only Java UDFs are available in Cassandra. For more information - CASSANDRA-18252 and CASSANDRA-17281 |
| uda_lua | OK | `0x0000` java.lang.AssertionError: Unable to find function object for resource <function ref.acc_lua(int, int)> |
| vector_table | OK | OK |
| vector_sai | OK | OK; warnings: "SAI ANN indexes on vector columns are experimental and are not recommended for production use.\nThey don't yet support SELECT queries with:\n * Consistency level higher than ONE/LOCAL_ONE.\n * Paging.\n * No LIMIT clauses.\n * PER PARTITION LIMIT clauses.\n * GROUP BY clauses.\n * Aggregation functions.\n * Filters on columns without a SAI index." |
| vector_index_scylla | OK | `0x2300` Unable to find custom indexer class 'vector_index' |
| masked_cassandra | `0x2000` line 1:51 no viable alternative at input 'text' | OK |
| trigger | `0x2000` line 1:0 no viable alternative at input 'CREATE' | `0x2200` Trigger class 'org.apache.cassandra.triggers.AuditTrigger' couldn't be loaded |
| mv_vnodes_base | OK | OK |
| mv_vnodes_view | OK | OK; warnings: "Materialized views are experimental and are not recommended for production use." |
| index_vnodes | OK | OK |

### Rows with features enabled, ScyllaDB 2026.3.3

#### `ref functions`

```
keyspace_name | function_name | argument_types | argument_names | body | called_on_null_input | language | return_type
ref | acc_lua | ['int', 'int'] | ['s', 'x'] | return s + x | True | lua | int
ref | plus1_lua | ['int'] | ['x'] | return x + 1 | False | lua | int
```

#### `ref aggregates`

```
keyspace_name | aggregate_name | argument_types | final_func | initcond | return_type | state_func | state_type
ref | sum1_lua | ['int'] | None | 0 | int | acc_lua | int
```

#### `ref views`

```
keyspace_name | view_name | base_table_id | base_table_name | bloom_filter_fp_chance | caching | comment | compaction | compression | crc_check_chance | dclocal_read_repair_chance | default_time_to_live | extensions | gc_grace_seconds | id | include_all_columns | max_index_interval | memtable_flush_period_in_ms | min_index_interval | read_repair_chance | speculative_retry | where_clause
ref | Quoted Idx_index | da806cc0-ccc1-11f1-b647-c0eb11f359c7 | QuotedTable | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | False | 2048 | 0 | 128 | None | 99.0PERCENTILE | "camelCase" IS NOT NULL
ref | t_all_b_idx_index | da7c7520-ccc1-11f1-b647-c0eb11f359c7 | t_all | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | False | 2048 | 0 | 128 | None | 99.0PERCENTILE | b IS NOT NULL
ref | t_all_by_txt | da7c7520-ccc1-11f1-b647-c0eb11f359c7 | t_all | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | False | 2048 | 0 | 128 | None | 99.0PERCENTILE | txt IS NOT null AND a IS NOT null AND b IS NOT null AND c1 IS NOT null AND c2 IS NOT null
ref | t_all_c2_idx_index | da7c7520-ccc1-11f1-b647-c0eb11f359c7 | t_all | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | False | 2048 | 0 | 128 | None | 99.0PERCENTILE | c2 IS NOT NULL
ref | t_all_fl_l_idx_index | da7c7520-ccc1-11f1-b647-c0eb11f359c7 | t_all | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | False | 2048 | 0 | 128 | None | 99.0PERCENTILE | fl_l IS NOT NULL
ref | t_all_local_idx_index | da7c7520-ccc1-11f1-b647-c0eb11f359c7 | t_all | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'scylla_tags': b'\x01\x00\x00\x00\x1f\x00\x00\x00system:synchronous_view_updates\x04\x00\x00\x00true', 'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x07\x00\x00\x00timeout\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | False | 2048 | 0 | 128 | None | 99.0PERCENTILE | vi IS NOT NULL
ref | t_all_m_entries_idx_index | da7c7520-ccc1-11f1-b647-c0eb11f359c7 | t_all | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | False | 2048 | 0 | 128 | None | 99.0PERCENTILE | 
ref | t_all_m_keys_idx_index | da7c7520-ccc1-11f1-b647-c0eb11f359c7 | t_all | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | False | 2048 | 0 | 128 | None | 99.0PERCENTILE | 
ref | t_all_s_idx_index | da7c7520-ccc1-11f1-b647-c0eb11f359c7 | t_all | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | False | 2048 | 0 | 128 | None | 99.0PERCENTILE | s IS NOT NULL
ref | t_all_st_idx_index | da7c7520-ccc1-11f1-b647-c0eb11f359c7 | t_all | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | False | 2048 | 0 | 128 | None | 99.0PERCENTILE | 
ref | t_all_txt_idx_index | da7c7520-ccc1-11f1-b647-c0eb11f359c7 | t_all | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | False | 2048 | 0 | 128 | None | 99.0PERCENTILE | txt IS NOT NULL
ref | t_simple_by_v | da723bf0-ccc1-11f1-b647-c0eb11f359c7 | t_simple | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} | mv | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | False | 2048 | 0 | 128 | None | 99.0PERCENTILE | v IS NOT null AND id IS NOT null
```

#### `ref indexes`

```
keyspace_name | table_name | index_name | kind | options
ref | QuotedTable | Quoted Idx | COMPOSITES | {'target': '"camelCase"'}
ref | t_all | t_all_b_idx | COMPOSITES | {'target': 'b'}
ref | t_all | t_all_c2_idx | COMPOSITES | {'target': 'c2'}
ref | t_all | t_all_fl_l_idx | COMPOSITES | {'target': 'fl_l'}
ref | t_all | t_all_local_idx | COMPOSITES | {'target': '{"pk":["a","b"],"ck":["vi"]}'}
ref | t_all | t_all_m_entries_idx | COMPOSITES | {'target': 'entries(m)'}
ref | t_all | t_all_m_keys_idx | COMPOSITES | {'target': 'keys(m)'}
ref | t_all | t_all_s_idx | COMPOSITES | {'target': 's'}
ref | t_all | t_all_st_idx | COMPOSITES | {'target': 'keys(st)'}
ref | t_all | t_all_txt_idx | COMPOSITES | {'target': 'txt'}
ref | t_vec | t_vec_emb_idx | CUSTOM | {'index_version': 'dba69d40-ccc1-11f1-b647-c0eb11f359c7', 'class_name': 'vector_index', 'target': 'emb', 'similarity_function': 'COSINE'}
ref | t_vec | t_vec_emb_vs | CUSTOM | {'target': 'emb', 'index_version': 'dbb06140-ccc1-11f1-b647-c0eb11f359c7', 'class_name': 'vector_index'}
```

#### `ref view_virtual_columns`

```
keyspace_name | table_name | column_name | clustering_order | column_name_bytes | kind | position | type
ref | t_all_b_idx_index | addr | NONE | b'addr' | regular | -1 | address
ref | t_all_b_idx_index | addrs | NONE | b'addrs' | regular | -1 | list<empty>
ref | t_all_b_idx_index | asc_ | NONE | b'asc_' | regular | -1 | empty
ref | t_all_b_idx_index | big | NONE | b'big' | regular | -1 | empty
ref | t_all_b_idx_index | bl | NONE | b'bl' | regular | -1 | empty
ref | t_all_b_idx_index | bo | NONE | b'bo' | regular | -1 | empty
ref | t_all_b_idx_index | da | NONE | b'da' | regular | -1 | empty
ref | t_all_b_idx_index | de | NONE | b'de' | regular | -1 | empty
ref | t_all_b_idx_index | do | NONE | b'do' | regular | -1 | empty
ref | t_all_b_idx_index | du | NONE | b'du' | regular | -1 | empty
ref | t_all_b_idx_index | faddr | NONE | b'faddr' | regular | -1 | empty
ref | t_all_b_idx_index | fl | NONE | b'fl' | regular | -1 | empty
ref | t_all_b_idx_index | fl_l | NONE | b'fl_l' | regular | -1 | empty
ref | t_all_b_idx_index | fl_m | NONE | b'fl_m' | regular | -1 | empty
ref | t_all_b_idx_index | fl_s | NONE | b'fl_s' | regular | -1 | empty
ref | t_all_b_idx_index | ftup | NONE | b'ftup' | regular | -1 | empty
ref | t_all_b_idx_index | ine | NONE | b'ine' | regular | -1 | empty
ref | t_all_b_idx_index | l | NONE | b'l' | regular | -1 | list<empty>
ref | t_all_b_idx_index | m | NONE | b'm' | regular | -1 | map<text, empty>
ref | t_all_b_idx_index | nested | NONE | b'nested' | regular | -1 | map<text, empty>
ref | t_all_b_idx_index | sm | NONE | b'sm' | regular | -1 | empty
ref | t_all_b_idx_index | st | NONE | b'st' | regular | -1 | set<text>
ref | t_all_b_idx_index | ti | NONE | b'ti' | regular | -1 | empty
ref | t_all_b_idx_index | tin | NONE | b'tin' | regular | -1 | empty
ref | t_all_b_idx_index | tu | NONE | b'tu' | regular | -1 | empty
ref | t_all_b_idx_index | tup | NONE | b'tup' | regular | -1 | empty
ref | t_all_b_idx_index | txt | NONE | b'txt' | regular | -1 | empty
ref | t_all_b_idx_index | vc | NONE | b'vc' | regular | -1 | empty
ref | t_all_b_idx_index | vi | NONE | b'vi' | regular | -1 | empty
ref | t_all_b_idx_index | who | NONE | b'who' | regular | -1 | empty
ref | t_all_c2_idx_index | addr | NONE | b'addr' | regular | -1 | address
ref | t_all_c2_idx_index | addrs | NONE | b'addrs' | regular | -1 | list<empty>
ref | t_all_c2_idx_index | asc_ | NONE | b'asc_' | regular | -1 | empty
ref | t_all_c2_idx_index | big | NONE | b'big' | regular | -1 | empty
ref | t_all_c2_idx_index | bl | NONE | b'bl' | regular | -1 | empty
ref | t_all_c2_idx_index | bo | NONE | b'bo' | regular | -1 | empty
ref | t_all_c2_idx_index | da | NONE | b'da' | regular | -1 | empty
ref | t_all_c2_idx_index | de | NONE | b'de' | regular | -1 | empty
ref | t_all_c2_idx_index | do | NONE | b'do' | regular | -1 | empty
ref | t_all_c2_idx_index | du | NONE | b'du' | regular | -1 | empty
ref | t_all_c2_idx_index | faddr | NONE | b'faddr' | regular | -1 | empty
ref | t_all_c2_idx_index | fl | NONE | b'fl' | regular | -1 | empty
ref | t_all_c2_idx_index | fl_l | NONE | b'fl_l' | regular | -1 | empty
ref | t_all_c2_idx_index | fl_m | NONE | b'fl_m' | regular | -1 | empty
ref | t_all_c2_idx_index | fl_s | NONE | b'fl_s' | regular | -1 | empty
ref | t_all_c2_idx_index | ftup | NONE | b'ftup' | regular | -1 | empty
ref | t_all_c2_idx_index | ine | NONE | b'ine' | regular | -1 | empty
ref | t_all_c2_idx_index | l | NONE | b'l' | regular | -1 | list<empty>
ref | t_all_c2_idx_index | m | NONE | b'm' | regular | -1 | map<text, empty>
ref | t_all_c2_idx_index | nested | NONE | b'nested' | regular | -1 | map<text, empty>
ref | t_all_c2_idx_index | sm | NONE | b'sm' | regular | -1 | empty
ref | t_all_c2_idx_index | st | NONE | b'st' | regular | -1 | set<text>
ref | t_all_c2_idx_index | ti | NONE | b'ti' | regular | -1 | empty
ref | t_all_c2_idx_index | tin | NONE | b'tin' | regular | -1 | empty
ref | t_all_c2_idx_index | tu | NONE | b'tu' | regular | -1 | empty
ref | t_all_c2_idx_index | tup | NONE | b'tup' | regular | -1 | empty
ref | t_all_c2_idx_index | txt | NONE | b'txt' | regular | -1 | empty
ref | t_all_c2_idx_index | vc | NONE | b'vc' | regular | -1 | empty
ref | t_all_c2_idx_index | vi | NONE | b'vi' | regular | -1 | empty
ref | t_all_c2_idx_index | who | NONE | b'who' | regular | -1 | empty
```

#### `ref_vnodes views`

```
keyspace_name | view_name | base_table_id | base_table_name | bloom_filter_fp_chance | caching | comment | compaction | compression | crc_check_chance | dclocal_read_repair_chance | default_time_to_live | extensions | gc_grace_seconds | id | include_all_columns | max_index_interval | memtable_flush_period_in_ms | min_index_interval | read_repair_chance | speculative_retry | where_clause
ref_vnodes | base_by_v | dbb87790-ccc1-11f1-b647-c0eb11f359c7 | base | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | True | 2048 | 0 | 128 | None | 99.0PERCENTILE | v IS NOT null AND id IS NOT null
ref_vnodes | base_v_idx_index | dbb87790-ccc1-11f1-b647-c0eb11f359c7 | base | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |  | {'class': 'IncrementalCompactionStrategy'} | {'sstable_compression': 'LZ4WithDictsCompressor'} | 1.0 | None | 0 | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} | 864000 | <uuid> | False | 2048 | 0 | 128 | None | 99.0PERCENTILE | v IS NOT NULL
```

### Rows with features enabled, Cassandra 5.0.9

#### `ref functions`

```
keyspace_name | function_name | argument_types | argument_names | body | called_on_null_input | language | return_type
ref | acc | ['int', 'int'] | ['s', 'x'] | return s + x; | True | java | int
ref | plus1 | ['int'] | ['x'] | return x + 1; | False | java | int
```

#### `ref aggregates`

```
keyspace_name | aggregate_name | argument_types | final_func | initcond | return_type | state_func | state_type
ref | sum1 | ['int'] | None | 0 | int | acc | int
```

#### `ref views`

```
keyspace_name | view_name | additional_write_policy | allow_auto_snapshot | base_table_id | base_table_name | bloom_filter_fp_chance | caching | cdc | comment | compaction | compression | crc_check_chance | dclocal_read_repair_chance | default_time_to_live | extensions | gc_grace_seconds | id | include_all_columns | incremental_backups | max_index_interval | memtable | memtable_flush_period_in_ms | min_index_interval | read_repair | read_repair_chance | speculative_retry | where_clause
ref | t_all_by_txt | 99p | None | e4deba80-cccd-11f1-9c21-2f5cfa694e51 | t_all | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'NONE'} | None |  | {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'} | {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'} | 1.0 | 0.0 | 0 | {} | 864000 | <uuid> | False | None | 2048 | None | 0 | 128 | BLOCKING | 0.0 | 99p | txt IS NOT NULL AND a IS NOT NULL AND b IS NOT NULL AND c1 IS NOT NULL AND c2 IS NOT NULL
ref | t_simple_by_v | 99p | None | e4c8e890-cccd-11f1-9c21-2f5cfa694e51 | t_simple | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'NONE'} | None | mv | {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'} | {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'} | 1.0 | 0.0 | 0 | {} | 864000 | <uuid> | False | None | 2048 | None | 0 | 128 | BLOCKING | 0.0 | 99p | v IS NOT NULL AND id IS NOT NULL
```

#### `ref indexes`

```
keyspace_name | table_name | index_name | kind | options
ref | QuotedTable | QuotedIdx | COMPOSITES | {'target': '"camelCase"'}
ref | t_all | t_all_b_idx | COMPOSITES | {'target': 'b'}
ref | t_all | t_all_c2_idx | COMPOSITES | {'target': 'c2'}
ref | t_all | t_all_fl_l_idx | COMPOSITES | {'target': 'full(fl_l)'}
ref | t_all | t_all_m_entries_idx | COMPOSITES | {'target': 'entries(m)'}
ref | t_all | t_all_m_keys_idx | COMPOSITES | {'target': 'keys(m)'}
ref | t_all | t_all_s_idx | COMPOSITES | {'target': 's'}
ref | t_all | t_all_st_idx | COMPOSITES | {'target': 'values(st)'}
ref | t_all | t_all_txt_idx | COMPOSITES | {'target': 'txt'}
ref | t_all | t_all_txt_sai | CUSTOM | {'case_sensitive': 'false', 'class_name': 'StorageAttachedIndex', 'normalize': 'true', 'target': 'vc'}
ref | t_opts | t_opts_sasi | CUSTOM | {'class_name': 'org.apache.cassandra.index.sasi.SASIIndex', 'target': 'v'}
ref | t_simple | t_simple_v_sai | CUSTOM | {'class_name': 'sai', 'target': 'v'}
ref | t_vec | t_vec_emb_idx | CUSTOM | {'class_name': 'sai', 'similarity_function': 'COSINE', 'target': 'emb'}
```

#### `ref view_virtual_columns`

error: `0x2200` table view_virtual_columns does not exist

#### `ref column_masks`

```
keyspace_name | table_name | column_name | function_argument_nulls | function_argument_types | function_argument_values | function_keyspace | function_name
ref | t_mask | email | [False, False] | ['int', 'int'] | ['1', '1'] | system | mask_inner
ref | t_mask | ssn | [] | [] | [] | system | mask_default
```

#### `ref_vnodes views`

```
keyspace_name | view_name | additional_write_policy | allow_auto_snapshot | base_table_id | base_table_name | bloom_filter_fp_chance | caching | cdc | comment | compaction | compression | crc_check_chance | dclocal_read_repair_chance | default_time_to_live | extensions | gc_grace_seconds | id | include_all_columns | incremental_backups | max_index_interval | memtable | memtable_flush_period_in_ms | min_index_interval | read_repair | read_repair_chance | speculative_retry | where_clause
ref_vnodes | base_by_v | 99p | None | e8299fc0-cccd-11f1-9c21-2f5cfa694e51 | base | 0.01 | {'keys': 'ALL', 'rows_per_partition': 'NONE'} | None |  | {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'} | {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'} | 1.0 | 0.0 | 0 | {} | 864000 | <uuid> | True | None | 2048 | None | 0 | 128 | BLOCKING | 0.0 | 99p | v IS NOT NULL AND id IS NOT NULL
```
