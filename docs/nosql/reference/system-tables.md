<!-- Generated from our own observations of the reference images (see conformance.md for the capture method). Do not edit by hand. -->

# Keyspaces and tables on a fresh node

Every system keyspace and table (including virtual ones) present on a fresh node. Other agents' user keyspaces are filtered out. Columns for the tables drivers and tools commonly read follow; rows are summarised (count, first rows with addresses/uuids masked) because they are volatile.

## ScyllaDB 2026.3.3

- `audit`: `audit_log`
- `system`: `IndexInfo`, `batchlog`, `batchlog_v2`, `built_views`, `cdc_generations_v3`, `cdc_local`, `cdc_streams`, `cdc_streams_history`, `cdc_streams_state`, `cdc_timestamps`, `client_routes`, `clients`, `cluster_status`, `commitlog_cleanups`, `compaction_history`, `compactions_in_progress`, `config`, `corrupt_data`, `dicts`, `discovery`, `group0_history`, `hints`, `large_cells`, `large_partitions`, `large_rows`, `load_per_node`, `local`, `paxos`, `peer_events`, `peers`, `protocol_servers`, `raft`, `raft_snapshot_config`, `raft_snapshots`, `raft_state`, `range_xfers`, `repair_history`, `repair_tasks`, `role_attributes`, `role_members`, `role_permissions`, `roles`, `runtime_info`, `scylla_local`, `scylla_table_schema_history`, `scylla_views_builds_in_progress`, `service_levels_v2`, `size_estimates`, `snapshots`, `sstable_activity`, `sstables`, `tablet_sizes`, `tablets`, `token_ring`, `topology`, `topology_requests`, `truncated`, `versions`, `view_build_status_v2`, `view_building_tasks`, `views_builds_in_progress`
- `system_distributed`: `cdc_generation_timestamps`, `cdc_streams_descriptions_v2`, `snapshot_keyspaces`, `snapshot_nodes`, `snapshot_remote_locations`, `snapshot_sstables`, `snapshot_tables`, `snapshot_tablets`, `snapshots`, `view_build_status`
- `system_replicated_keys`: `encrypted_keys`
- `system_schema`: `aggregates`, `columns`, `computed_columns`, `dropped_columns`, `functions`, `indexes`, `keyspaces`, `scylla_aggregates`, `scylla_keyspaces`, `scylla_tables`, `tables`, `triggers`, `types`, `view_virtual_columns`, `views`
- `system_traces`: `events`, `node_slow_log`, `node_slow_log_time_idx`, `sessions`, `sessions_time_idx`

### system.size_estimates

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | NONE | text |
| table_name | clustering | 0 | ASC | text |
| range_start | clustering | 1 | ASC | text |
| range_end | clustering | 2 | ASC | text |
| mean_partition_size | regular | -1 | NONE | bigint |
| partitions_count | regular | -1 | NONE | bigint |

1000 rows on a fresh node (first 5, masked):

```
keyspace_name | table_name | range_start | range_end | mean_partition_size | partitions_count
system_replicated_keys | encrypted_keys | -1066131549945567833 | -978048782294737565 | 0 | 0
system_replicated_keys | encrypted_keys | -1102178384441593671 | -1066131549945567833 | 0 | 0
system_replicated_keys | encrypted_keys | -1116721547422761544 | -1102178384441593671 | 0 | 0
system_replicated_keys | encrypted_keys | -1170385816353652701 | -1116721547422761544 | 0 | 0
system_replicated_keys | encrypted_keys | -1180746700727346454 | -1170385816353652701 | 0 | 0
```
### system.clients

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| address | partition_key | 0 | NONE | inet |
| port | clustering | 0 | ASC | int |
| client_type | clustering | 1 | ASC | text |
| client_options | regular | -1 | NONE | frozen<map<text, text>> |
| connection_stage | regular | -1 | NONE | text |
| driver_name | regular | -1 | NONE | text |
| driver_version | regular | -1 | NONE | text |
| hostname | regular | -1 | NONE | text |
| protocol_version | regular | -1 | NONE | int |
| scheduling_group | regular | -1 | NONE | text |
| shard_id | regular | -1 | NONE | int |
| ssl_cipher_suite | regular | -1 | NONE | text |
| ssl_enabled | regular | -1 | NONE | boolean |
| ssl_protocol | regular | -1 | NONE | text |
| username | regular | -1 | NONE | text |

2 rows on a fresh node (first 5, masked):

```
address | port | client_type | client_options | connection_stage | driver_name | driver_version | hostname | protocol_version | scheduling_group | shard_id | ssl_cipher_suite | ssl_enabled | ssl_protocol | username
<ip> | 57634 | cql | {'SCYLLA_LWT_ADD_METADATA_MARK': '2147483648', 'TABLETS_ROUTING_V1': '', 'DRIVER_VERSION': '3.29.7', 'CQL_VERSION': '3.3… (196 chars) | READY | ScyllaDB Python Driver | 3.29.7 | None | 4 | sl:driver | 0 | None | False | None | anonymous
<ip> | 57636 | cql | {'SCYLLA_LWT_ADD_METADATA_MARK': '2147483648', 'TABLETS_ROUTING_V1': '', 'DRIVER_VERSION': '3.29.7', 'CQL_VERSION': '3.3… (196 chars) | READY | ScyllaDB Python Driver | 3.29.7 | None | 4 | sl:default | 0 | None | False | None | anonymous
```
### system.config

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| name | partition_key | 0 | NONE | text |
| source | regular | -1 | NONE | text |
| type | regular | -1 | NONE | text |
| value | regular | -1 | NONE | text |

415 rows on a fresh node (first 5, masked):

```
name | source | type | value
row_cache_keys_to_save | default | integer | 0
strict_is_not_null_in_views | config | restriction mode | "true"
max_memory_for_unlimited_query_hard_limit | default | integer | 104857600
enable_tablets | default | bool | false
alternator_encryption_options | default | string map | {}
```
### system.tablets

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| table_id | partition_key | 0 | NONE | uuid |
| last_token | clustering | 0 | ASC | bigint |
| base_table | static | -1 | NONE | uuid |
| isolated_tablet_for_merge | static | -1 | NONE | bigint |
| keyspace_name | static | -1 | NONE | text |
| repair_scheduler_config | static | -1 | NONE | frozen<repair_scheduler_config> |
| resize_seq_number | static | -1 | NONE | bigint |
| resize_task_info | static | -1 | NONE | frozen<tablet_task_info> |
| resize_type | static | -1 | NONE | text |
| selected_tablets_for_merge | static | -1 | NONE | frozen<list<bigint>> |
| table_name | static | -1 | NONE | text |
| tablet_count | static | -1 | NONE | int |
| target_pow2_tablet_count | static | -1 | NONE | bigint |
| migration_task_info | regular | -1 | NONE | frozen<tablet_task_info> |
| new_replicas | regular | -1 | NONE | frozen<list<frozen<tuple<uuid, int>>>> |
| repair_incremental_mode | regular | -1 | NONE | text |
| repair_task_info | regular | -1 | NONE | frozen<tablet_task_info> |
| repair_time | regular | -1 | NONE | timestamp |
| replicas | regular | -1 | NONE | frozen<list<frozen<tuple<uuid, int>>>> |
| session | regular | -1 | NONE | uuid |
| snapshot_name | regular | -1 | NONE | text |
| sstables_repaired_at | regular | -1 | NONE | bigint |
| stage | regular | -1 | NONE | text |
| transition | regular | -1 | NONE | text |

16 rows on a fresh node (first 5, masked):

