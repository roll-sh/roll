<!-- Generated from our own observations of the reference images (see conformance.md for the capture method). Do not edit by hand. -->

# Default options and settings

## Table defaults as `DESCRIBE` prints them

`CREATE TABLE ref.t_simple (id int PRIMARY KEY, v text)` then `ALTER TABLE ref.t_simple WITH comment = 'altered' AND gc_grace_seconds = 7200` (plus an index, a materialized view and dropped columns where accepted):

### ScyllaDB 2026.3.3, `DESCRIBE TABLE ref.t_counter` (no options given)

```cql
CREATE TABLE ref.t_counter (
    id int,
    k text,
    c counter,
    c2 counter,
    PRIMARY KEY (id, k)
) WITH CLUSTERING ORDER BY (k ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'LZ4WithDictsCompressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE'
    AND tombstone_gc = {'mode': 'repair', 'propagation_delay_in_seconds': '3600'};

```
### Cassandra 5.0.9, `DESCRIBE TABLE ref.t_counter` (no options given)

```cql
CREATE TABLE ref.t_counter (
    id int,
    k text,
    c counter,
    c2 counter,
    PRIMARY KEY (id, k)
) WITH CLUSTERING ORDER BY (k ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = ''
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 864000
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
## Table defaults as `system_schema.tables` stores them

### ScyllaDB 2026.3.3, `ref.t_counter`

| column | value |
|---|---|
| keyspace_name | ref |
| table_name | t_counter |
| bloom_filter_fp_chance | 0.01 |
| caching | {'keys': 'ALL', 'rows_per_partition': 'ALL'} |
| comment |  |
| compaction | {'class': 'IncrementalCompactionStrategy'} |
| compression | {'sstable_compression': 'LZ4WithDictsCompressor'} |
| crc_check_chance | 1.0 |
| dclocal_read_repair_chance | None |
| default_time_to_live | 0 |
| extensions | {'tombstone_gc': b'\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600'} |
| flags | SortedSet(['compound', 'counter']) |
| gc_grace_seconds | 864000 |
| id | <uuid> |
| max_index_interval | 2048 |
| memtable_flush_period_in_ms | 0 |
| min_index_interval | 128 |
| read_repair_chance | None |
| speculative_retry | 99.0PERCENTILE |

`system_schema.scylla_tables` for `ref.t_counter`:

| column | value |
|---|---|
| keyspace_name | ref |
| table_name | t_counter |
| cdc | None |
| committed_by_group0 | True |
| in_memory | None |
| large_data_guardrails_enabled | None |
| partitioner | None |
| storage_engine | None |
| tablets | None |
| version | <uuid> |

### Cassandra 5.0.9, `ref.t_counter`

| column | value |
|---|---|
| keyspace_name | ref |
| table_name | t_counter |
| additional_write_policy | 99p |
| allow_auto_snapshot | None |
| bloom_filter_fp_chance | 0.01 |
| caching | {'keys': 'ALL', 'rows_per_partition': 'NONE'} |
| cdc | None |
| comment |  |
| compaction | {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'} |
| compression | {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'} |
| crc_check_chance | 1.0 |
| dclocal_read_repair_chance | 0.0 |
| default_time_to_live | 0 |
| extensions | {} |
| flags | SortedSet(['compound', 'counter']) |
| gc_grace_seconds | 864000 |
| id | <uuid> |
| incremental_backups | None |
| max_index_interval | 2048 |
| memtable | None |
| memtable_flush_period_in_ms | 0 |
| min_index_interval | 128 |
| read_repair | BLOCKING |
| read_repair_chance | 0.0 |
| speculative_retry | 99p |

## Keyspace defaults

### ScyllaDB 2026.3.3

`ref keyspaces`:

```
keyspace_name | durable_writes | next_replication | replication | replication_v2
ref | True | None | {'class': 'org.apache.cassandra.locator.NetworkTopologyStrategy', 'datacenter1': '1'} | None
```

`ref scylla_keyspaces`:

```
keyspace_name | consistency | initial_tablets | storage_options | storage_type
ref | None | 0 | None | None
```

`ref_simple keyspaces`:

```
keyspace_name | durable_writes | next_replication | replication | replication_v2
ref_simple | False | None | {'class': 'org.apache.cassandra.locator.SimpleStrategy', 'replication_factor': '1'} | None
```

`ref_vnodes keyspaces`:

```
keyspace_name | durable_writes | next_replication | replication | replication_v2
ref_vnodes | True | None | {'class': 'org.apache.cassandra.locator.NetworkTopologyStrategy', 'datacenter1': '1'} | None
```

System keyspaces on a fresh node (`system_schema.keyspaces`):

```
keyspace_name | durable_writes | next_replication | replication | replication_v2
system_replicated_keys | True | None | {'class': 'org.apache.cassandra.locator.EverywhereStrategy'} | None
system_auth | True | None | {'class': 'org.apache.cassandra.locator.SimpleStrategy', 'replication_factor': '1'} | None
system_schema | True | None | {'class': 'org.apache.cassandra.locator.LocalStrategy'} | None
system_distributed | True | None | {'class': 'org.apache.cassandra.locator.SimpleStrategy', 'replication_factor': '3'} | None
system | True | None | {'class': 'org.apache.cassandra.locator.LocalStrategy'} | None
audit | True | None | {'class': 'org.apache.cassandra.locator.NetworkTopologyStrategy', 'datacenter1': '3'} | None
system_traces | True | None | {'class': 'org.apache.cassandra.locator.SimpleStrategy', 'replication_factor': '2'} | None
```

### Cassandra 5.0.9

`ref keyspaces`:

```
keyspace_name | durable_writes | replication
ref | True | {'class': 'org.apache.cassandra.locator.NetworkTopologyStrategy', 'datacenter1': '1'}
```

`ref_simple keyspaces`:

```
keyspace_name | durable_writes | replication
ref_simple | False | {'class': 'org.apache.cassandra.locator.SimpleStrategy', 'replication_factor': '1'}
```

`ref_vnodes keyspaces`:

```
keyspace_name | durable_writes | replication
ref_vnodes | True | {'class': 'org.apache.cassandra.locator.NetworkTopologyStrategy', 'datacenter1': '1'}
```

System keyspaces on a fresh node (`system_schema.keyspaces`):

```
keyspace_name | durable_writes | replication
system_auth | True | {'class': 'org.apache.cassandra.locator.SimpleStrategy', 'replication_factor': '1'}
system_schema | True | {'class': 'org.apache.cassandra.locator.LocalStrategy'}
system_distributed | True | {'class': 'org.apache.cassandra.locator.SimpleStrategy', 'replication_factor': '3'}
system | True | {'class': 'org.apache.cassandra.locator.LocalStrategy'}
system_traces | True | {'class': 'org.apache.cassandra.locator.SimpleStrategy', 'replication_factor': '2'}
```

## Server settings relevant to clients

Filtered from `system.config` (ScyllaDB) and `system_views.settings` (Cassandra) by name (thresholds, feature switches, auth, tokens, names, partitioner, tablets, timeouts, guardrails). Values are as the reference images report them with the flags in [conformance.md](conformance.md) (default image config).

### ScyllaDB 2026.3.3 (`system.config`, 415 rows in total)

```
name | source | type | value
allow_per_table_encryption | default | bool | true
alternator_address | cli | string | "<ip>"
alternator_allow_system_table_write | default | bool | false
alternator_describe_endpoints | default | string | ""
alternator_describe_table_info_cache_validity_in_seconds | default | integer | 21600
alternator_encryption_options | default | string map | {}
alternator_enforce_authorization | default | bool | false
alternator_force_read_before_write | default | bool | false
alternator_http_response_disable_content_type_header | default | bool | false
alternator_http_response_disable_date_header | default | bool | false
alternator_http_response_server_header | default | string | ""
alternator_https_port | default | integer | 0
alternator_https_port_proxy_protocol | default | integer | 0
alternator_max_expression_cache_entries_per_shard | default | integer | 2000
alternator_max_items_in_batch_write | default | integer | 100
alternator_max_users_query_size_in_trace_output | default | integer | 4096
alternator_port | cli | integer | 8000
alternator_port_proxy_protocol | default | integer | 0
alternator_response_compression_threshold_in_bytes | default | integer | 4096
alternator_response_gzip_compression_level | default | integer | 6
alternator_streams_increased_compatibility | default | bool | false
alternator_streams_time_window_s | default | integer | 10
alternator_timeout_in_ms | default | integer | 10000
alternator_ttl_period_in_seconds | default | double | 86400
alternator_warn_authorization | default | bool | false
alternator_write_isolation | cli | string | "always_use_lwt"
auth_superuser_name | default | string | ""
auth_superuser_salted_password | default | string | ""
authenticator | default | string | "org.apache.cassandra.auth.AllowAllAuthenticator"
authorizer | default | string | "org.apache.cassandra.auth.AllowAllAuthorizer"
auto_repair_enabled_default | default | bool | false
auto_repair_threshold_default_in_seconds | default | integer | 86400
auto_snapshot | config | bool | true
auto_snapshot_ttl | config | integer | 864000
batch_size_fail_threshold_in_kb | config | integer | 1024
batch_size_warn_threshold_in_kb | config | integer | 128
batchlog_replay_throttle_in_kb | default | integer | 1024
broadcast_address | default | string | ""
broadcast_rpc_address | default | string | ""
cache_index_pages | default | bool | true
cas_contention_timeout_in_ms | config | integer | 1000
cdc_dont_rewrite_streams | default | bool | false
cluster_name | default | string | ""
column_index_auto_scale_threshold_in_kb | default | integer | 10240
commit_failure_policy | default | string | "stop"
commitlog_flush_threshold_in_mb | default | integer | -1
commitlog_max_data_lifetime_in_seconds | default | integer | 86400
commitlog_sync | config | string | "periodic"
commitlog_sync_batch_window_in_ms | default | integer | 10000
commitlog_sync_period_in_ms | config | integer | 10000
commitlog_use_hard_size_limit | default | bool | true
compaction_collection_elements_count_warning_threshold | default | integer | 10000
compaction_enforce_min_threshold | default | bool | false
compaction_large_cell_warning_threshold_mb | default | integer | 1
compaction_large_partition_warning_threshold_mb | default | integer | 1000
compaction_large_row_warning_threshold_mb | default | integer | 10
compaction_max_shares | default | float | 1000
compaction_rows_count_warning_threshold | default | integer | 100000
components_memory_reclaim_threshold | default | double | 0.2
counter_write_request_timeout_in_ms | default | integer | 5000
cql_duplicate_bind_variable_names_refer_to_same_variable | default | bool | true
cql_in_bind_variable_name_uses_uppercase_operator | default | bool | true
cross_node_timeout | default | bool | false
default_log_level | default | string | "info"
direct_failure_detector_ping_timeout_in_ms | default | integer | 600
disk_failure_policy | default | string | "stop"
disk_space_monitor_polling_interval_threshold | default | float | 0.9
dynamic_snitch_badness_threshold | default | double | 0
dynamic_snitch_reset_interval_in_ms | default | integer | 60000
dynamic_snitch_update_interval_in_ms | default | integer | 100
enable_3_1_0_compatibility_mode | default | bool | false
enable_cache | default | bool | true
enable_cassio_compatibility | default | bool | false
enable_commitlog | default | bool | true
enable_compacting_data_for_streaming_and_repair | default | bool | true
enable_cql_config_updates | default | bool | true
enable_create_table_with_compact_storage | default | bool | false
enable_dangerous_direct_import_of_cassandra_counters | default | bool | false
enable_deprecated_partitioners | default | bool | false
enable_file_stream | default | bool | true
enable_in_memory_data_store | default | bool | false
enable_ipv6_dns_lookup | default | bool | false
enable_keyspace_column_family_metrics | default | bool | false
enable_node_aggregated_table_metrics | default | bool | true
enable_parallelized_aggregation | default | bool | true
enable_repair_based_node_ops | default | bool | true
enable_shard_aware_drivers | default | bool | true
enable_small_table_optimization_for_rbno | default | bool | true
enable_sstable_data_integrity_check | default | bool | false
enable_sstable_key_validation | default | bool | false
enable_sstables_mc_format | default | bool | true
enable_sstables_md_format | default | bool | true
enable_tablets | default | bool | false
enable_tombstone_gc_for_streaming_and_repair | default | bool | false
enable_user_defined_functions | default | bool | false
endpoint_snitch | config | string | "SimpleSnitch"
enforce_rack_list | default | bool | false
experimental_features | default | experimental features | []
failure_detector_timeout_in_ms | default | integer | 20000
fd_max_interval_ms | default | integer | 2000
flush_schema_tables_after_modification | default | bool | true
force_schema_commit_log | default | bool | false
group0_raft_op_timeout_in_ms | default | integer | 60000
group0_tombstone_gc_refresh_interval_in_ms | default | integer | 3600000
hinted_handoff_enabled | default | hinted handoff enabled | "true"
hinted_handoff_throttle_in_kb | default | integer | 1024
in_memory_compaction_limit_in_mb | default | integer | 64
internode_authenticator | default | string | "enabled"
internode_compression_zstd_max_cpu_fraction | default | float | 0
internode_compression_zstd_max_longterm_cpu_fraction | default | float | 1
internode_compression_zstd_max_message_size | default | integer | 4294967295
large_cell_fail_threshold_mb | config | integer | 2
large_collection_elements_fail_threshold | config | integer | 20000
large_data_cql_warnings | config | bool | true
large_memory_allocation_warning_threshold | default | integer | 131073
large_partition_fail_threshold_mb | config | integer | 2000
large_row_fail_threshold_mb | config | integer | 20
listen_address | cli | string | "<ip>"
listen_on_broadcast_address | default | bool | false
live_updatable_config_params_changeable_via_cql | default | bool | true
logstor_separator_delay_limit_ms | default | integer | 100
logstor_separator_max_memory_in_mb | default | integer | 256
maintenance_reader_concurrency_semaphore_count_limit | default | integer | 10
max_clustering_key_restrictions_per_query | default | integer | 100
max_concurrent_requests_per_shard | default | integer | 4294967295
max_hint_window_in_ms | default | integer | 10800000
max_hinted_handoff_concurrency | default | integer | 0
max_hints_delivery_threads | default | integer | 2
max_memory_for_unlimited_query_hard_limit | default | integer | 104857600
max_memory_for_unlimited_query_soft_limit | default | integer | 1048576
max_partition_key_restrictions_per_query | default | integer | 100
max_relations_in_where_clause | default | integer | 100
maximum_replication_factor_fail_threshold | default | integer | -1
maximum_replication_factor_warn_threshold | default | integer | -1
memtable_cleanup_threshold | default | double | 0.11
minimal_tablet_size_for_balancing | default | integer | 53687091
minimum_replication_factor_fail_threshold | default | integer | -1
minimum_replication_factor_warn_threshold | default | integer | 3
murmur3_partitioner_ignore_msb_bits | config | integer | 12
native_transport_max_frame_size_in_mb | default | integer | 256
native_transport_max_threads | default | integer | 128
native_transport_port | config | integer | 9042
native_transport_port_proxy_protocol | default | integer | 0
native_transport_port_ssl | default | integer | 9142
native_transport_port_ssl_proxy_protocol | default | integer | 0
nodeops_watchdog_timeout_seconds | default | integer | 120
num_tokens | config | integer | 256
partitioner | config | string | "org.apache.cassandra.dht.Murmur3Partitioner"
permissions_cache_max_entries | default | integer | 1000
permissions_update_interval_in_ms | default | integer | 2000
permissions_validity_in_ms | default | integer | 10000
phi_convict_threshold | default | integer | 8
preheat_kernel_page_cache | default | bool | false
prometheus_allow_protobuf | default | bool | false
query_page_size_in_bytes | default | integer | 1048576
query_tombstone_page_limit | default | integer | 10000
range_request_timeout_in_ms | default | integer | 10000
read_request_timeout_in_ms | config | integer | 5000
reader_concurrency_semaphore_kill_limit_multiplier | default | integer | 4
reader_concurrency_semaphore_serialize_limit_multiplier | default | integer | 2
replication_strategy_fail_list | default | replication strategy list | []
replication_strategy_warn_list | default | replication strategy list | ["SimpleStrategy"]
request_timeout_in_ms | default | integer | 10000
request_timeout_on_shutdown_in_seconds | default | integer | 30
restrict_dtcs | default | restriction mode | "1"
restrict_future_timestamp | default | bool | true
restrict_mixed_storage_clusters | default | restriction mode | "1"
restrict_replication_simplestrategy | default | restriction mode | "0"
restrict_twcs_without_default_ttl | default | restriction mode | "warn"
rf_rack_valid_keyspaces | config | bool | false
role_manager | default | string | "org.apache.cassandra.auth.CassandraRoleManager"
rows_count_fail_threshold | config | integer | 200000
rpc_address | cli | string | "<ip>"
schema_commitlog_directory | internal | string | "/var/lib/scylla/commitlog/schema"
schema_commitlog_segment_size_in_mb | config | integer | 128
schema_registry_grace_period | default | integer | 1
select_internal_page_size | default | integer | 10000
size_based_balance_threshold_percentage | default | float | 1
skip_wait_for_gossip_to_settle | default | integer | -1
sstable_compression_dictionaries_allow_in_ddl | default | bool | true
sstable_format | config | string | "mt"
sstable_summary_max_partitions_per_page | default | integer | 10000
start_native_transport | default | bool | true
streaming_socket_timeout_in_ms | default | integer | 0
strict_allow_filtering | default | restriction mode | "warn"
strict_is_not_null_in_views | config | restriction mode | "true"
tablet_load_stats_refresh_interval_in_seconds | default | integer | 60
tablet_streaming_read_concurrency_per_shard | default | integer | 2
tablet_streaming_write_concurrency_per_shard | default | integer | 2
tablets_initial_scale_factor | default | double | 10
tablets_mode_for_new_keyspaces | config | tablets mode | "enabled"
tablets_per_shard_goal | default | integer | 100
target_tablet_size_in_bytes | default | integer | 5368709120
task_ttl_in_seconds | default | integer | 0
tombstone_failure_threshold | default | integer | 100000
tombstone_warn_threshold | default | integer | 1000
topology_barrier_stall_detector_threshold_seconds | default | double | 2
truncate_request_timeout_in_ms | default | integer | 60000
twcs_max_window_count | default | integer | 50
unspooled_dirty_soft_limit | default | double | 0.6
user_defined_function_allocation_limit_bytes | default | integer | 1048576
user_defined_function_contiguous_allocation_limit_bytes | default | integer | 1048576
user_defined_function_time_limit_ms | default | integer | 10
user_task_ttl_in_seconds | default | integer | 3600
uuid_sstable_identifiers_enabled | default | bool | true
vector_store_encryption_options | default | string map | {}
vector_store_primary_uri | default | string | ""
vector_store_secondary_uri | default | string | ""
vector_store_unreachable_node_detection_time_in_ms | default | integer | 3000
view_flow_control_delay_limit_in_ms | default | integer | 1000
view_update_reader_concurrency_semaphore_kill_limit_multiplier | default | integer | 4
view_update_reader_concurrency_semaphore_serialize_limit_multiplier | default | integer | 2
wasm_cache_instance_size_limit | default | integer | 1048576
wasm_cache_timeout_in_ms | default | integer | 5000
wasm_udf_memory_limit | default | integer | 2097152
wasm_udf_total_fuel | default | integer | 100000000
wasm_udf_yield_fuel | default | integer | 100000
write_consistency_levels_disallowed | default | consistency level list | []
write_consistency_levels_warned | default | consistency level list | []
write_request_timeout_in_ms | config | integer | 2000
```
### Cassandra 5.0.9 (`system_views.settings`, 638 rows in total)

```
name | value
allocate_tokens_for_local_replication_factor | 3
allow_extra_insecure_udfs | false
allow_filtering_enabled | true
allow_insecure_udfs | false
alter_table_enabled | true
audit_logging_options.allow_nodetool_archive_command | false
audit_logging_options.max_archive_retries | 10
audit_logging_options.max_log_size | 17179869184
audit_logging_options.max_queue_weight | 268435456
audit_logging_options_enabled | false
auth_cache_warming_enabled | false
auth_read_consistency_level | LOCAL_QUORUM
auth_write_consistency_level | EACH_QUORUM
authenticator | AllowAllAuthenticator
authenticator.class_name | AllowAllAuthenticator
authenticator.parameters | {}
authorizer | AllowAllAuthorizer
authorizer.class_name | AllowAllAuthorizer
authorizer.parameters | {}
auto_hints_cleanup_enabled | false
auto_repair.auto_repair_scheduling_enabled | false
auto_repair.global_settings.allow_parallel_replica_repair | false
auto_repair.global_settings.allow_parallel_replica_repair_across_schedules | true
auto_repair.global_settings.materialized_view_repair_enabled | false
auto_repair.global_settings.repair_max_retries | 3
auto_repair.global_settings.repair_session_timeout | 3h
auto_repair.global_settings.sstable_upper_threshold | 50000
auto_repair.global_settings.table_max_repair_time | 6h
auto_repair.mixed_major_version_repair_enabled | false
auto_snapshot | true
auto_snapshot_ttl | None
autocompaction_on_startup_enabled | true
back_pressure_enabled | false
batch_size_fail_threshold | 50KiB
batch_size_fail_threshold_in_kb | 50
batch_size_warn_threshold | 5KiB
batch_size_warn_threshold_in_kb | 5
batchlog_replay_throttle | 1024KiB
batchlog_replay_throttle_in_kb | 1024
block_for_peers_timeout_in_secs | 10
broadcast_address | <ip>
broadcast_rpc_address | <ip>
cache_load_timeout | 30s
cache_load_timeout_seconds | 30
cas_contention_timeout | 1000ms
cas_contention_timeout_in_ms | 1000
cdc_block_writes | true
cdc_enabled | false
cdc_free_space_check_interval | 250ms
cdc_free_space_check_interval_ms | 250
cdc_on_repair_enabled | true
cdc_raw_directory | None
cdc_total_space | 0MiB
cdc_total_space_in_mb | 0
cidr_authorizer.class_name | AllowAllCIDRAuthorizer
cidr_authorizer.parameters | None
client_request_size_metrics_enabled | true
cluster_name | Test Cluster
collection_size_fail_threshold | None
collection_size_warn_threshold | None
column_value_size_fail_threshold | None
column_value_size_warn_threshold | None
columns_per_table_fail_threshold | -1
columns_per_table_warn_threshold | -1
commit_failure_policy | stop
commitlog_max_compression_buffers_in_pool | 3
commitlog_sync | periodic
commitlog_sync_group_window | 0ms
commitlog_sync_group_window_in_ms | 0.0
commitlog_sync_period | 10000ms
commitlog_sync_period_in_ms | 10000
compact_tables_enabled | true
compaction_large_partition_warning_threshold | None
compaction_large_partition_warning_threshold_mb | None
compaction_tombstone_warning_threshold | -1
concurrent_materialized_view_builders | 1
concurrent_materialized_view_writes | 32
consecutive_message_errors_threshold | 1
coordinator_read_size_fail_threshold | None
coordinator_read_size_warn_threshold | None
corrupted_tombstone_strategy | disabled
counter_write_request_timeout | 5000ms
counter_write_request_timeout_in_ms | 5000
cql_start_time | REQUEST
credentials_cache_active_update | false
credentials_cache_max_entries | 1000
credentials_update_interval | None
credentials_update_interval_in_ms | -1
credentials_validity | 2000ms
credentials_validity_in_ms | 2000
cross_node_timeout | true
data_disk_usage_max_disk_size | None
data_disk_usage_percentage_fail_threshold | -1
data_disk_usage_percentage_warn_threshold | -1
default_compaction.class_name | None
default_compaction.parameters | None
default_keyspace_rf | 1
default_secondary_index | legacy_local_table
default_secondary_index_enabled | true
denylist_consistency_level | QUORUM
denylist_max_keys_per_table | 1000
denylist_max_keys_total | 10000
denylist_range_reads_enabled | true
denylist_reads_enabled | true
denylist_writes_enabled | true
diagnostic_events_enabled | false
disk_failure_policy | stop
disk_optimization_page_cross_chance | 0.1
drop_compact_storage_enabled | false
drop_keyspace_enabled | true
drop_truncate_table_enabled | true
dynamic_data_masking_enabled | false
dynamic_snitch | true
dynamic_snitch_badness_threshold | 1.0
dynamic_snitch_reset_interval | 600000ms
dynamic_snitch_reset_interval_in_ms | 600000
dynamic_snitch_update_interval | 100ms
dynamic_snitch_update_interval_in_ms | 100
enable_drop_compact_storage | false
enable_materialized_views | false
enable_sasi_indexes | false
enable_scripted_user_defined_functions | false
enable_transient_replication | false
enable_user_defined_functions | false
enable_user_defined_functions_threads | true
endpoint_snitch | SimpleSnitch
failure_detector | FailureDetector
fields_per_udt_fail_threshold | -1
fields_per_udt_warn_threshold | -1
file_cache_enabled | false
full_query_logging_options.allow_nodetool_archive_command | false
full_query_logging_options.archive_command | 
full_query_logging_options.block | true
full_query_logging_options.log_dir | 
full_query_logging_options.max_archive_retries | 10
full_query_logging_options.max_log_size | 17179869184
full_query_logging_options.max_queue_weight | 268435456
full_query_logging_options.roll_cycle | HOURLY
gc_log_threshold | 200ms
gc_log_threshold_in_ms | 200
gc_warn_threshold | 1s
gc_warn_threshold_in_ms | 1000
group_by_enabled | true
hint_window_persistent_enabled | true
hinted_handoff_enabled | true
hinted_handoff_throttle | 1024KiB
hinted_handoff_throttle_in_kb | 1024
ideal_consistency_level | None
in_select_cartesian_product_fail_threshold | -1
in_select_cartesian_product_warn_threshold | -1
initial_range_tombstone_list_allocation_size | 1
internode_authenticator | None
internode_authenticator.class_name | None
internode_authenticator.parameters | None
internode_max_message_size | 134217728B
internode_max_message_size_in_bytes | 134217728
internode_streaming_tcp_user_timeout | 300s
internode_streaming_tcp_user_timeout_in_ms | 300000
internode_tcp_connect_timeout | 2s
internode_tcp_connect_timeout_in_ms | 2000
internode_tcp_user_timeout | 30s
internode_tcp_user_timeout_in_ms | 30000
internode_timeout | true
intersect_filtering_query_enabled | true
intersect_filtering_query_warned | true
invalid_legacy_protocol_magic_no_spam_enabled | false
items_per_collection_fail_threshold | -1
items_per_collection_warn_threshold | -1
keyspace_count_warn_threshold | 4
keyspaces_fail_threshold | -1
keyspaces_warn_threshold | -1
listen_address | <ip>
listen_on_broadcast_address | false
local_read_size_fail_threshold | None
local_read_size_warn_threshold | None
materialized_views_enabled | false
materialized_views_on_repair_enabled | true
materialized_views_per_table_fail_threshold | -1
materialized_views_per_table_warn_threshold | -1
max_concurrent_automatic_sstable_upgrades | 1
max_hint_window | 3h
max_hint_window_in_ms | 10800000
max_hints_delivery_threads | 2
max_hints_file_size | 128MiB
max_hints_file_size_in_mb | 128
max_hints_size_per_host | 0B
max_mutation_size | 16384KiB
max_mutation_size_in_kb | 16384
max_space_usable_for_compactions_in_percentage | 0.95
max_streaming_retries | 3
max_top_size_partition_count | 10
max_top_tombstone_partition_count | 10
max_value_size | 256MiB
max_value_size_in_mb | 256
maximum_replication_factor_fail_threshold | -1
maximum_replication_factor_warn_threshold | -1
maximum_timestamp_fail_threshold | None
maximum_timestamp_warn_threshold | None
memtable_cleanup_threshold | 0.33333334
min_tracked_partition_size | 1MiB
min_tracked_partition_tombstone_count | 5000
minimum_replication_factor_fail_threshold | -1
minimum_replication_factor_warn_threshold | -1
minimum_timestamp_fail_threshold | None
minimum_timestamp_warn_threshold | None
native_transport_allow_older_protocols | true
native_transport_flush_in_batches_legacy | false
native_transport_idle_timeout | 0ms
native_transport_idle_timeout_in_ms | 0
native_transport_max_auth_threads | 4
native_transport_max_backoff_on_queue_overload | 200ms
native_transport_max_concurrent_connections | -1
native_transport_max_concurrent_connections_per_ip | -1
native_transport_max_concurrent_requests_in_bytes | 67108864
native_transport_max_concurrent_requests_in_bytes_per_ip | 16777216
native_transport_max_frame_size | 16MiB
native_transport_max_frame_size_in_mb | 16
native_transport_max_message_size | 16777216B
native_transport_max_request_data_in_flight | 67108864B
native_transport_max_request_data_in_flight_per_ip | 16777216B
native_transport_max_requests_per_second | 1000000
native_transport_max_threads | 128
native_transport_min_backoff_on_queue_overload | 10ms
native_transport_port | 9042
native_transport_port_ssl | None
native_transport_queue_max_item_age_threshold | 1.7976931348623157E308
native_transport_rate_limiting_enabled | false
native_transport_receive_queue_capacity | 1MiB
native_transport_receive_queue_capacity_in_bytes | 1048576
native_transport_throw_on_overload | false
native_transport_timeout | 12s
network_authorizer | AllowAllNetworkAuthorizer
network_authorizer.class_name | AllowAllNetworkAuthorizer
network_authorizer.parameters | {}
non_partition_restricted_index_query_enabled | true
num_tokens | 16
page_size_fail_threshold | -1
page_size_warn_threshold | -1
partition_denylist_enabled | false
partition_keys_in_select_fail_threshold | -1
partition_keys_in_select_warn_threshold | -1
partition_size_fail_threshold | None
partition_size_warn_threshold | None
partition_tombstones_fail_threshold | -1
partition_tombstones_warn_threshold | -1
partitioner | org.apache.cassandra.dht.Murmur3Partitioner
paxos_contention_max_wait | None
paxos_repair_enabled | true
paxos_topology_repair_strict_each_quorum | false
periodic_commitlog_sync_lag_block | None
periodic_commitlog_sync_lag_block_in_ms | None
permissions_cache_active_update | false
permissions_cache_max_entries | 1000
permissions_update_interval | None
permissions_update_interval_in_ms | -1
permissions_validity | 2000ms
permissions_validity_in_ms | 2000
phi_convict_threshold | 8.0
range_request_timeout | 10000ms
range_request_timeout_in_ms | 10000
range_tombstone_list_growth_factor | 1.5
read_before_write_list_operations_enabled | true
read_consistency_levels_disallowed | []
read_consistency_levels_warned | []
read_request_timeout | 5000ms
read_request_timeout_in_ms | 5000
read_thresholds_enabled | false
reject_out_of_token_range_requests | false
reject_repair_compaction_threshold | 2147483647
repair_disk_headroom_reject_ratio | 0.0
repair_request_timeout | 120000ms
repair_request_timeout_in_ms | 120000
repair_session_max_tree_depth | 20
repaired_data_tracking_for_partition_reads_enabled | false
repaired_data_tracking_for_range_reads_enabled | false
replica_filtering_protection.cached_rows_fail_threshold | 32000
replica_filtering_protection.cached_rows_warn_threshold | 2000
request_timeout | 10000ms
request_timeout_in_ms | 10000
role_manager | CassandraRoleManager
role_manager.class_name | CassandraRoleManager
role_manager.parameters | {}
roles_cache_active_update | false
roles_cache_max_entries | 1000
roles_update_interval | None
roles_update_interval_in_ms | -1
roles_validity | 2000ms
roles_validity_in_ms | 2000
row_index_read_size_fail_threshold | None
row_index_read_size_warn_threshold | None
rpc_address | 0.0.0.0
sai_frozen_term_size_fail_threshold | 8KiB
sai_frozen_term_size_warn_threshold | 1KiB
sai_options.prioritize_over_legacy_index | false
sai_options.segment_write_buffer_size | 1024MiB
sai_sstable_indexes_per_query_fail_threshold | -1
sai_sstable_indexes_per_query_warn_threshold | 32
sai_string_term_size_fail_threshold | 8KiB
sai_string_term_size_warn_threshold | 1KiB
sai_vector_term_size_fail_threshold | 32KiB
sai_vector_term_size_warn_threshold | 16KiB
sasi_indexes_enabled | false
scripted_user_defined_functions_enabled | false
secondary_indexes_enabled | true
secondary_indexes_per_table_fail_threshold | -1
secondary_indexes_per_table_warn_threshold | -1
server_encryption_options.legacy_ssl_storage_port_enabled | false
server_encryption_options_enabled | false
simplestrategy_enabled | true
slow_query_log_timeout | 500ms
slow_query_log_timeout_in_ms | 500
sstable_read_rate_persistence_enabled | false
start_native_transport | true
storage_compatibility_mode | CASSANDRA_4
stream_transfer_task_timeout | 12h
streaming_slow_events_log_timeout | 10s
streaming_stats_enabled | true
table_count_warn_threshold | 47
table_properties_warned | []
tables_fail_threshold | -1
tables_warn_threshold | -1
tombstone_failure_threshold | 100000
tombstone_warn_threshold | 1000
top_partitions_enabled | true
trace_type_query_ttl | 1d
trace_type_repair_ttl | 7d
tracetype_query_ttl | 86400
tracetype_repair_ttl | 604800
transient_replication_enabled | false
transparent_data_encryption_options_enabled | false
truncate_request_timeout | 60000ms
truncate_request_timeout_in_ms | 60000
uncompressed_tables_enabled | true
unlogged_batch_across_partitions_warn_threshold | 10
use_creation_time_for_hint_ttl | true
use_statements_enabled | true
user_defined_function_fail_timeout | 1500
user_defined_function_warn_timeout | 500
user_defined_functions_enabled | false
user_defined_functions_fail_timeout | 1500ms
user_defined_functions_threads_enabled | true
user_defined_functions_warn_timeout | 500ms
user_function_timeout_policy | die
user_timestamps_enabled | true
uuid_sstable_identifiers_enabled | false
vector_dimensions_fail_threshold | -1
vector_dimensions_warn_threshold | -1
vector_type_enabled | true
write_consistency_levels_disallowed | []
write_consistency_levels_warned | []
write_request_timeout | 2000ms
write_request_timeout_in_ms | 2000
zero_ttl_on_twcs_enabled | true
zero_ttl_on_twcs_warned | true
```
### Cassandra `system_views.system_properties`

```
name | value
JAVA_HOME | /opt/java/openjdk
cassandra-foreground | yes
cassandra.jmx.local.port | 7199
cassandra.libjemalloc | /usr/local/lib/libjemalloc.so
cassandra.logdir | /opt/cassandra/logs
cassandra.storagedir | /opt/cassandra/data
chronicle.analytics.disable | true
com.sun.management.jmxremote.authenticate | false
com.sun.management.jmxremote.password.file | /etc/cassandra/jmxremote.password
io.netty.transport.estimateSizeOnSubmit | false
java.class.path | /etc/cassandra:/opt/cassandra/lib/HdrHistogram-2.1.12.jar:/opt/cassandra/lib/ST4-4.0.8.jar:/opt/cassandra/lib/affinity-3.23.3.jar:/opt/cassandra/lib/agrona-1.17.1.jar:/opt/cassandra/lib/airline-0.8.jar:/opt/cassandra/lib/antlr-runtime-3.5.2.jar:/opt/cassandra/lib/apache-cassandra-5.0.9.jar:/opt/cassandra/lib/asm-9.4.jar:/opt/cassandra/lib/big-math-2.3.0.jar:/opt/cassandra/lib/caffeine-3.1.8.jar:/opt/cassandra/lib/cassandra-driver-core-3.12.1-shaded.jar:/opt/cassandra/lib/chronicle-bytes-2.23.33.jar:/opt/cassandra/lib/chronicle-core-2.23.36.jar:/opt/cassandra/lib/chronicle-queue-5.23.37.jar:/opt/cassandra/lib/chronicle-threads-2.23.25.jar:/opt/cassandra/lib/chronicle-wire-2.23.39.jar:/opt/cassandra/lib/commons-cli-1.5.0.jar:/opt/cassandra/lib/commons-lang3-3.18.0.jar:/opt/cassandra/lib/commons-math3-3.2.jar:/opt/cassandra/lib/concurrent-trees-2.4.0.jar:/opt/cassandra/lib/ecj-3.33.0.jar:/opt/cassandra/lib/failureaccess-1.0.1.jar:/opt/cassandra/lib/guava-32.0.1-jre.jar:/opt/cassandra/lib/high-scale-lib-1.0.6.jar:/opt/cassandra/lib/hppc-0.8.1.jar:/opt/cassandra/lib/ipaddress-5.3.3.jar:/opt/cassandra/lib/j2objc-annotations-1.3.jar:/opt/cassandra/lib/jackson-annotations-2.19.2.jar:/opt/cassandra/lib/jackson-core-2.19.2.jar:/opt/cassandra/lib/jackson-databind-2.19.2.jar:/opt/cassandra/lib/jackson-datatype-jsr310-2.19.2.jar:/opt/cassandra/lib/jamm-0.4.0.jar:/opt/cassandra/lib/javax.inject-1.jar:/opt/cassandra/lib/jbcrypt-0.4.jar:/opt/cassandra/lib/jcl-over-slf4j-2.0.17.jar:/opt/cassandra/lib/jcommander-1.30.jar:/opt/cassandra/lib/jctools-core-3.1.0.jar:/opt/cassandra/lib/jffi-1.3.11-native.jar:/opt/cassandra/lib/jffi-1.3.11.jar:/opt/cassandra/lib/jna-5.13.0.jar:/opt/cassandra/lib/jna-platform-5.13.0.jar:/opt/cassandra/lib/jnr-a64asm-1.0.0.jar:/opt/cassandra/lib/jnr-constants-0.10.4.jar:/opt/cassandra/lib/jnr-ffi-2.2.13.jar:/opt/cassandra/lib/jnr-x86asm-1.0.2.jar:/opt/cassandra/lib/jsr305-2.0.2.jar:/opt/cassandra/lib/jvector-1.0.2.jar:/opt/cassandra/lib/jvm-attach-api-1.5.jar:/opt/cassandra/lib/log4j-over-slf4j-2.0.17.jar:/opt/cassandra/lib/logback-classic-1.5.18.jar:/opt/cassandra/lib/logback-core-1.5.18.jar:/opt/cassandra/lib/lucene-analysis-common-9.7.0.jar:/opt/cassandra/lib/lucene-core-9.7.0.jar:/opt/cassandra/lib/lz4-java-1.10.1.jar:/opt/cassandra/lib/metrics-core-4.2.28.jar:/opt/cassandra/lib/metrics-jvm-4.2.19.jar:/opt/cassandra/lib/metrics-logback-4.2.19.jar:/opt/cassandra/lib/mxdump-0.14.jar:/opt/cassandra/lib/netty-all-4.1.130.Final.jar:/opt/cassandra/lib/netty-buffer-4.1.130.Final.jar:/opt/cassandra/lib/netty-codec-4.1.130.Final.jar:/opt/cassandra/lib/netty-common-4.1.130.Final.jar:/opt/cassandra/lib/netty-handler-4.1.130.Final.jar:/opt/cassandra/lib/netty-handler-proxy-4.1.130.Final.jar:/opt/cassandra/lib/netty-handler-ssl-ocsp-4.1.130.Final.jar:/opt/cassandra/lib/netty-resolver-4.1.130.Final.jar:/opt/cassandra/lib/netty-tcnative-boringssl-static-2.0.70.Final-linux-aarch_64.jar:/opt/cassandra/lib/netty-tcnative-boringssl-static-2.0.70.Final-linux-x86_64.jar:/opt/cassandra/lib/netty-tcnative-boringssl-static-2.0.70.Final-osx-aarch_64.jar:/opt/cassandra/lib/netty-tcnative-boringssl-static-2.0.70.Final-osx-x86_64.jar:/opt/cassandra/lib/netty-tcnative-boringssl-static-2.0.70.Final.jar:/opt/cassandra/lib/netty-tcnative-classes-2.0.70.Final.jar:/opt/cassandra/lib/netty-transport-4.1.130.Final.jar:/opt/cassandra/lib/netty-transport-classes-epoll-4.1.130.Final.jar:/opt/cassandra/lib/netty-transport-classes-kqueue-4.1.130.Final.jar:/opt/cassandra/lib/netty-transport-native-epoll-4.1.130.Final-linux-aarch_64.jar:/opt/cassandra/lib/netty-transport-native-epoll-4.1.130.Final-linux-x86_64.jar:/opt/cassandra/lib/netty-transport-native-epoll-4.1.130.Final.jar:/opt/cassandra/lib/netty-transport-native-unix-common-4.1.130.Final.jar:/opt/cassandra/lib/ohc-core-0.5.1.jar:/opt/cassandra/lib/ohc-core-j8-0.5.1.jar:/opt/cassandra/lib/posix-2.24ea4.jar:/opt/cassandra/lib/psjava-0.1.19.jar:/opt/cassandra/lib/semver4j-3.1.0.jar:/opt/cassandra/lib/sigar-1.6.4.jar:/opt/cassandra/lib/sjk-cli-0.14.jar:/opt/cassandra/lib/sjk-core-0.14.jar:/opt/cassandra/lib/sjk-json-0.14.jar:/opt/cassandra/lib/sjk-stacktrace-0.14.jar:/opt/cassandra/lib/slf4j-api-2.0.17.jar:/opt/cassandra/lib/snakeyaml-2.1.jar:/opt/cassandra/lib/snappy-java-1.1.10.4.jar:/opt/cassandra/lib/stream-2.5.2.jar:/opt/cassandra/lib/zstd-jni-1.5.7-2.jar:/opt/cassandra/lib/jsr223/*/*.jar::/opt/cassandra/lib/x86_64/AmazonCorrettoCryptoProvider-2.2.0-linux-x86_64.jar
java.home | /opt/java/openjdk
java.io.tmpdir | /tmp
java.library.path | /opt/cassandra/lib/sigar-bin
java.rmi.server.hostname | 127.0.0.1
java.rmi.server.randomIDs | true
java.version | 17.0.20.1
java.vm.name | OpenJDK 64-Bit Server VM
line.separator | 

logback.configurationFile | logback.xml
os.arch | amd64
os.name | Linux
sun.arch.data.model | 64
sun.java.command | org.apache.cassandra.service.CassandraDaemon
user.home | /home/cassandra
```
