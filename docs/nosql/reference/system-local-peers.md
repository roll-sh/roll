<!-- Generated from our own observations of the reference images (see conformance.md for the capture method). Do not edit by hand. -->

# system.local, system.peers, system.peers_v2

Columns come from `system_schema.columns` (`system_virtual_schema.columns` for virtual tables). Values are from a fresh single node started with the reference flags; values marked `<volatile>` change per node or per boot (addresses are the container IP).

## ScyllaDB 2026.3.3

### system.local

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| key | partition_key | 0 | NONE | text |
| bootstrapped | regular | -1 | NONE | text |
| broadcast_address | regular | -1 | NONE | inet |
| cluster_name | regular | -1 | NONE | text |
| cql_version | regular | -1 | NONE | text |
| data_center | regular | -1 | NONE | text |
| gossip_generation | regular | -1 | NONE | int |
| host_id | regular | -1 | NONE | uuid |
| listen_address | regular | -1 | NONE | inet |
| native_protocol_version | regular | -1 | NONE | text |
| partitioner | regular | -1 | NONE | text |
| rack | regular | -1 | NONE | text |
| release_version | regular | -1 | NONE | text |
| rpc_address | regular | -1 | NONE | inet |
| schema_version | regular | -1 | NONE | uuid |
| supported_features | regular | -1 | NONE | text |
| tokens | regular | -1 | NONE | set<text> |
| truncated_at | regular | -1 | NONE | map<uuid, blob> |

`SELECT *` column order and driver types: `key varchar`, `bootstrapped varchar`, `broadcast_address inet`, `cluster_name varchar`, `cql_version varchar`, `data_center varchar`, `gossip_generation int`, `host_id uuid`, `listen_address inet`, `native_protocol_version varchar`, `partitioner varchar`, `rack varchar`, `release_version varchar`, `rpc_address inet`, `schema_version uuid`, `supported_features varchar`, `tokens set<varchar>`, `truncated_at map<uuid, blob>`

Values:

| column | value |
|---|---|
| key | local |
| bootstrapped | COMPLETED |
| broadcast_address | <volatile> |
| cluster_name |  |
| cql_version | 3.3.1 |
| data_center | datacenter1 |
| gossip_generation | <volatile> |
| host_id | <volatile> |
| listen_address | <volatile> |
| native_protocol_version | 4 |
| partitioner | org.apache.cassandra.dht.Murmur3Partitioner |
| rack | rack1 |
| release_version | 3.0.8 |
| rpc_address | <volatile> |
| schema_version | <volatile> |
| supported_features | ADDRESS_NODES_BY_HOST_IDS,AGGREGATE_STORAGE_OPTIONS,ALTERNATOR_STREAMS,ALTERNATOR_TTL,ARBITRARY_TABLET_BOUNDARIES,BATCHLOG_V2,CDC,CDC_BLOCK_TABLET_MERGES_FOR_ALTERNATOR_STREAMS,CDC_GENERATIONS_V2,CDC_WITH_TABLETS,CLIENT_ROUTES,COLLECTION_INDEXING,COLOCATED_TABLETS,COMPACTION_HISTORY_UPGRADE,COMPRESS… (2740 chars) |
| tokens | <256 tokens, volatile> |
| truncated_at | None |

### system.peers

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| peer | partition_key | 0 | NONE | inet |
| data_center | regular | -1 | NONE | text |
| host_id | regular | -1 | NONE | uuid |
| preferred_ip | regular | -1 | NONE | inet |
| rack | regular | -1 | NONE | text |
| release_version | regular | -1 | NONE | text |
| rpc_address | regular | -1 | NONE | inet |
| schema_version | regular | -1 | NONE | uuid |
| supported_features | regular | -1 | NONE | text |
| tokens | regular | -1 | NONE | set<text> |

`SELECT *` column order and driver types: `peer inet`, `data_center varchar`, `host_id uuid`, `preferred_ip inet`, `rack varchar`, `release_version varchar`, `rpc_address inet`, `schema_version uuid`, `supported_features varchar`, `tokens set<varchar>`

No rows.

### system.peers_v2

Not present: `SELECT * FROM system.peers_v2` → `None` not captured

### system.scylla_local

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| key | partition_key | 0 | NONE | text |
| value | regular | -1 | NONE | text |

Rows (addresses and uuids masked):

```
key | value
enabled_features | ADDRESS_NODES_BY_HOST_IDS,AGGREGATE_STORAGE_OPTIONS,ALTERNATOR_STREAMS,ALTERNATOR_TTL,ARBITRARY_TABLET_BOUNDARIES,BATCHLOG_V2,CDC,CDC_BLOCK_TABLET_MERGES_FOR_AL… (2740 chars)
auth_version | 2
raft_group0_id | <uuid>
service_level_version | 2
service_level_driver_created | true
group0_schema_version | <uuid>
group0_upgrade_state | use_post_raft_procedures
view_builder_version | 20
```
### system.versions

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| key | partition_key | 0 | NONE | text |
| build_id | regular | -1 | NONE | text |
| build_mode | regular | -1 | NONE | text |
| version | regular | -1 | NONE | text |