```
table_id | last_token | base_table | isolated_tablet_for_merge | keyspace_name | repair_scheduler_config | resize_seq_number | resize_task_info | resize_type | selected_tablets_for_merge | table_name | tablet_count | target_pow2_tablet_count | migration_task_info | new_replicas | repair_incremental_mode | repair_task_info | repair_time | replicas | session | snapshot_name | sstables_repaired_at | stage | transition
<uuid> | -8070450532247928833 | None | None | cql_ks | None | 0 | None | none | None | t | 16 | None | None | None | None | None | None | [['<uuid>', 0]] | None | None | 0 | None | None
<uuid> | -6917529027641081857 | None | None | cql_ks | None | 0 | None | none | None | t | 16 | None | None | None | None | None | None | [['<uuid>', 0]] | None | None | 0 | None | None
<uuid> | -5764607523034234881 | None | None | cql_ks | None | 0 | None | none | None | t | 16 | None | None | None | None | None | None | [['<uuid>', 0]] | None | None | 0 | None | None
<uuid> | -4611686018427387905 | None | None | cql_ks | None | 0 | None | none | None | t | 16 | None | None | None | None | None | None | [['<uuid>', 0]] | None | None | 0 | None | None
<uuid> | -3458764513820540929 | None | None | cql_ks | None | 0 | None | none | None | t | 16 | None | None | None | None | None | None | [['<uuid>', 0]] | None | None | 0 | None | None
```
### system.scylla_local

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| key | partition_key | 0 | NONE | text |
| value | regular | -1 | NONE | text |

8 rows on a fresh node (first 5, masked):

```
key | value
enabled_features | ADDRESS_NODES_BY_HOST_IDS,AGGREGATE_STORAGE_OPTIONS,ALTERNATOR_STREAMS,ALTERNATOR_TTL,ARBITRARY_TABLET_BOUNDARIES,BATCHL… (2740 chars)
auth_version | 2
raft_group0_id | <uuid>
service_level_version | 2
service_level_driver_created | true
```
### system.versions

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| key | partition_key | 0 | NONE | text |
| build_id | regular | -1 | NONE | text |
| build_mode | regular | -1 | NONE | text |
| version | regular | -1 | NONE | text |

1 rows on a fresh node (first 5, masked):

```
key | build_id | build_mode | version
local | d0d29ed4522f3714b887a0841c08d59f92b273fc | release | 2026.3.3-0.20261004.d80b3d6ca26b
```
### system.roles

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| role | partition_key | 0 | NONE | text |
| can_login | regular | -1 | NONE | boolean |
| is_superuser | regular | -1 | NONE | boolean |
| member_of | regular | -1 | NONE | set<text> |
| salted_hash | regular | -1 | NONE | text |

0 rows on a fresh node.

### system.role_members

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| role | partition_key | 0 | NONE | text |
| member | clustering | 0 | ASC | text |

0 rows on a fresh node.

### system.role_permissions

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| role | partition_key | 0 | NONE | text |
| resource | clustering | 0 | ASC | text |
| permissions | regular | -1 | NONE | set<text> |

0 rows on a fresh node.

### system.role_attributes

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| role | partition_key | 0 | NONE | text |
| name | clustering | 0 | ASC | text |
| value | regular | -1 | NONE | text |

0 rows on a fresh node.

### system.service_levels_v2

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| service_level | partition_key | 0 | NONE | text |
| shares | regular | -1 | NONE | int |
| timeout | regular | -1 | NONE | duration |
| workload_type | regular | -1 | NONE | text |

1 rows on a fresh node (first 5, masked):

```
service_level | shares | timeout | workload_type
driver | 200 | None | batch
```
### system.topology

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| key | partition_key | 0 | NONE | text |
| host_id | clustering | 0 | ASC | uuid |
| committed_cdc_generations | static | -1 | NONE | set<frozen<tuple<timestamp, timeuuid>>> |
| enabled_features | static | -1 | NONE | set<text> |
| fence_version | static | -1 | NONE | bigint |
| global_requests | static | -1 | NONE | set<timeuuid> |
| global_topology_request | static | -1 | NONE | text |
| global_topology_request_id | static | -1 | NONE | timeuuid |
| ignore_nodes | static | -1 | NONE | set<uuid> |
| new_cdc_generation_data_uuid | static | -1 | NONE | timeuuid |
| new_keyspace_rf_change_data | static | -1 | NONE | frozen<map<text, text>> |
| new_keyspace_rf_change_ks_name | static | -1 | NONE | text |
| ongoing_restore_requests | static | -1 | NONE | set<timeuuid> |
| ongoing_rf_changes | static | -1 | NONE | set<timeuuid> |
| paused_rf_change_requests | static | -1 | NONE | set<timeuuid> |
| session | static | -1 | NONE | uuid |
| tablet_balancing_enabled | static | -1 | NONE | boolean |
| transition_state | static | -1 | NONE | text |
| unpublished_cdc_generations | static | -1 | NONE | set<frozen<tuple<timestamp, timeuuid>>> |
| upgrade_state | static | -1 | NONE | text |
| version | static | -1 | NONE | bigint |
| cleanup_status | regular | -1 | NONE | text |
| datacenter | regular | -1 | NONE | text |
| ignore_msb | regular | -1 | NONE | int |
| intended_storage_mode | regular | -1 | NONE | text |
| node_state | regular | -1 | NONE | text |
| num_tokens | regular | -1 | NONE | int |
| rack | regular | -1 | NONE | text |
| rebuild_option | regular | -1 | NONE | text |
| release_version | regular | -1 | NONE | text |
| replaced_id | regular | -1 | NONE | uuid |
| request_id | regular | -1 | NONE | timeuuid |
| shard_count | regular | -1 | NONE | int |
| supported_features | regular | -1 | NONE | set<text> |
| tokens | regular | -1 | NONE | set<text> |
| tokens_string | regular | -1 | NONE | text |
| topology_request | regular | -1 | NONE | text |

1 rows on a fresh node (first 5, masked):

```
key | host_id | committed_cdc_generations | enabled_features | fence_version | global_requests | global_topology_request | global_topology_request_id | ignore_nodes | new_cdc_generation_data_uuid | new_keyspace_rf_change_data | new_keyspace_rf_change_ks_name | ongoing_restore_requests | ongoing_rf_changes | paused_rf_change_requests | session | tablet_balancing_enabled | transition_state | unpublished_cdc_generations | upgrade_state | version | cleanup_status | datacenter | ignore_msb | intended_storage_mode | node_state | num_tokens | rack | rebuild_option | release_version | replaced_id | request_id | shard_count | supported_features | tokens | tokens_string | topology_request
topology | <uuid> | SortedSet([(datetime.datetime(2026, 10, 20, 18, 33, 6, 228000), UUID('<uuid>'))]) | SortedSet(['ADDRESS_NODES_BY_HOST_IDS', 'AGGREGATE_STORAGE_OPTIONS', 'ALTERNATOR_STREAMS', 'ALTERNATOR_TTL', 'ARBITRARY_… (3106 chars) | 3 | None | None | None | None | <uuid> | None | None | None | None | None | None | None | None | None | done | 4 | clean | datacenter1 | 12 | None | normal | 256 | rack1 | None | 3.0.8 | None | <uuid> | 1 | SortedSet(['ADDRESS_NODES_BY_HOST_IDS', 'AGGREGATE_STORAGE_OPTIONS', 'ALTERNATOR_STREAMS', 'ALTERNATOR_TTL', 'ARBITRARY_… (3106 chars) | SortedSet(['-1066131549945567833', '-1102178384441593671', '-1116721547422761544', '-1170385816353652701', '-11807467007… (6001 chars) |  | None
```
### system.token_ring

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | NONE | text |
| table_name | clustering | 0 | ASC | text |
| start_token | clustering | 1 | ASC | text |
| endpoint | clustering | 2 | ASC | inet |
| dc | regular | -1 | NONE | text |
| end_token | regular | -1 | NONE | text |
| rack | regular | -1 | NONE | text |

1000 rows on a fresh node (first 5, masked):

```
keyspace_name | table_name | start_token | endpoint | dc | end_token | rack
system_replicated_keys | <ALL> | -1066131549945567833 | <ip> | datacenter1 | -978048782294737565 | rack1
system_replicated_keys | <ALL> | -1102178384441593671 | <ip> | datacenter1 | -1066131549945567833 | rack1
system_replicated_keys | <ALL> | -1116721547422761544 | <ip> | datacenter1 | -1102178384441593671 | rack1
system_replicated_keys | <ALL> | -1170385816353652701 | <ip> | datacenter1 | -1116721547422761544 | rack1
system_replicated_keys | <ALL> | -1180746700727346454 | <ip> | datacenter1 | -1170385816353652701 | rack1
```
### system.runtime_info

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| group | partition_key | 0 | NONE | text |
| item | clustering | 0 | ASC | text |
| value | regular | -1 | NONE | text |

22 rows on a fresh node (first 5, masked):

```
group | item | value
cache | entries | 65
cache | hit_rate_recent | 84
cache | hit_rate_total | 1
cache | hits | 13930
cache | memory_free | 1909334
```
### system.protocol_servers

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| name | partition_key | 0 | NONE | text |
| listen_addresses | regular | -1 | NONE | frozen<list<text>> |
| protocol | regular | -1 | NONE | text |
| protocol_version | regular | -1 | NONE | text |

2 rows on a fresh node (first 5, masked):

```
name | listen_addresses | protocol | protocol_version
native transport | ['<ip>:9042', '<ip>:19042'] | cql | 3.3.1
alternator | ['<ip>:8000'] | dynamodb | 2012-08-10
```
### system.cluster_status

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| peer | partition_key | 0 | NONE | inet |
| dc | regular | -1 | NONE | text |
| draining | regular | -1 | NONE | boolean |
| excluded | regular | -1 | NONE | boolean |
| host_id | regular | -1 | NONE | uuid |
| load | regular | -1 | NONE | text |
| owns | regular | -1 | NONE | float |
| rack | regular | -1 | NONE | text |
| status | regular | -1 | NONE | text |
| tokens | regular | -1 | NONE | int |
| up | regular | -1 | NONE | boolean |

1 rows on a fresh node (first 5, masked):

```
peer | dc | draining | excluded | host_id | load | owns | rack | status | tokens | up
<ip> | datacenter1 | False | False | <uuid> | 346044 | 1.0 | rack1 | NORMAL | 256 | True
```
### system.load_per_node

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| node | partition_key | 0 | NONE | uuid |
| dc | regular | -1 | NONE | text |
| draining | regular | -1 | NONE | boolean |
| effective_capacity | regular | -1 | NONE | bigint |
| excluded | regular | -1 | NONE | boolean |
| ip | regular | -1 | NONE | inet |
| rack | regular | -1 | NONE | text |
| storage_allocated_load | regular | -1 | NONE | bigint |
| storage_allocated_utilization | regular | -1 | NONE | double |
| storage_capacity | regular | -1 | NONE | bigint |
| storage_load | regular | -1 | NONE | bigint |
| storage_utilization | regular | -1 | NONE | double |
| tablets_allocated | regular | -1 | NONE | bigint |
| tablets_allocated_per_shard | regular | -1 | NONE | double |
| up | regular | -1 | NONE | boolean |

1 rows on a fresh node (first 5, masked):

```
node | dc | draining | effective_capacity | excluded | ip | rack | storage_allocated_load | storage_allocated_utilization | storage_capacity | storage_load | storage_utilization | tablets_allocated | tablets_allocated_per_shard | up
<uuid> | datacenter1 | False | 614107455488 | False | <ip> | rack1 | 85899345920 | 0.1398767351745309 | 784364208128 | 0 | 0.0 | 16 | 16.0 | True
```
### system.snapshots

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | NONE | text |
| table_name | clustering | 0 | ASC | text |
| snapshot_name | clustering | 1 | ASC | text |
| live | regular | -1 | NONE | bigint |
| total | regular | -1 | NONE | bigint |

0 rows on a fresh node.

### system.large_partitions

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | NONE | text |
| table_name | partition_key | 1 | NONE | text |
| sstable_name | clustering | 0 | ASC | text |
| partition_size | clustering | 1 | DESC | bigint |
| partition_key | clustering | 2 | ASC | text |
| compaction_time | regular | -1 | NONE | timestamp |
| dead_rows | regular | -1 | NONE | bigint |
| range_tombstones | regular | -1 | NONE | bigint |
| rows | regular | -1 | NONE | bigint |

0 rows on a fresh node.

### system.large_rows

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | NONE | text |
| table_name | partition_key | 1 | NONE | text |
| sstable_name | clustering | 0 | ASC | text |
| row_size | clustering | 1 | DESC | bigint |
| partition_key | clustering | 2 | ASC | text |
| clustering_key | clustering | 3 | ASC | text |
| compaction_time | regular | -1 | NONE | timestamp |

0 rows on a fresh node.

### system.large_cells

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | NONE | text |
| table_name | partition_key | 1 | NONE | text |
| sstable_name | clustering | 0 | ASC | text |
| cell_size | clustering | 1 | DESC | bigint |
| partition_key | clustering | 2 | ASC | text |
| clustering_key | clustering | 3 | ASC | text |
| column_name | clustering | 4 | ASC | text |
| collection_elements | regular | -1 | NONE | bigint |
| compaction_time | regular | -1 | NONE | timestamp |

0 rows on a fresh node.

### system.built_views

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | NONE | text |
| view_name | clustering | 0 | ASC | text |

0 rows on a fresh node.

### system.view_build_status_v2

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | NONE | text |
| view_name | partition_key | 1 | NONE | text |
| host_id | clustering | 0 | ASC | uuid |
| status | regular | -1 | NONE | text |

0 rows on a fresh node.

### system.cdc_local

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| key | partition_key | 0 | NONE | text |
| streams_timestamp | regular | -1 | NONE | timestamp |
| uuid | regular | -1 | NONE | uuid |

0 rows on a fresh node.

### system.paxos

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| row_key | partition_key | 0 | NONE | blob |
| cf_id | clustering | 0 | ASC | uuid |
| most_recent_commit | regular | -1 | NONE | blob |
| most_recent_commit_at | regular | -1 | NONE | timeuuid |
| promise | regular | -1 | NONE | timeuuid |
| proposal | regular | -1 | NONE | blob |
| proposal_ballot | regular | -1 | NONE | timeuuid |

0 rows on a fresh node.

### system.compaction_history

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| id | partition_key | 0 | NONE | uuid |
| bytes_in | regular | -1 | NONE | bigint |
| bytes_out | regular | -1 | NONE | bigint |
| columnfamily_name | regular | -1 | NONE | text |
| compacted_at | regular | -1 | NONE | timestamp |
| compaction_type | regular | -1 | NONE | text |
| keyspace_name | regular | -1 | NONE | text |
| rows_merged | regular | -1 | NONE | map<int, bigint> |
| shard_id | regular | -1 | NONE | int |
| sstables_in | regular | -1 | NONE | frozen<list<frozen<sstableinfo>>> |
| sstables_out | regular | -1 | NONE | frozen<list<frozen<sstableinfo>>> |
| started_at | regular | -1 | NONE | timestamp |
| total_tombstone_purge_attempt | regular | -1 | NONE | bigint |
| total_tombstone_purge_failure_due_to_overlapping_with_memtable | regular | -1 | NONE | bigint |
| total_tombstone_purge_failure_due_to_overlapping_with_uncompacting_sstable | regular | -1 | NONE | bigint |

30 rows on a fresh node (first 5, masked):