Rows (addresses and uuids masked):

```
key | build_id | build_mode | version
local | d0d29ed4522f3714b887a0841c08d59f92b273fc | release | 2026.3.3-0.20261004.d80b3d6ca26b
```
### system.protocol_servers

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| name | partition_key | 0 | NONE | text |
| listen_addresses | regular | -1 | NONE | frozen<list<text>> |
| protocol | regular | -1 | NONE | text |
| protocol_version | regular | -1 | NONE | text |

Rows (addresses and uuids masked):

```
name | listen_addresses | protocol | protocol_version
native transport | ['<ip>:9042', '<ip>:19042'] | cql | 3.3.1
alternator | ['<ip>:8000'] | dynamodb | 2012-08-10
```
### system.runtime_info

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| group | partition_key | 0 | NONE | text |
| item | clustering | 0 | ASC | text |
| value | regular | -1 | NONE | text |

Rows (addresses and uuids masked):

```
group | item | value
cache | entries | 65
cache | hit_rate_recent | 84
cache | hit_rate_total | 1
cache | hits | 13930
cache | memory_free | 1909334
cache | memory_total | 2752512
cache | memory_used | 843178
cache | misses | 0
cache | requests_recent | 83
cache | requests_total | 13930
memory | free | 63479808
memory | total | 1073741824
memory | used | 1010262016
generic | gossip_active | true
generic | incremental_backup_enabled | false
generic | load | 0
generic | trace_probability | 0
generic | uptime | 167 seconds
memtable | entries | 0
memtable | memory_free | 0
memtable | memory_total | 0
memtable | memory_used | 0
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

Rows (addresses and uuids masked):

```
peer | dc | draining | excluded | host_id | load | owns | rack | status | tokens | up
<ip> | datacenter1 | False | False | <uuid> | 346044 | 1.0 | rack1 | NORMAL | 256 | True
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

Rows (addresses and uuids masked):

```
key | host_id | committed_cdc_generations | enabled_features | fence_version | global_requests | global_topology_request | global_topology_request_id | ignore_nodes | new_cdc_generation_data_uuid | new_keyspace_rf_change_data | new_keyspace_rf_change_ks_name | ongoing_restore_requests | ongoing_rf_changes | paused_rf_change_requests | session | tablet_balancing_enabled | transition_state | unpublished_cdc_generations | upgrade_state | version | cleanup_status | datacenter | ignore_msb | intended_storage_mode | node_state | num_tokens | rack | rebuild_option | release_version | replaced_id | request_id | shard_count | supported_features | tokens | tokens_string | topology_request
topology | <uuid> | SortedSet([(datetime.datetime(2026, 10, 20, 18, 33, 6, 228000), UUID('<uuid>'))]) | SortedSet(['ADDRESS_NODES_BY_HOST_IDS', 'AGGREGATE_STORAGE_OPTIONS', 'ALTERNATOR_STREAMS', 'ALTERNATOR_TTL', 'ARBITRARY_TABLET_BOUNDARIES', 'BATCHLOG_V2', 'CDC'… (3106 chars) | 3 | None | None | None | None | <uuid> | None | None | None | None | None | None | None | None | None | done | 4 | clean | datacenter1 | 12 | None | normal | 256 | rack1 | None | 3.0.8 | None | <uuid> | 1 | SortedSet(['ADDRESS_NODES_BY_HOST_IDS', 'AGGREGATE_STORAGE_OPTIONS', 'ALTERNATOR_STREAMS', 'ALTERNATOR_TTL', 'ARBITRARY_TABLET_BOUNDARIES', 'BATCHLOG_V2', 'CDC'… (3106 chars) | SortedSet(['-1066131549945567833', '-1102178384441593671', '-1116721547422761544', '-1170385816353652701', '-1180746700727346454', '-1194630233174319523', '-120… (6001 chars) |  | None
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

Rows (addresses and uuids masked):

```
node | dc | draining | effective_capacity | excluded | ip | rack | storage_allocated_load | storage_allocated_utilization | storage_capacity | storage_load | storage_utilization | tablets_allocated | tablets_allocated_per_shard | up
<uuid> | datacenter1 | False | 614107455488 | False | <ip> | rack1 | 85899345920 | 0.1398767351745309 | 784364208128 | 0 | 0.0 | 16 | 16.0 | True
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

Rows (addresses and uuids masked):