```
id | bytes_in | bytes_out | columnfamily_name | compacted_at | compaction_type | keyspace_name | rows_merged | shard_id | sstables_in | sstables_out | started_at | total_tombstone_purge_attempt | total_tombstone_purge_failure_due_to_overlapping_with_memtable | total_tombstone_purge_failure_due_to_overlapping_with_uncompacting_sstable
<uuid> | 12872 | 6463 | aggregates | 2026-10-20 18:33:44.582000 | Compact | system_schema | {1: 6} | 0 | [['<uuid>', 'memtable', 6404], ['<uuid>', 'memtable', 6468]] | [['<uuid>', 'compaction', 6463]] | 2026-10-20 18:33:44.555000 | 0 | 0 | 0
<uuid> | 65497 | 59422 | raft | 2026-10-20 18:33:06.577000 | Compact | system | {1: 7, 2: 3} | 0 | [['<uuid>', 'memtable', 22993], ['<uuid>', 'memtable', 42504… (122 chars) | [['<uuid>', 'compaction', 59422]] | 2026-10-20 18:33:06.398000 | 0 | 0 | 0
<uuid> | 12001 | 6060 | dropped_columns | 2026-10-20 18:33:06.886000 | Compact | system_schema | {1: 14} | 0 | [['<uuid>', 'memtable', 5900], ['<uuid>', 'memtable', 6101]] | [['<uuid>', 'compaction', 6060]] | 2026-10-20 18:33:06.797000 | 0 | 0 | 0
<uuid> | 12635 | 6494 | types | 2026-10-20 18:34:05.346000 | Compact | system_schema | {1: 7, 2: 2} | 0 | [['<uuid>', 'memtable', 6135], ['<uuid>', 'memtable', 6500]] | [['<uuid>', 'compaction', 6494]] | 2026-10-20 18:34:05.321000 | 0 | 0 | 0
<uuid> | 11906 | 6028 | truncated | 2026-10-20 18:33:05.883000 | Compact | system | {1: 8} | 0 | [['<uuid>', 'memtable', 5949], ['<uuid>', 'compaction', 5957… (122 chars) | [['<uuid>', 'compaction', 6028]] | 2026-10-20 18:33:05.801000 | 0 | 0 | 0
```
### system_traces.sessions

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| session_id | partition_key | 0 | NONE | uuid |
| client | regular | -1 | NONE | inet |
| command | regular | -1 | NONE | text |
| coordinator | regular | -1 | NONE | inet |
| duration | regular | -1 | NONE | int |
| parameters | regular | -1 | NONE | map<text, text> |
| request | regular | -1 | NONE | text |
| request_size | regular | -1 | NONE | int |
| response_size | regular | -1 | NONE | int |
| started_at | regular | -1 | NONE | timestamp |
| username | regular | -1 | NONE | text |

1 rows on a fresh node (first 5, masked):

```
session_id | client | command | coordinator | duration | parameters | request | request_size | response_size | started_at | username
<uuid> | <ip> | QUERY | <ip> | 117 | {'consistency_level': 'ONE', 'query': 'SELECT key FROM system.local', 'serial_consistency_level': 'SERIAL', 'table': 'sy… (174 chars) | Execute CQL3 query | 35 | 63 | 2026-10-20 18:35:45.677000 | anonymous
```
### system_traces.events

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| session_id | partition_key | 0 | NONE | uuid |
| event_id | clustering | 0 | ASC | timeuuid |
| activity | regular | -1 | NONE | text |
| scylla_parent_id | regular | -1 | NONE | bigint |
| scylla_span_id | regular | -1 | NONE | bigint |
| source | regular | -1 | NONE | inet |
| source_elapsed | regular | -1 | NONE | int |
| thread | regular | -1 | NONE | text |

11 rows on a fresh node (first 5, masked):

```
session_id | event_id | activity | scylla_parent_id | scylla_span_id | source | source_elapsed | thread
<uuid> | <uuid> | Parsing a statement | 0 | 375895580420903 | <ip> | 0 | shard 0/sl:driver
<uuid> | <uuid> | Processing a statement for authenticated user: anonymous | 0 | 375895580420903 | <ip> | 15 | shard 0/sl:driver
<uuid> | <uuid> | Executing read query (reversed false) | 0 | 375895580420903 | <ip> | 20 | shard 0/sl:driver
<uuid> | <uuid> | read_data: querying locally | 0 | 375895580420903 | <ip> | 23 | shard 0/sl:driver
<uuid> | <uuid> | Start querying singular range (-inf, +inf) | 0 | 375895580420903 | <ip> | 44 | shard 0/sl:driver
```
### system_traces.node_slow_log

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| start_time | partition_key | 0 | NONE | timeuuid |
| node_ip | clustering | 0 | ASC | inet |
| shard | clustering | 1 | ASC | int |
| command | regular | -1 | NONE | text |
| date | regular | -1 | NONE | timestamp |
| duration | regular | -1 | NONE | int |
| parameters | regular | -1 | NONE | map<text, text> |
| session_id | regular | -1 | NONE | uuid |
| source_ip | regular | -1 | NONE | inet |
| table_names | regular | -1 | NONE | set<text> |
| username | regular | -1 | NONE | text |

0 rows on a fresh node.

### system_traces.sessions_time_idx

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| minute | partition_key | 0 | NONE | timestamp |
| started_at | clustering | 0 | ASC | timestamp |
| session_id | clustering | 1 | ASC | uuid |

1 rows on a fresh node (first 5, masked):

```
minute | started_at | session_id
2026-10-20 18:35:00 | 2026-10-20 18:35:45.677000 | <uuid>
```
### system_distributed.view_build_status

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | NONE | text |
| view_name | partition_key | 1 | NONE | text |
| host_id | clustering | 0 | ASC | uuid |
| status | regular | -1 | NONE | text |

0 rows on a fresh node.

### system_distributed.cdc_streams_descriptions_v2

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| time | partition_key | 0 | NONE | timestamp |
| range_end | clustering | 0 | ASC | bigint |
| streams | regular | -1 | NONE | frozen<set<blob>> |

256 rows on a fresh node (first 5, masked):