```
address | port | client_type | client_options | connection_stage | driver_name | driver_version | hostname | protocol_version | scheduling_group | shard_id | ssl_cipher_suite | ssl_enabled | ssl_protocol | username
<ip> | 57634 | cql | {'SCYLLA_LWT_ADD_METADATA_MARK': '2147483648', 'TABLETS_ROUTING_V1': '', 'DRIVER_VERSION': '3.29.7', 'CQL_VERSION': '3.3.1', 'SCYLLA_RATE_LIMIT_ERROR': '', 'DRI… (196 chars) | READY | ScyllaDB Python Driver | 3.29.7 | None | 4 | sl:driver | 0 | None | False | None | anonymous
<ip> | 57636 | cql | {'SCYLLA_LWT_ADD_METADATA_MARK': '2147483648', 'TABLETS_ROUTING_V1': '', 'DRIVER_VERSION': '3.29.7', 'CQL_VERSION': '3.3.1', 'SCYLLA_RATE_LIMIT_ERROR': '', 'DRI… (196 chars) | READY | ScyllaDB Python Driver | 3.29.7 | None | 4 | sl:default | 0 | None | False | None | anonymous
```
## Cassandra 5.0.9

### system.local

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| key | partition_key | 0 | none | text |
| bootstrapped | regular | -1 | none | text |
| broadcast_address | regular | -1 | none | inet |
| broadcast_port | regular | -1 | none | int |
| cluster_name | regular | -1 | none | text |
| cql_version | regular | -1 | none | text |
| data_center | regular | -1 | none | text |
| gossip_generation | regular | -1 | none | int |
| host_id | regular | -1 | none | uuid |
| listen_address | regular | -1 | none | inet |
| listen_port | regular | -1 | none | int |
| native_protocol_version | regular | -1 | none | text |
| partitioner | regular | -1 | none | text |
| rack | regular | -1 | none | text |
| release_version | regular | -1 | none | text |
| rpc_address | regular | -1 | none | inet |
| rpc_port | regular | -1 | none | int |
| schema_version | regular | -1 | none | uuid |
| tokens | regular | -1 | none | set<text> |
| truncated_at | regular | -1 | none | map<uuid, blob> |

`SELECT *` column order and driver types: `key varchar`, `bootstrapped varchar`, `broadcast_address inet`, `broadcast_port int`, `cluster_name varchar`, `cql_version varchar`, `data_center varchar`, `gossip_generation int`, `host_id uuid`, `listen_address inet`, `listen_port int`, `native_protocol_version varchar`, `partitioner varchar`, `rack varchar`, `release_version varchar`, `rpc_address inet`, `rpc_port int`, `schema_version uuid`, `tokens set<varchar>`, `truncated_at map<uuid, blob>`

Values:

| column | value |
|---|---|
| key | local |
| bootstrapped | COMPLETED |
| broadcast_address | <volatile> |
| broadcast_port | 7000 |
| cluster_name | Test Cluster |
| cql_version | 3.4.7 |
| data_center | datacenter1 |
| gossip_generation | <volatile> |
| host_id | <volatile> |
| listen_address | <volatile> |
| listen_port | 7000 |
| native_protocol_version | 5 |
| partitioner | org.apache.cassandra.dht.Murmur3Partitioner |
| rack | rack1 |
| release_version | 5.0.9 |
| rpc_address | <volatile> |
| rpc_port | 9042 |
| schema_version | <volatile> |
| tokens | <16 tokens, volatile> |
| truncated_at | <volatile> |

### system.peers

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| peer | partition_key | 0 | none | inet |
| data_center | regular | -1 | none | text |
| host_id | regular | -1 | none | uuid |
| preferred_ip | regular | -1 | none | inet |
| rack | regular | -1 | none | text |
| release_version | regular | -1 | none | text |
| rpc_address | regular | -1 | none | inet |
| schema_version | regular | -1 | none | uuid |
| tokens | regular | -1 | none | set<text> |

`SELECT *` column order and driver types: `peer inet`, `data_center varchar`, `host_id uuid`, `preferred_ip inet`, `rack varchar`, `release_version varchar`, `rpc_address inet`, `schema_version uuid`, `tokens set<varchar>`

No rows.

### system.peers_v2

| column | kind | position | clustering_order | type |
|---|---|---|---|---|
| peer | partition_key | 0 | none | inet |
| peer_port | clustering | 0 | asc | int |
| data_center | regular | -1 | none | text |
| host_id | regular | -1 | none | uuid |
| native_address | regular | -1 | none | inet |
| native_port | regular | -1 | none | int |
| preferred_ip | regular | -1 | none | inet |
| preferred_port | regular | -1 | none | int |
| rack | regular | -1 | none | text |
| release_version | regular | -1 | none | text |
| schema_version | regular | -1 | none | uuid |
| tokens | regular | -1 | none | set<text> |

`SELECT *` column order and driver types: `peer inet`, `peer_port int`, `data_center varchar`, `host_id uuid`, `native_address inet`, `native_port int`, `preferred_ip inet`, `preferred_port int`, `rack varchar`, `release_version varchar`, `schema_version uuid`, `tokens set<varchar>`

No rows.