```
time | range_end | streams
2026-10-20 18:33:06.228000 | -9157682465471492864 | SortedSet([b'\x7f\rg@\x95\xb9\xf3\x8dw\xfb\xdd\xf5\xe0\x00\x00\x01'])
2026-10-20 18:33:06.228000 | -9112348565548354993 | SortedSet([b'\x80\xe9`Q/\xbf\x8d\x01x\xa6\xc5\x15\xd4\x00\x00\x11'])
2026-10-20 18:33:06.228000 | -9057730211558655937 | SortedSet([b'\x81\x8aoBH\xf32P\xe3\xd0\xa4\xe3$\x00\x00!'])
2026-10-20 18:33:06.228000 | -8935514474700616985 | SortedSet([b'\x82Lz\\\xcc\xb8\xf8@`h\xd6\xae\xc4\x00\x001'])
2026-10-20 18:33:06.228000 | -8860125455525732872 | SortedSet([b'\x83\xfe\xac\xedz\x1b\xca\xe8\x070\xda\xe0,\x00\x00A'])
```
### system_distributed.cdc_generation_timestamps

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| key | partition_key | 0 | NONE | text |
| time | clustering | 0 | DESC | timestamp |
| expired | regular | -1 | NONE | timestamp |

1 rows on a fresh node (first 5, masked):

```
key | time | expired
timestamps | 2026-10-20 18:33:06.228000 | None
```
### Virtual tables

`system_virtual_schema` does not exist (``0x2200` Keyspace system_virtual_schema does not exist`). ScyllaDB lists its virtual tables in `system_schema.tables` like any other table.

### All other system tables (columns)

- `audit.audit_log`: date timestamp (partition_key), node inet (partition_key), event_time timeuuid (clustering), category text, consistency text, error boolean, keyspace_name text, operation text, source inet, table_name text, username text
- `system.IndexInfo`: table_name text (partition_key), index_name text (clustering), value empty
- `system.batchlog`: id uuid (partition_key), data blob, version int, written_at timestamp
- `system.batchlog_v2`: version int (partition_key), stage tinyint (partition_key), shard int (partition_key), written_at timestamp (clustering), id uuid (clustering), data blob
- `system.cdc_generations_v3`: key text (partition_key), id timeuuid (clustering), range_end bigint (clustering), ignore_msb tinyint, streams frozen<set<blob>>
- `system.cdc_streams`: keyspace_name text (partition_key), table_name text (partition_key), timestamp timestamp (clustering), stream_state tinyint (clustering), stream_id blob (clustering)
- `system.cdc_streams_history`: table_id uuid (partition_key), timestamp timestamp (clustering), stream_state tinyint (clustering), last_token bigint (clustering), stream_id blob
- `system.cdc_streams_state`: table_id uuid (partition_key), last_token bigint (clustering), timestamp timestamp (static), stream_id blob
- `system.cdc_timestamps`: keyspace_name text (partition_key), table_name text (partition_key), timestamp timestamp (clustering)
- `system.client_routes`: connection_id text (partition_key), host_id uuid (clustering), address text, alternator_https_port int, alternator_port int, port int, tls_port int
- `system.commitlog_cleanups`: shard int (partition_key), position frozen<tuple<bigint, int>> (clustering), table_uuid uuid (clustering), start_token_exclusive bigint (clustering), end_token_inclusive bigint (clustering)
- `system.compactions_in_progress`: id uuid (partition_key), columnfamily_name text, inputs set<int>, keyspace_name text
- `system.corrupt_data`: keyspace_name text (partition_key), table_name text (partition_key), id timeuuid (clustering), clustering_key blob, frozen_mutation_fragment blob, mutation_fragment_kind text, origin text, partition_key blob, sstable_name text
- `system.dicts`: name text (partition_key), data blob, origin uuid, timestamp timestamp
- `system.discovery`: key text (partition_key), ip_addr inet (clustering), raft_server_id uuid
- `system.group0_history`: key text (partition_key), state_id timeuuid (clustering), description text
- `system.hints`: target_id uuid (partition_key), hint_id timeuuid (clustering), message_version int (clustering), mutation blob
- `system.local`: key text (partition_key), bootstrapped text, broadcast_address inet, cluster_name text, cql_version text, data_center text, gossip_generation int, host_id uuid, listen_address inet, native_protocol_version text, partitioner text, rack text, release_version text, rpc_address inet, schema_version uuid, supported_features text, tokens set<text>, truncated_at map<uuid, blob>
- `system.peer_events`: peer inet (partition_key), hints_dropped map<uuid, int>
- `system.peers`: peer inet (partition_key), data_center text, host_id uuid, preferred_ip inet, rack text, release_version text, rpc_address inet, schema_version uuid, supported_features text, tokens set<text>
- `system.raft`: group_id timeuuid (partition_key), index bigint (clustering), commit_idx bigint (static), snapshot_id uuid (static), vote uuid (static), vote_term bigint (static), data blob, term bigint
- `system.raft_snapshot_config`: group_id timeuuid (partition_key), disposition ascii (clustering), server_id uuid (clustering), can_vote boolean
- `system.raft_snapshots`: group_id timeuuid (partition_key), idx bigint, snapshot_id uuid, term bigint
- `system.raft_state`: group_id timeuuid (partition_key), disposition ascii (clustering), server_id uuid (clustering), can_vote boolean
- `system.range_xfers`: token_bytes blob (partition_key), requested_at timestamp
- `system.repair_history`: table_uuid uuid (partition_key), repair_time timestamp (clustering), repair_uuid uuid (clustering), range_start bigint (clustering), range_end bigint (clustering), keyspace_name text (static), table_name text (static)
- `system.repair_tasks`: task_uuid uuid (partition_key), operation text (clustering), first_token bigint (clustering), last_token bigint (clustering), table_uuid uuid (static), timestamp timestamp
- `system.scylla_table_schema_history`: cf_id uuid (partition_key), schema_version uuid (clustering), column_name text (clustering), clustering_order text, column_name_bytes blob, kind text, position int, type text
- `system.scylla_views_builds_in_progress`: keyspace_name text (partition_key), view_name text (clustering), cpu_id int (clustering), first_token text, generation_number int, next_token text
- `system.sstable_activity`: keyspace_name text (partition_key), columnfamily_name text (partition_key), generation int (partition_key), rate_120m double, rate_15m double
- `system.sstables`: table_id uuid (partition_key), node_owner uuid (partition_key), generation timeuuid (clustering), format text, sstable_id uuid, state text, status text, version text
- `system.tablet_sizes`: table_id uuid (partition_key), last_token bigint (clustering), missing_replicas frozen<set<uuid>>, replicas frozen<map<uuid, bigint>>
- `system.topology_requests`: id timeuuid (partition_key), done boolean, end_time timestamp, error text, finalize_migration_ks_name text, initiating_host uuid, new_keyspace_rf_change_data frozen<map<text, text>>, new_keyspace_rf_change_ks_name text, request_type text, restore_snapshot_name text, restore_table_id uuid, snapshot_expiry timestamp, snapshot_skip_flush boolean, snapshot_table_ids frozen<set<uuid>>, snapshot_tag text, start_time timestamp, truncate_table_id uuid
- `system.truncated`: table_uuid uuid (partition_key), shard int (clustering), truncated_at timestamp (static), position int, segment_id bigint
- `system.view_building_tasks`: key text (partition_key), id timeuuid (clustering), min_task_id timeuuid (static), aborted boolean, base_id uuid, host_id uuid, last_token bigint, shard int, type text, view_id uuid
- `system.views_builds_in_progress`: keyspace_name text (partition_key), view_name text (clustering), generation_number int, last_token text
- `system_distributed.snapshot_keyspaces`: snapshot_name text (partition_key), keyspace_name text (clustering), keyspace_schema text
- `system_distributed.snapshot_nodes`: snapshot_name text (partition_key), datacenter text (clustering), rack text (clustering), node uuid (clustering)
- `system_distributed.snapshot_remote_locations`: snapshot_name text (partition_key), datacenter text (clustering), bucket text, endpoint text, prefix text, state int
- `system_distributed.snapshot_sstables`: snapshot_name text (partition_key), keyspace text (partition_key), table text (partition_key), datacenter text (partition_key), rack text (partition_key), first_token bigint (clustering), sstable_id uuid (clustering), data_size bigint, downloaded boolean, index_size bigint, last_token bigint, node uuid, prefix text, repaired_at bigint, state int, tablet bigint, toc_name text
- `system_distributed.snapshot_tables`: snapshot_name text (partition_key), keyspace_name text (clustering), table_name text (clustering), base_table_id uuid, table_id uuid, table_schema text, tablet_layout text, type int
- `system_distributed.snapshot_tablets`: snapshot_name text (partition_key), keyspace_name text (partition_key), table_name text (partition_key), datacenter text (partition_key), first_token bigint (clustering), last_token bigint, repair_time timestamp, repaired_at bigint, tablet_id bigint
- `system_distributed.snapshots`: name text (partition_key), created_at timestamp, expires_at timestamp, manifest_version text, namespace_version text
- `system_replicated_keys.encrypted_keys`: key_file text (partition_key), cipher text (partition_key), strength int (clustering), key_id timeuuid (clustering), key text
- `system_traces.node_slow_log_time_idx`: minute timestamp (partition_key), started_at timestamp (clustering), session_id uuid (clustering), node_ip inet, shard int, start_time timeuuid

## Cassandra 5.0.9

- `system`: `IndexInfo`, `available_ranges`, `available_ranges_v2`, `batches`, `built_views`, `compaction_history`, `local`, `paxos`, `paxos_repair_history`, `peer_events`, `peer_events_v2`, `peers`, `peers_v2`, `prepared_statements`, `repairs`, `size_estimates`, `sstable_activity`, `sstable_activity_v2`, `table_estimates`, `top_partitions`, `transferred_ranges`, `transferred_ranges_v2`, `view_builds_in_progress`
- `system_auth`: `cidr_groups`, `cidr_permissions`, `identity_to_role`, `network_permissions`, `resource_role_permissons_index`, `role_members`, `role_permissions`, `roles`
- `system_distributed`: `parent_repair_history`, `partition_denylist`, `repair_history`, `view_build_status`
- `system_schema`: `aggregates`, `column_masks`, `columns`, `dropped_columns`, `functions`, `indexes`, `keyspaces`, `tables`, `triggers`, `types`, `views`
- `system_traces`: `events`, `sessions`
- `system_views` (virtual): `batch_metrics`, `caches`, `cidr_filtering_metrics_counts`, `cidr_filtering_metrics_latencies`, `clients`, `coordinator_read_latency`, `coordinator_scan_latency`, `coordinator_write_latency`, `cql_metrics`, `credentials_cache_keys`, `disk_usage`, `gossip_info`, `internode_inbound`, `internode_outbound`, `jmx_permissions_cache_keys`, `local_read_latency`, `local_scan_latency`, `local_write_latency`, `max_partition_size`, `max_sstable_duration`, `max_sstable_size`, `network_permissions_cache_keys`, `pending_hints`, `permissions_cache_keys`, `queries`, `repair_jobs`, `repair_participates`, `repair_sessions`, `repair_validations`, `repairs`, `roles_cache_keys`, `rows_per_read`, `sai_column_indexes`, `sai_sstable_index_segments`, `sai_sstable_indexes`, `settings`, `snapshots`, `sstable_tasks`, `streaming`, `system_logs`, `system_properties`, `thread_pools`, `tombstones_per_read`
- `system_virtual_schema` (virtual): `columns`, `keyspaces`, `tables`

### system.size_estimates

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | none | text |
| table_name | clustering | 0 | asc | text |
| range_start | clustering | 1 | asc | text |
| range_end | clustering | 2 | asc | text |
| mean_partition_size | regular | -1 | none | bigint |
| partitions_count | regular | -1 | none | bigint |

0 rows on a fresh node.

### system.table_estimates

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | none | text |
| table_name | clustering | 0 | asc | text |
| range_type | clustering | 1 | asc | text |
| range_start | clustering | 2 | asc | text |
| range_end | clustering | 3 | asc | text |
| mean_partition_size | regular | -1 | none | bigint |
| partitions_count | regular | -1 | none | bigint |

0 rows on a fresh node.

### system.built_views

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | none | text |
| view_name | clustering | 0 | asc | text |
| status_replicated | regular | -1 | none | boolean |

0 rows on a fresh node.

### system.prepared_statements

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| prepared_id | partition_key | 0 | none | blob |
| logged_keyspace | regular | -1 | none | text |
| query_string | regular | -1 | none | text |

1 rows on a fresh node (first 5, masked):

```
prepared_id | logged_keyspace | query_string
b'\x1a\xfb\xb4\xdd0\\.F\xe3\xe3d\x0f\n\x89\xc8\xd7' | None | SELECT key FROM system.local WHERE key = ?
```
### system.top_partitions

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | none | text |
| table_name | clustering | 0 | asc | text |
| top_type | clustering | 1 | asc | text |
| last_update | regular | -1 | none | timestamp |
| top | regular | -1 | none | frozen<list<frozen<tuple<text, bigint>>>> |

0 rows on a fresh node.

### system.paxos

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| row_key | partition_key | 0 | none | blob |
| cf_id | clustering | 0 | asc | uuid |
| in_progress_ballot | regular | -1 | none | timeuuid |
| in_progress_read_ballot | regular | -1 | none | timeuuid |
| most_recent_commit | regular | -1 | none | blob |
| most_recent_commit_at | regular | -1 | none | timeuuid |
| most_recent_commit_version | regular | -1 | none | int |
| proposal | regular | -1 | none | blob |
| proposal_ballot | regular | -1 | none | timeuuid |
| proposal_version | regular | -1 | none | int |

0 rows on a fresh node.

### system.compaction_history

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| id | partition_key | 0 | none | timeuuid |
| bytes_in | regular | -1 | none | bigint |
| bytes_out | regular | -1 | none | bigint |
| columnfamily_name | regular | -1 | none | text |
| compacted_at | regular | -1 | none | timestamp |
| compaction_properties | regular | -1 | none | frozen<map<text, text>> |
| keyspace_name | regular | -1 | none | text |
| rows_merged | regular | -1 | none | map<int, bigint> |

4 rows on a fresh node (first 5, masked):

```
id | bytes_in | bytes_out | columnfamily_name | compacted_at | compaction_properties | keyspace_name | rows_merged
<uuid> | 8286 | 8024 | columns | 2026-10-20 18:36:09.261000 | {'compaction_type': 'Compaction'} | system_schema | {1: 6}
<uuid> | 857 | 677 | local | 2026-10-20 18:34:02.234000 | {'compaction_type': 'Compaction'} | system | {7: 1}
<uuid> | 4411 | 3887 | tables | 2026-10-20 18:36:09.338000 | {'compaction_type': 'Compaction'} | system_schema | {1: 6}
<uuid> | 592 | 337 | keyspaces | 2026-10-20 18:36:09.014000 | {'compaction_type': 'Compaction'} | system_schema | {1: 6}
```
### system_auth.roles

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| role | partition_key | 0 | none | text |
| can_login | regular | -1 | none | boolean |
| is_superuser | regular | -1 | none | boolean |
| member_of | regular | -1 | none | set<text> |
| salted_hash | regular | -1 | none | text |

1 rows on a fresh node (first 5, masked):

```
role | can_login | is_superuser | member_of | salted_hash
cassandra | True | True | None | <salted hash>
```
### system_auth.role_members

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| role | partition_key | 0 | none | text |
| member | clustering | 0 | asc | text |

0 rows on a fresh node.

### system_auth.role_permissions

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| role | partition_key | 0 | none | text |
| resource | clustering | 0 | asc | text |
| permissions | regular | -1 | none | set<text> |

0 rows on a fresh node.

### system_auth.network_permissions

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| role | partition_key | 0 | none | text |
| dcs | regular | -1 | none | frozen<set<text>> |

0 rows on a fresh node.

### system_auth.cidr_groups

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| cidr_group | partition_key | 0 | none | text |
| cidrs | regular | -1 | none | frozen<set<frozen<tuple<inet, smallint>>>> |

0 rows on a fresh node.

### system_auth.cidr_permissions

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| role | partition_key | 0 | none | text |
| cidr_groups | regular | -1 | none | frozen<set<text>> |

0 rows on a fresh node.

### system_auth.identity_to_role

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| identity | partition_key | 0 | none | text |
| role | regular | -1 | none | text |

0 rows on a fresh node.

### system_auth.resource_role_permissons_index

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| resource | partition_key | 0 | none | text |
| role | clustering | 0 | asc | text |

0 rows on a fresh node.

### system_traces.sessions

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| session_id | partition_key | 0 | none | uuid |
| client | regular | -1 | none | inet |
| command | regular | -1 | none | text |
| coordinator | regular | -1 | none | inet |
| coordinator_port | regular | -1 | none | int |
| duration | regular | -1 | none | int |
| parameters | regular | -1 | none | map<text, text> |
| request | regular | -1 | none | text |
| started_at | regular | -1 | none | timestamp |

1 rows on a fresh node (first 5, masked):

```
session_id | client | command | coordinator | coordinator_port | duration | parameters | request | started_at
<uuid> | <ip> | QUERY | <ip> | 7000 | 4542 | {'consistency_level': 'ONE', 'query': 'SELECT key FROM system.local', 'serial_consistency_level': 'SERIAL'} | Execute CQL3 query | 2026-10-20 18:37:42.137000
```
### system_traces.events

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| session_id | partition_key | 0 | none | uuid |
| event_id | clustering | 0 | asc | timeuuid |
| activity | regular | -1 | none | text |
| source | regular | -1 | none | inet |
| source_elapsed | regular | -1 | none | int |
| source_port | regular | -1 | none | int |
| thread | regular | -1 | none | text |

7 rows on a fresh node (first 5, masked):

```
session_id | event_id | activity | source | source_elapsed | source_port | thread
<uuid> | <uuid> | Parsing SELECT key FROM system.local | <ip> | 1975 | 7000 | Native-Transport-Requests-2
<uuid> | <uuid> | Preparing statement | <ip> | 2297 | 7000 | Native-Transport-Requests-2
<uuid> | <uuid> | Computing ranges to query | <ip> | 2632 | 7000 | Native-Transport-Requests-2
<uuid> | <uuid> | Submitting range requests on 1 ranges with a concurrency of 1 (31.831577 rows per range expected) | <ip> | 2826 | 7000 | Native-Transport-Requests-2
<uuid> | <uuid> | Submitted 1 concurrent range requests | <ip> | 3000 | 7000 | Native-Transport-Requests-2
```
### system_distributed.view_build_status

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | none | text |
| view_name | partition_key | 1 | none | text |
| host_id | clustering | 0 | asc | uuid |
| status | regular | -1 | none | text |

0 rows on a fresh node.

### system_distributed.repair_history

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| keyspace_name | partition_key | 0 | none | text |
| columnfamily_name | partition_key | 1 | none | text |
| id | clustering | 0 | asc | timeuuid |
| coordinator | regular | -1 | none | inet |
| coordinator_port | regular | -1 | none | int |
| exception_message | regular | -1 | none | text |
| exception_stacktrace | regular | -1 | none | text |
| finished_at | regular | -1 | none | timestamp |
| parent_id | regular | -1 | none | timeuuid |
| participants | regular | -1 | none | set<inet> |
| participants_v2 | regular | -1 | none | set<text> |
| range_begin | regular | -1 | none | text |
| range_end | regular | -1 | none | text |
| started_at | regular | -1 | none | timestamp |
| status | regular | -1 | none | text |

0 rows on a fresh node.

### system_distributed.parent_repair_history

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| parent_id | partition_key | 0 | none | timeuuid |
| columnfamily_names | regular | -1 | none | set<text> |
| exception_message | regular | -1 | none | text |
| exception_stacktrace | regular | -1 | none | text |
| finished_at | regular | -1 | none | timestamp |
| keyspace_name | regular | -1 | none | text |
| options | regular | -1 | none | map<text, text> |
| requested_ranges | regular | -1 | none | set<text> |
| started_at | regular | -1 | none | timestamp |
| successful_ranges | regular | -1 | none | set<text> |

0 rows on a fresh node.

### system_distributed.partition_denylist

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| ks_name | partition_key | 0 | none | text |
| table_name | partition_key | 1 | none | text |
| key | clustering | 0 | asc | blob |

0 rows on a fresh node.

### All virtual tables (columns)

- `system_views.batch_metrics` — Metrics specific to batch statements: name text (partition_key), max bigint, p50th double, p999th double, p99th double
- `system_views.caches` — system caches: name text (partition_key), capacity_bytes bigint, entry_count int, hit_count bigint, hit_ratio double, recent_hit_rate_per_second bigint, recent_request_rate_per_second bigint, request_count bigint, size_bytes bigint
- `system_views.cidr_filtering_metrics_counts` — Count metrics specific to CIDR filtering: name text (partition_key), value bigint
- `system_views.cidr_filtering_metrics_latencies` — Latency metrics specific to CIDR filtering: name text (partition_key), max double, p50th double, p95th double, p999th double, p99th double
- `system_views.clients` — currently connected clients: address inet (partition_key), port int (clustering), client_options frozen<map<text, text>>, connection_stage text, driver_name text, driver_version text, hostname text, keyspace_name text, protocol_version int, request_count bigint, ssl_cipher_suite text, ssl_enabled boolean, ssl_protocol text, username text
- `system_views.coordinator_read_latency` — : keyspace_name text (partition_key), table_name text (partition_key), count bigint, max_ms double, p50th_ms double, p99th_ms double, per_second double
- `system_views.coordinator_scan_latency` — : keyspace_name text (partition_key), table_name text (partition_key), count bigint, max_ms double, p50th_ms double, p99th_ms double, per_second double
- `system_views.coordinator_write_latency` — : keyspace_name text (partition_key), table_name text (partition_key), count bigint, max_ms double, p50th_ms double, p99th_ms double, per_second double
- `system_views.cql_metrics` — Metrics specific to CQL prepared statement caching: name text (partition_key), value double
- `system_views.credentials_cache_keys` — keys in the credentials cache: role text (partition_key)
- `system_views.disk_usage` — : keyspace_name text (partition_key), table_name text (partition_key), mebibytes bigint
- `system_views.gossip_info` — lists the gossip information for the cluster: address inet (partition_key), port int (clustering), dc text, dc_version int, disk_usage text, disk_usage_version int, generation int, heartbeat int, host_id text, host_id_version int, hostname text, index_status text, index_status_version int, internal_address_and_port text, internal_address_and_port_version int, internal_ip text, internal_ip_version int, load text, load_version int, native_address_and_port text, native_address_and_port_version int, net_version text, net_version_version int, rack text, rack_version int, release_version text, release_version_version int, removal_coordinator text, removal_coordinator_version int, rpc_address text, rpc_address_version int, rpc_ready text, rpc_ready_version int, schema text, schema_version int, severity text, severity_version int, sstable_versions text, sstable_versions_version int, status text, status_version int, status_with_port text, status_with_port_version int, tokens_version int, x1 text, x10 text, x10_version int, x1_version int, x2 text, x2_version int, x3 text, x3_version int, x4 text, x4_version int, x5 text, x5_version int, x6 text, x6_version int, x7 text, x7_version int, x8 text, x8_version int, x9 text, x9_version int, x_11_padding text, x_11_padding_version int
- `system_views.internode_inbound` — : address inet (partition_key), port int (partition_key), dc text (clustering), rack text (clustering), corrupt_frames_recovered bigint, corrupt_frames_unrecovered bigint, error_bytes bigint, error_count bigint, expired_bytes bigint, expired_count bigint, processed_bytes bigint, processed_count bigint, received_bytes bigint, received_count bigint, scheduled_bytes bigint, scheduled_count bigint, throttled_count bigint, throttled_nanos bigint, using_bytes bigint, using_reserve_bytes bigint
- `system_views.internode_outbound` — : address inet (partition_key), port int (partition_key), dc text (clustering), rack text (clustering), active_connections bigint, connection_attempts bigint, error_bytes bigint, error_count bigint, expired_bytes bigint, expired_count bigint, overload_bytes bigint, overload_count bigint, pending_bytes bigint, pending_count bigint, sent_bytes bigint, sent_count bigint, successful_connection_attempts bigint, using_bytes bigint, using_reserve_bytes bigint
- `system_views.jmx_permissions_cache_keys` — keys in the JMX permissions cache: role text (partition_key)
- `system_views.local_read_latency` — : keyspace_name text (partition_key), table_name text (partition_key), count bigint, max_ms double, p50th_ms double, p99th_ms double, per_second double
- `system_views.local_scan_latency` — : keyspace_name text (partition_key), table_name text (partition_key), count bigint, max_ms double, p50th_ms double, p99th_ms double, per_second double
- `system_views.local_write_latency` — : keyspace_name text (partition_key), table_name text (partition_key), count bigint, max_ms double, p50th_ms double, p99th_ms double, per_second double
- `system_views.max_partition_size` — : keyspace_name text (partition_key), table_name text (partition_key), mebibytes bigint
- `system_views.max_sstable_duration` — : keyspace_name text (partition_key), table_name text (partition_key), max_sstable_duration bigint
- `system_views.max_sstable_size` — : keyspace_name text (partition_key), table_name text (partition_key), mebibytes bigint
- `system_views.network_permissions_cache_keys` — keys in the network permissions cache: role text (partition_key)
- `system_views.pending_hints` — Pending hints that this node has for other nodes: host_id uuid (partition_key), address inet, dc text, files int, newest timestamp, oldest timestamp, port int, rack text, status text
- `system_views.permissions_cache_keys` — keys in the permissions cache: role text (partition_key), resource text (partition_key)
- `system_views.queries` — Lists currently running queries: thread_id text (partition_key), queued_micros bigint, running_micros bigint, task text
- `system_views.repair_jobs` — Repair job: id uuid (partition_key), completed boolean, duration_millis bigint, failure_cause text, keyspace_name text, last_updated_at timestamp, participants frozen<list<text>>, ranges frozen<list<text>>, repair_id timeuuid, session_id timeuuid, state_failure_timestamp timestamp, state_init_timestamp timestamp, state_skipped_timestamp timestamp, state_snapshot_complete_timestamp timestamp, state_snapshot_start_timestamp timestamp, state_start_timestamp timestamp, state_stream_start_timestamp timestamp, state_success_timestamp timestamp, state_validation_complete_timestamp timestamp, state_validation_start_timestamp timestamp, status text, success_message text, table_name text
- `system_views.repair_participates` — Repair participate summary: id timeuuid (partition_key), completed boolean, duration_millis bigint, failure_cause text, global boolean, incremental boolean, initiator text, last_updated_at timestamp, preview_kind text, ranges frozen<list<text>>, repaired_at timestamp, state_failure_timestamp timestamp, state_init_timestamp timestamp, state_skipped_timestamp timestamp, state_success_timestamp timestamp, success_message text, tables frozen<set<text>>, validations frozen<set<uuid>>
- `system_views.repair_sessions` — Repair session: id timeuuid (partition_key), completed boolean, duration_millis bigint, failure_cause text, jobs frozen<set<uuid>>, keyspace_name text, last_updated_at timestamp, participants frozen<list<text>>, ranges frozen<list<text>>, repair_id timeuuid, state_failure_timestamp timestamp, state_init_timestamp timestamp, state_jobs_start_timestamp timestamp, state_skipped_timestamp timestamp, state_start_timestamp timestamp, state_success_timestamp timestamp, status text, success_message text, table_names frozen<list<text>>
- `system_views.repair_validations` — Repair validation: id uuid (partition_key), bytes_read bigint, completed boolean, duration_millis bigint, estimated_partitions bigint, estimated_total_bytes bigint, failure_cause text, initiator text, keyspace_name text, last_updated_at timestamp, partitions_processed bigint, progress_percentage float, ranges frozen<list<text>>, repair_id timeuuid, session_id timeuuid, state_accept_timestamp timestamp, state_failure_timestamp timestamp, state_init_timestamp timestamp, state_sending_trees_timestamp timestamp, state_skipped_timestamp timestamp, state_start_timestamp timestamp, state_success_timestamp timestamp, status text, success_message text, table_name text
- `system_views.repairs` — Repair summary: id timeuuid (partition_key), command_id int, completed boolean, duration_millis bigint, failure_cause text, keyspace_name text, last_updated_at timestamp, options_column_families frozen<set<text>>, options_data_centers frozen<set<text>>, options_force_repair boolean, options_hosts frozen<set<text>>, options_ignore_unreplicated_keyspaces boolean, options_incremental boolean, options_job_threads int, options_optimise_streams boolean, options_parallelism text, options_preview_kind text, options_primary_range boolean, options_pull_repair boolean, options_ranges frozen<set<text>>, options_subrange_repair boolean, options_trace boolean, participants frozen<list<text>>, ranges frozen<list<frozen<list<text>>>>, sessions frozen<set<timeuuid>>, state_failure_timestamp timestamp, state_init_timestamp timestamp, state_prepare_complete_timestamp timestamp, state_prepare_start_timestamp timestamp, state_repair_complete_timestamp timestamp, state_repair_start_timestamp timestamp, state_setup_timestamp timestamp, state_skipped_timestamp timestamp, state_start_timestamp timestamp, state_success_timestamp timestamp, status text, success_message text, table_names frozen<list<text>>, type text, unfiltered_ranges frozen<list<frozen<list<text>>>>
- `system_views.roles_cache_keys` — keys in the roles cache: role text (partition_key)
- `system_views.rows_per_read` — : keyspace_name text (partition_key), table_name text (partition_key), count bigint, max double, p50th double, p99th double
- `system_views.sai_column_indexes` — Storage-attached column index metadata: keyspace_name text (partition_key), index_name text (clustering), analyzer text, column_name text, is_building boolean, is_queryable boolean, is_string boolean, table_name text
- `system_views.sai_sstable_index_segments` — SSTable index segment metadata: keyspace_name text (partition_key), index_name text (clustering), sstable_name text (clustering), segment_row_id_offset bigint (clustering), cell_count bigint, column_name text, component_metadata frozen<map<text, frozen<map<text, text>>>>, end_token text, max_sstable_row_id bigint, max_term text, min_sstable_row_id bigint, min_term text, start_token text, table_name text
- `system_views.sai_sstable_indexes` — SSTable index metadata: keyspace_name text (partition_key), index_name text (clustering), sstable_name text (clustering), cell_count bigint, column_name text, end_token text, format_version text, max_row_id bigint, min_row_id bigint, per_column_disk_size bigint, per_table_disk_size bigint, start_token text, table_name text
- `system_views.settings` — current settings: name text (partition_key), value text
- `system_views.snapshots` — available snapshots: name text (partition_key), keyspace_name text (clustering), table_name text (clustering), created_at timestamp, ephemeral boolean, expires_at timestamp, size_on_disk bigint, true_size bigint
- `system_views.sstable_tasks` — current sstable tasks: keyspace_name text (partition_key), table_name text (clustering), task_id timeuuid (clustering), completion_ratio double, kind text, progress bigint, sstables int, target_directory text, total bigint, total_compressed bigint, unit text
- `system_views.streaming` — : id timeuuid (partition_key), bytes_received bigint, bytes_sent bigint, bytes_to_receive bigint, bytes_to_send bigint, duration_millis bigint, failure_cause text, files_received bigint, files_sent bigint, files_to_receive bigint, files_to_send bigint, follower boolean, last_updated_at timestamp, operation text, peers frozen<list<text>>, progress_percentage float, status text, status_failure_timestamp timestamp, status_init_timestamp timestamp, status_start_timestamp timestamp, status_success_timestamp timestamp, success_message text
- `system_views.system_logs` — Cassandra logs: timestamp timestamp (partition_key), order_in_millisecond int (clustering), level text, logger text, message text
- `system_views.system_properties` — Cassandra relevant system properties: name text (partition_key), value text
- `system_views.thread_pools` — : name text (partition_key), active_tasks int, active_tasks_limit int, blocked_tasks bigint, blocked_tasks_all_time bigint, completed_tasks bigint, pending_tasks int
- `system_views.tombstones_per_read` — : keyspace_name text (partition_key), table_name text (partition_key), count bigint, max double, p50th double, p99th double
- `system_virtual_schema.columns` — virtual column definitions: keyspace_name text (partition_key), table_name text (clustering), column_name text (clustering), clustering_order text, column_name_bytes blob, kind text, position int, type text
- `system_virtual_schema.keyspaces` — virtual keyspace definitions: keyspace_name text (partition_key)
- `system_virtual_schema.tables` — virtual table definitions: keyspace_name text (partition_key), table_name text (clustering), comment text

### All other system tables (columns)

- `system.IndexInfo`: table_name text (partition_key), index_name text (clustering), value blob
- `system.available_ranges`: keyspace_name text (partition_key), ranges set<blob>
- `system.available_ranges_v2`: keyspace_name text (partition_key), full_ranges set<blob>, transient_ranges set<blob>
- `system.batches`: id timeuuid (partition_key), mutations list<blob>, version int
- `system.local`: key text (partition_key), bootstrapped text, broadcast_address inet, broadcast_port int, cluster_name text, cql_version text, data_center text, gossip_generation int, host_id uuid, listen_address inet, listen_port int, native_protocol_version text, partitioner text, rack text, release_version text, rpc_address inet, rpc_port int, schema_version uuid, tokens set<text>, truncated_at map<uuid, blob>
- `system.paxos_repair_history`: keyspace_name text (partition_key), table_name text (clustering), points frozen<list<frozen<tuple<blob, timeuuid>>>>
- `system.peer_events`: peer inet (partition_key), hints_dropped map<timeuuid, int>
- `system.peer_events_v2`: peer inet (partition_key), peer_port int (clustering), hints_dropped map<timeuuid, int>
- `system.peers`: peer inet (partition_key), data_center text, host_id uuid, preferred_ip inet, rack text, release_version text, rpc_address inet, schema_version uuid, tokens set<text>
- `system.peers_v2`: peer inet (partition_key), peer_port int (clustering), data_center text, host_id uuid, native_address inet, native_port int, preferred_ip inet, preferred_port int, rack text, release_version text, schema_version uuid, tokens set<text>
- `system.repairs`: parent_id timeuuid (partition_key), cfids set<uuid>, coordinator inet, coordinator_port int, last_update timestamp, participants set<inet>, participants_wp set<text>, ranges set<blob>, repaired_at timestamp, started_at timestamp, state int
- `system.sstable_activity`: keyspace_name text (partition_key), columnfamily_name text (partition_key), generation int (partition_key), rate_120m double, rate_15m double
- `system.sstable_activity_v2`: keyspace_name text (partition_key), table_name text (partition_key), id text (partition_key), rate_120m double, rate_15m double
- `system.transferred_ranges`: operation text (partition_key), keyspace_name text (partition_key), peer inet (clustering), ranges set<blob>
- `system.transferred_ranges_v2`: operation text (partition_key), keyspace_name text (partition_key), peer inet (clustering), peer_port int (clustering), ranges set<blob>
- `system.view_builds_in_progress`: keyspace_name text (partition_key), view_name text (clustering), start_token text (clustering), end_token text (clustering), keys_built bigint, last_token text
