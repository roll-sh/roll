<!-- Generated from our own observations of the reference images (see conformance.md for the capture method). Do not edit by hand. -->

# Server-side DESCRIBE over system keyspaces

Same format as [describe.md](describe.md).

## `DESCRIBE FULL SCHEMA`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`system_traces | keyspace | system_traces`

```cql
CREATE KEYSPACE system_traces WITH replication = {'class': 'org.apache.cassandra.locator.SimpleStrategy', 'replication_factor': '2'} AND durable_writes = true AND tablets = {'enabled': false};
```
`system_traces | table | events`

```cql
CREATE TABLE system_traces.events (
    session_id uuid,
    event_id timeuuid,
    activity text,
    scylla_parent_id bigint,
    scylla_span_id bigint,
    source inet,
    source_elapsed int,
    thread text,
    PRIMARY KEY (session_id, event_id)
) WITH CLUSTERING ORDER BY (event_id ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 86400
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE'
    AND tombstone_gc = {'mode': 'repair', 'propagation_delay_in_seconds': '3600'};

```
`system_traces | table | node_slow_log`

```cql
CREATE TABLE system_traces.node_slow_log (
    start_time timeuuid,
    node_ip inet,
    shard int,
    command text,
    date timestamp,
    duration int,
    parameters map<text, text>,
    session_id uuid,
    source_ip inet,
    table_names set<text>,
    username text,
    PRIMARY KEY (start_time, node_ip, shard)
) WITH CLUSTERING ORDER BY (node_ip ASC, shard ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 86400
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE'
    AND tombstone_gc = {'mode': 'repair', 'propagation_delay_in_seconds': '3600'};

```
`system_traces | table | node_slow_log_time_idx`

```cql
CREATE TABLE system_traces.node_slow_log_time_idx (
    minute timestamp,
    started_at timestamp,
    session_id uuid,
    node_ip inet,
    shard int,
    start_time timeuuid,
    PRIMARY KEY (minute, started_at, session_id)
) WITH CLUSTERING ORDER BY (started_at ASC, session_id ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 86400
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE'
    AND tombstone_gc = {'mode': 'repair', 'propagation_delay_in_seconds': '3600'};

```
`system_traces | table | sessions`

```cql
CREATE TABLE system_traces.sessions (
    session_id uuid,
    client inet,
    command text,
    coordinator inet,
    duration int,
    parameters map<text, text>,
    request text,
    request_size int,
    response_size int,
    started_at timestamp,
    username text,
    PRIMARY KEY (session_id)
) WITH bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 86400
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE'
    AND tombstone_gc = {'mode': 'repair', 'propagation_delay_in_seconds': '3600'};

```
`system_traces | table | sessions_time_idx`

```cql
CREATE TABLE system_traces.sessions_time_idx (
    minute timestamp,
    started_at timestamp,
    session_id uuid,
    PRIMARY KEY (minute, started_at, session_id)
) WITH CLUSTERING ORDER BY (started_at ASC, session_id ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 86400
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE'
    AND tombstone_gc = {'mode': 'repair', 'propagation_delay_in_seconds': '3600'};

```
`system | keyspace | system`

```cql
CREATE KEYSPACE system WITH replication = {'class': 'org.apache.cassandra.locator.LocalStrategy'} AND durable_writes = true AND tablets = {'enabled': false};
```
`system | type | sstableinfo`

```cql
CREATE TYPE system.sstableinfo (
    generation uuid,
    origin text,
    size bigint
);
```
`system | type | tablet_task_info`

```cql
CREATE TYPE system.tablet_task_info (
    request_type text,
    tablet_task_id uuid,
    request_time timestamp,
    sched_nr bigint,
    sched_time timestamp,
    repair_hosts_filter text,
    repair_dcs_filter text
);
```
`system | type | repair_scheduler_config`

```cql
CREATE TYPE system.repair_scheduler_config (
    auto_repair_enabled boolean,
    auto_repair_threshold bigint
);
```
`system | table | "IndexInfo"`

```cql
CREATE TABLE system."IndexInfo" (
    table_name text,
    index_name text,
    value empty,
    PRIMARY KEY (table_name, index_name)
) WITH CLUSTERING ORDER BY (index_name ASC)
    AND COMPACT STORAGE
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'built column indexes'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 0
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | batchlog`

```cql
CREATE TABLE system.batchlog (
    id uuid,
    data blob,
    version int,
    written_at timestamp,
    PRIMARY KEY (id)
) WITH bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'batches awaiting replay'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 0
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | batchlog_v2`

```cql
CREATE TABLE system.batchlog_v2 (
    version int,
    stage tinyint,
    shard int,
    written_at timestamp,
    id uuid,
    data blob,
    PRIMARY KEY ((version, stage, shard), written_at, id)
) WITH CLUSTERING ORDER BY (written_at ASC, id ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'enabled': 'false', 'keys': 'NONE', 'rows_per_partition': 'NONE'}
    AND comment = 'batches awaiting replay'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 0
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | built_views`

```cql
CREATE TABLE system.built_views (
    keyspace_name text,
    view_name text,
    PRIMARY KEY (keyspace_name, view_name)
) WITH CLUSTERING ORDER BY (view_name ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'built views'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 0
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | cdc_generations_v3`

```cql
CREATE TABLE system.cdc_generations_v3 (
    key text,
    id timeuuid,
    range_end bigint,
    ignore_msb tinyint,
    streams frozen<set<blob>>,
    PRIMARY KEY (key, id, range_end)
) WITH CLUSTERING ORDER BY (id ASC, range_end ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | cdc_local`

```cql
CREATE TABLE system.cdc_local (
    key text,
    streams_timestamp timestamp,
    uuid uuid,
    PRIMARY KEY (key)
) WITH bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'CDC-specific information that the local node stores'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 0
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | cdc_streams`

```cql
CREATE TABLE system.cdc_streams (
    keyspace_name text,
    table_name text,
    timestamp timestamp,
    stream_state tinyint,
    stream_id blob,
    PRIMARY KEY ((keyspace_name, table_name), timestamp, stream_state, stream_id)
) WITH CLUSTERING ORDER BY (timestamp ASC, stream_state ASC, stream_id ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'CDC streams for each generation for tablets-based tables'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | cdc_streams_history`

```cql
CREATE TABLE system.cdc_streams_history (
    table_id uuid,
    timestamp timestamp,
    stream_state tinyint,
    last_token bigint,
    stream_id blob,
    PRIMARY KEY (table_id, timestamp, stream_state, last_token)
) WITH CLUSTERING ORDER BY (timestamp ASC, stream_state ASC, last_token ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'CDC stream sets for tablets-based tables described as differences from the previous state'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | cdc_streams_state`

```cql
CREATE TABLE system.cdc_streams_state (
    table_id uuid,
    last_token bigint,
    timestamp timestamp STATIC,
    stream_id blob,
    PRIMARY KEY (table_id, last_token)
) WITH CLUSTERING ORDER BY (last_token ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'Oldest CDC stream set for tablets-based tables'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | cdc_timestamps`

```cql
CREATE TABLE system.cdc_timestamps (
    keyspace_name text,
    table_name text,
    timestamp timestamp,
    PRIMARY KEY ((keyspace_name, table_name), timestamp)
) WITH CLUSTERING ORDER BY (timestamp DESC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'CDC generations for tablets-based tables'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | client_routes`

```cql
CREATE TABLE system.client_routes (
    connection_id text,
    host_id uuid,
    address text,
    alternator_https_port int,
    alternator_port int,
    port int,
    tls_port int,
    PRIMARY KEY (connection_id, host_id)
) WITH CLUSTERING ORDER BY (host_id ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | clients`

```cql
CREATE TABLE system.clients (
    address inet,
    port int,
    client_type text,
    client_options frozen<map<text, text>>,
    connection_stage text,
    driver_name text,
    driver_version text,
    hostname text,
    protocol_version int,
    scheduling_group text,
    shard_id int,
    ssl_cipher_suite text,
    ssl_enabled boolean,
    ssl_protocol text,
    username text,
    PRIMARY KEY (address, port, client_type)
) WITH CLUSTERING ORDER BY (port ASC, client_type ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | cluster_status`

```cql
CREATE TABLE system.cluster_status (
    peer inet,
    dc text,
    draining boolean,
    excluded boolean,
    host_id uuid,
    load text,
    owns float,
    rack text,
    status text,
    tokens int,
    up boolean,
    PRIMARY KEY (peer)
) WITH bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | commitlog_cleanups`

```cql
CREATE TABLE system.commitlog_cleanups (
    shard int,
    position frozen<tuple<bigint, int>>,
    table_uuid uuid,
    start_token_exclusive bigint,
    end_token_inclusive bigint,
    PRIMARY KEY (shard, position, table_uuid, start_token_exclusive, end_token_inclusive)
) WITH CLUSTERING ORDER BY (position ASC, table_uuid ASC, start_token_exclusive ASC, end_token_inclusive ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'information about cleanups, for filtering commitlog replay'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | compaction_history`

```cql
CREATE TABLE system.compaction_history (
    id uuid,
    bytes_in bigint,
    bytes_out bigint,
    columnfamily_name text,
    compacted_at timestamp,
    compaction_type text,
    keyspace_name text,
    rows_merged map<int, bigint>,
    shard_id int,
    sstables_in frozen<list<frozen<sstableinfo>>>,
    sstables_out frozen<list<frozen<sstableinfo>>>,
    started_at timestamp,
    total_tombstone_purge_attempt bigint,
    total_tombstone_purge_failure_due_to_overlapping_with_memtable bigint,
    total_tombstone_purge_failure_due_to_overlapping_with_uncompacting_sstable bigint,
    PRIMARY KEY (id)
) WITH bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'week-long compaction history'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 604800
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | compactions_in_progress`

```cql
CREATE TABLE system.compactions_in_progress (
    id uuid,
    columnfamily_name text,
    inputs set<int>,
    keyspace_name text,
    PRIMARY KEY (id)
) WITH bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'unfinished compactions'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | config`

```cql
CREATE TABLE system.config (
    name text,
    source text,
    type text,
    value text,
    PRIMARY KEY (name)
) WITH bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | corrupt_data`

```cql
CREATE TABLE system.corrupt_data (
    keyspace_name text,
    table_name text,
    id timeuuid,
    clustering_key blob,
    frozen_mutation_fragment blob,
    mutation_fragment_kind text,
    origin text,
    partition_key blob,
    sstable_name text,
    PRIMARY KEY ((keyspace_name, table_name), id)
) WITH CLUSTERING ORDER BY (id ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'mutation-fragments found to be corrupted'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 0
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | dicts`

```cql
CREATE TABLE system.dicts (
    name text,
    data blob,
    origin uuid,
    timestamp timestamp,
    PRIMARY KEY (name)
) WITH bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | discovery`

```cql
CREATE TABLE system.discovery (
    key text,
    ip_addr inet,
    raft_server_id uuid,
    PRIMARY KEY (key, ip_addr)
) WITH CLUSTERING ORDER BY (ip_addr ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'State of cluster discovery algorithm: the set of discovered peers'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | group0_history`

```cql
CREATE TABLE system.group0_history (
    key text,
    state_id timeuuid,
    description text,
    PRIMARY KEY (key, state_id)
) WITH CLUSTERING ORDER BY (state_id DESC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'History of Raft group 0 state changes'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | hints`

```cql
CREATE TABLE system.hints (
    target_id uuid,
    hint_id timeuuid,
    message_version int,
    mutation blob,
    PRIMARY KEY (target_id, hint_id, message_version)
) WITH CLUSTERING ORDER BY (hint_id ASC, message_version ASC)
    AND COMPACT STORAGE
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'hints awaiting delivery'
    AND compaction = {'class': 'IncrementalCompactionStrategy', 'enabled': 'false'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 0
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | large_cells`

```cql
CREATE TABLE system.large_cells (
    keyspace_name text,
    table_name text,
    sstable_name text,
    cell_size bigint,
    partition_key text,
    clustering_key text,
    column_name text,
    collection_elements bigint,
    compaction_time timestamp,
    PRIMARY KEY ((keyspace_name, table_name), sstable_name, cell_size, partition_key, clustering_key, column_name)
) WITH CLUSTERING ORDER BY (sstable_name ASC, cell_size DESC, partition_key ASC, clustering_key ASC, column_name ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'cells larger than specified threshold'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | large_partitions`

```cql
CREATE TABLE system.large_partitions (
    keyspace_name text,
    table_name text,
    sstable_name text,
    partition_size bigint,
    partition_key text,
    compaction_time timestamp,
    dead_rows bigint,
    range_tombstones bigint,
    rows bigint,
    PRIMARY KEY ((keyspace_name, table_name), sstable_name, partition_size, partition_key)
) WITH CLUSTERING ORDER BY (sstable_name ASC, partition_size DESC, partition_key ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'partitions larger than specified threshold'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | large_rows`

```cql
CREATE TABLE system.large_rows (
    keyspace_name text,
    table_name text,
    sstable_name text,
    row_size bigint,
    partition_key text,
    clustering_key text,
    compaction_time timestamp,
    PRIMARY KEY ((keyspace_name, table_name), sstable_name, row_size, partition_key, clustering_key)
) WITH CLUSTERING ORDER BY (sstable_name ASC, row_size DESC, partition_key ASC, clustering_key ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'rows larger than specified threshold'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | load_per_node`

```cql
CREATE TABLE system.load_per_node (
    node uuid,
    dc text,
    draining boolean,
    effective_capacity bigint,
    excluded boolean,
    ip inet,
    rack text,
    storage_allocated_load bigint,
    storage_allocated_utilization double,
    storage_capacity bigint,
    storage_load bigint,
    storage_utilization double,
    tablets_allocated bigint,
    tablets_allocated_per_shard double,
    up boolean,
    PRIMARY KEY (node)
) WITH bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | local`

```cql
CREATE TABLE system.local (
    key text,
    bootstrapped text,
    broadcast_address inet,
    cluster_name text,
    cql_version text,
    data_center text,
    gossip_generation int,
    host_id uuid,
    listen_address inet,
    native_protocol_version text,
    partitioner text,
    rack text,
    release_version text,
    rpc_address inet,
    schema_version uuid,
    supported_features text,
    tokens set<text>,
    truncated_at map<uuid, blob>,
    PRIMARY KEY (key)
) WITH bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'information about the local node'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 0
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | paxos`

```cql
CREATE TABLE system.paxos (
    row_key blob,
    cf_id uuid,
    most_recent_commit blob,
    most_recent_commit_at timeuuid,
    promise timeuuid,
    proposal blob,
    proposal_ballot timeuuid,
    PRIMARY KEY (row_key, cf_id)
) WITH CLUSTERING ORDER BY (cf_id ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'in-progress paxos proposals'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 0
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | peer_events`

```cql
CREATE TABLE system.peer_events (
    peer inet,
    hints_dropped map<uuid, int>,
    PRIMARY KEY (peer)
) WITH bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'events related to peers'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 0
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | peers`

```cql
CREATE TABLE system.peers (
    peer inet,
    data_center text,
    host_id uuid,
    preferred_ip inet,
    rack text,
    release_version text,
    rpc_address inet,
    schema_version uuid,
    supported_features text,
    tokens set<text>,
    PRIMARY KEY (peer)
) WITH bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'information about known peers in the cluster'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 0
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | protocol_servers`

```cql
CREATE TABLE system.protocol_servers (
    name text,
    listen_addresses frozen<list<text>>,
    protocol text,
    protocol_version text,
    PRIMARY KEY (name)
) WITH bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'Lists all client protocol servers and their status.'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | raft`

```cql
CREATE TABLE system.raft (
    group_id timeuuid,
    "index" bigint,
    commit_idx bigint STATIC,
    snapshot_id uuid STATIC,
    vote uuid STATIC,
    vote_term bigint STATIC,
    data blob,
    term bigint,
    PRIMARY KEY (group_id, "index")
) WITH CLUSTERING ORDER BY ("index" ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'enabled': 'false', 'keys': 'NONE', 'rows_per_partition': 'NONE'}
    AND comment = 'Persisted RAFT log, votes and snapshot info'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | raft_snapshot_config`

```cql
CREATE TABLE system.raft_snapshot_config (
    group_id timeuuid,
    disposition ascii,
    server_id uuid,
    can_vote boolean,
    PRIMARY KEY (group_id, disposition, server_id)
) WITH CLUSTERING ORDER BY (disposition ASC, server_id ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'RAFT configuration for the latest snapshot descriptor'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | raft_snapshots`

```cql
CREATE TABLE system.raft_snapshots (
    group_id timeuuid,
    idx bigint,
    snapshot_id uuid,
    term bigint,
    PRIMARY KEY (group_id)
) WITH bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'Persisted RAFT snapshots for strongly consistent tablets'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | raft_state`

```cql
CREATE TABLE system.raft_state (
    group_id timeuuid,
    disposition ascii,
    server_id uuid,
    can_vote boolean,
    PRIMARY KEY (group_id, disposition, server_id)
) WITH CLUSTERING ORDER BY (disposition ASC, server_id ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'Currently operating RAFT configuration'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | range_xfers`

```cql
CREATE TABLE system.range_xfers (
    token_bytes blob,
    requested_at timestamp,
    PRIMARY KEY (token_bytes)
) WITH bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'ranges requested for transfer'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 0
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | repair_history`

```cql
CREATE TABLE system.repair_history (
    table_uuid uuid,
    repair_time timestamp,
    repair_uuid uuid,
    range_start bigint,
    range_end bigint,
    keyspace_name text STATIC,
    table_name text STATIC,
    PRIMARY KEY (table_uuid, repair_time, repair_uuid, range_start, range_end)
) WITH CLUSTERING ORDER BY (repair_time ASC, repair_uuid ASC, range_start ASC, range_end ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'Record repair history'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | repair_tasks`

```cql
CREATE TABLE system.repair_tasks (
    task_uuid uuid,
    operation text,
    first_token bigint,
    last_token bigint,
    table_uuid uuid STATIC,
    timestamp timestamp,
    PRIMARY KEY (task_uuid, operation, first_token, last_token)
) WITH CLUSTERING ORDER BY (operation ASC, first_token ASC, last_token ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'Record tablet repair tasks'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | role_attributes`

```cql
CREATE TABLE system.role_attributes (
    role text,
    name text,
    value text,
    PRIMARY KEY (role, name)
) WITH CLUSTERING ORDER BY (name ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'role permissions in RBAC'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | role_members`

```cql
CREATE TABLE system.role_members (
    role text,
    member text,
    PRIMARY KEY (role, member)
) WITH CLUSTERING ORDER BY (member ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'joins users and their granted roles in RBAC'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | role_permissions`

```cql
CREATE TABLE system.role_permissions (
    role text,
    resource text,
    permissions set<text>,
    PRIMARY KEY (role, resource)
) WITH CLUSTERING ORDER BY (resource ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'role permissions for CassandraAuthorizer'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | roles`

```cql
CREATE TABLE system.roles (
    role text,
    can_login boolean,
    is_superuser boolean,
    member_of set<text>,
    salted_hash text,
    PRIMARY KEY (role)
) WITH bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'roles for authentication and RBAC'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | runtime_info`

```cql
CREATE TABLE system.runtime_info (
    group text,
    item text,
    value text,
    PRIMARY KEY (group, item)
) WITH CLUSTERING ORDER BY (item ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'Runtime system information.'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | scylla_local`

```cql
CREATE TABLE system.scylla_local (
    key text,
    value text,
    PRIMARY KEY (key)
) WITH bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'Scylla specific information about the local node'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | scylla_table_schema_history`

```cql
CREATE TABLE system.scylla_table_schema_history (
    cf_id uuid,
    schema_version uuid,
    column_name text,
    clustering_order text,
    column_name_bytes blob,
    kind text,
    position int,
    type text,
    PRIMARY KEY (cf_id, schema_version, column_name)
) WITH CLUSTERING ORDER BY (schema_version ASC, column_name ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'Scylla specific table to store a history of column mappings for each table schema version upon an CREATE TABLE/ALTER TABLE operations'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | scylla_views_builds_in_progress`

```cql
CREATE TABLE system.scylla_views_builds_in_progress (
    keyspace_name text,
    view_name text,
    cpu_id int,
    first_token text,
    generation_number int,
    next_token text,
    PRIMARY KEY (keyspace_name, view_name, cpu_id)
) WITH CLUSTERING ORDER BY (view_name ASC, cpu_id ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | service_levels_v2`

```cql
CREATE TABLE system.service_levels_v2 (
    service_level text,
    shares int,
    timeout duration,
    workload_type text,
    PRIMARY KEY (service_level)
) WITH bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | size_estimates`

```cql
CREATE TABLE system.size_estimates (
    keyspace_name text,
    table_name text,
    range_start text,
    range_end text,
    mean_partition_size bigint,
    partitions_count bigint,
    PRIMARY KEY (keyspace_name, table_name, range_start, range_end)
) WITH CLUSTERING ORDER BY (table_name ASC, range_start ASC, range_end ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'per-table primary range size estimates'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 0
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | snapshots`

```cql
CREATE TABLE system.snapshots (
    keyspace_name text,
    table_name text,
    snapshot_name text,
    live bigint,
    total bigint,
    PRIMARY KEY (keyspace_name, table_name, snapshot_name)
) WITH CLUSTERING ORDER BY (table_name ASC, snapshot_name ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'Lists all the snapshots along with their size, dropped tables are not part of the listing.'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | sstable_activity`

```cql
CREATE TABLE system.sstable_activity (
    keyspace_name text,
    columnfamily_name text,
    generation int,
    rate_120m double,
    rate_15m double,
    PRIMARY KEY ((keyspace_name, columnfamily_name, generation))
) WITH bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'historic sstable read rates'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | sstables`

```cql
CREATE TABLE system.sstables (
    table_id uuid,
    node_owner uuid,
    generation timeuuid,
    format text,
    sstable_id uuid,
    state text,
    status text,
    version text,
    PRIMARY KEY ((table_id, node_owner), generation)
) WITH CLUSTERING ORDER BY (generation ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'SSTables ownership table'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | tablet_sizes`

```cql
CREATE TABLE system.tablet_sizes (
    table_id uuid,
    last_token bigint,
    missing_replicas frozen<set<uuid>>,
    replicas frozen<map<uuid, bigint>>,
    PRIMARY KEY (table_id, last_token)
) WITH CLUSTERING ORDER BY (last_token ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | tablets`

```cql
CREATE TABLE system.tablets (
    table_id uuid,
    last_token bigint,
    base_table uuid STATIC,
    isolated_tablet_for_merge bigint STATIC,
    keyspace_name text STATIC,
    repair_scheduler_config frozen<repair_scheduler_config> STATIC,
    resize_seq_number bigint STATIC,
    resize_task_info frozen<tablet_task_info> STATIC,
    resize_type text STATIC,
    selected_tablets_for_merge frozen<list<bigint>> STATIC,
    table_name text STATIC,
    tablet_count int STATIC,
    target_pow2_tablet_count bigint STATIC,
    migration_task_info frozen<tablet_task_info>,
    new_replicas frozen<list<frozen<tuple<uuid, int>>>>,
    repair_incremental_mode text,
    repair_task_info frozen<tablet_task_info>,
    repair_time timestamp,
    replicas frozen<list<frozen<tuple<uuid, int>>>>,
    session uuid,
    snapshot_name text,
    sstables_repaired_at bigint,
    stage text,
    transition text,
    PRIMARY KEY (table_id, last_token)
) WITH CLUSTERING ORDER BY (last_token ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | token_ring`

```cql
CREATE TABLE system.token_ring (
    keyspace_name text,
    table_name text,
    start_token text,
    endpoint inet,
    dc text,
    end_token text,
    rack text,
    PRIMARY KEY (keyspace_name, table_name, start_token, endpoint)
) WITH CLUSTERING ORDER BY (table_name ASC, start_token ASC, endpoint ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | topology`

```cql
CREATE TABLE system.topology (
    key text,
    host_id uuid,
    committed_cdc_generations set<frozen<tuple<timestamp, timeuuid>>> STATIC,
    enabled_features set<text> STATIC,
    fence_version bigint STATIC,
    global_requests set<timeuuid> STATIC,
    global_topology_request text STATIC,
    global_topology_request_id timeuuid STATIC,
    ignore_nodes set<uuid> STATIC,
    new_cdc_generation_data_uuid timeuuid STATIC,
    new_keyspace_rf_change_data frozen<map<text, text>> STATIC,
    new_keyspace_rf_change_ks_name text STATIC,
    ongoing_restore_requests set<timeuuid> STATIC,
    ongoing_rf_changes set<timeuuid> STATIC,
    paused_rf_change_requests set<timeuuid> STATIC,
    session uuid STATIC,
    tablet_balancing_enabled boolean STATIC,
    transition_state text STATIC,
    unpublished_cdc_generations set<frozen<tuple<timestamp, timeuuid>>> STATIC,
    upgrade_state text STATIC,
    version bigint STATIC,
    cleanup_status text,
    datacenter text,
    ignore_msb int,
    intended_storage_mode text,
    node_state text,
    num_tokens int,
    rack text,
    rebuild_option text,
    release_version text,
    replaced_id uuid,
    request_id timeuuid,
    shard_count int,
    supported_features set<text>,
    tokens set<text>,
    tokens_string text,
    topology_request text,
    PRIMARY KEY (key, host_id)
) WITH CLUSTERING ORDER BY (host_id ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'Current state of topology change machine'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | topology_requests`

```cql
CREATE TABLE system.topology_requests (
    id timeuuid,
    done boolean,
    end_time timestamp,
    error text,
    finalize_migration_ks_name text,
    initiating_host uuid,
    new_keyspace_rf_change_data frozen<map<text, text>>,
    new_keyspace_rf_change_ks_name text,
    request_type text,
    restore_snapshot_name text,
    restore_table_id uuid,
    snapshot_expiry timestamp,
    snapshot_skip_flush boolean,
    snapshot_table_ids frozen<set<uuid>>,
    snapshot_tag text,
    start_time timestamp,
    truncate_table_id uuid,
    PRIMARY KEY (id)
) WITH bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'Topology request tracking'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | truncated`

```cql
CREATE TABLE system.truncated (
    table_uuid uuid,
    shard int,
    truncated_at timestamp STATIC,
    position int,
    segment_id bigint,
    PRIMARY KEY (table_uuid, shard)
) WITH CLUSTERING ORDER BY (shard ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'information about table truncation'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 0
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | versions`

```cql
CREATE TABLE system.versions (
    key text,
    build_id text,
    build_mode text,
    version text,
    PRIMARY KEY (key)
) WITH bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'Version information.'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | view_build_status_v2`

```cql
CREATE TABLE system.view_build_status_v2 (
    keyspace_name text,
    view_name text,
    host_id uuid,
    status text,
    PRIMARY KEY ((keyspace_name, view_name), host_id)
) WITH CLUSTERING ORDER BY (host_id ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | view_building_tasks`

```cql
CREATE TABLE system.view_building_tasks (
    key text,
    id timeuuid,
    min_task_id timeuuid STATIC,
    aborted boolean,
    base_id uuid,
    host_id uuid,
    last_token bigint,
    shard int,
    type text,
    view_id uuid,
    PRIMARY KEY (key, id)
) WITH CLUSTERING ORDER BY (id ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system | table | views_builds_in_progress`

```cql
CREATE TABLE system.views_builds_in_progress (
    keyspace_name text,
    view_name text,
    generation_number int,
    last_token text,
    PRIMARY KEY (keyspace_name, view_name)
) WITH CLUSTERING ORDER BY (view_name ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'views builds current progress'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 0
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system_schema | keyspace | system_schema`

```cql
CREATE KEYSPACE system_schema WITH replication = {'class': 'org.apache.cassandra.locator.LocalStrategy'} AND durable_writes = true AND tablets = {'enabled': false};
```
`system_schema | table | aggregates`

```cql
CREATE TABLE system_schema.aggregates (
    keyspace_name text,
    aggregate_name text,
    argument_types frozen<list<text>>,
    final_func text,
    initcond text,
    return_type text,
    state_func text,
    state_type text,
    PRIMARY KEY (keyspace_name, aggregate_name, argument_types)
) WITH CLUSTERING ORDER BY (aggregate_name ASC, argument_types ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'user defined aggregate definitions'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system_schema | table | columns`

```cql
CREATE TABLE system_schema.columns (
    keyspace_name text,
    table_name text,
    column_name text,
    clustering_order text,
    column_name_bytes blob,
    kind text,
    position int,
    type text,
    PRIMARY KEY (keyspace_name, table_name, column_name)
) WITH CLUSTERING ORDER BY (table_name ASC, column_name ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'column definitions'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system_schema | table | computed_columns`

```cql
CREATE TABLE system_schema.computed_columns (
    keyspace_name text,
    table_name text,
    column_name text,
    computation blob,
    PRIMARY KEY (keyspace_name, table_name, column_name)
) WITH CLUSTERING ORDER BY (table_name ASC, column_name ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'computed columns'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system_schema | table | dropped_columns`

```cql
CREATE TABLE system_schema.dropped_columns (
    keyspace_name text,
    table_name text,
    column_name text,
    dropped_time timestamp,
    type text,
    PRIMARY KEY (keyspace_name, table_name, column_name)
) WITH CLUSTERING ORDER BY (table_name ASC, column_name ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'dropped column registry'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system_schema | table | functions`

```cql
CREATE TABLE system_schema.functions (
    keyspace_name text,
    function_name text,
    argument_types frozen<list<text>>,
    argument_names frozen<list<text>>,
    body text,
    called_on_null_input boolean,
    language text,
    return_type text,
    PRIMARY KEY (keyspace_name, function_name, argument_types)
) WITH CLUSTERING ORDER BY (function_name ASC, argument_types ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'user defined function definitions'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system_schema | table | indexes`

```cql
CREATE TABLE system_schema.indexes (
    keyspace_name text,
    table_name text,
    index_name text,
    kind text,
    options frozen<map<text, text>>,
    PRIMARY KEY (keyspace_name, table_name, index_name)
) WITH CLUSTERING ORDER BY (table_name ASC, index_name ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'secondary index definitions'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system_schema | table | keyspaces`

```cql
CREATE TABLE system_schema.keyspaces (
    keyspace_name text,
    durable_writes boolean,
    next_replication frozen<map<text, text>>,
    replication frozen<map<text, text>>,
    replication_v2 frozen<map<text, text>>,
    PRIMARY KEY (keyspace_name)
) WITH bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'keyspace definitions'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system_schema | table | scylla_aggregates`

```cql
CREATE TABLE system_schema.scylla_aggregates (
    keyspace_name text,
    aggregate_name text,
    argument_types frozen<list<text>>,
    reduce_func text,
    state_type text,
    PRIMARY KEY (keyspace_name, aggregate_name, argument_types)
) WITH CLUSTERING ORDER BY (aggregate_name ASC, argument_types ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'scylla-specific information for user defined aggregates'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system_schema | table | scylla_keyspaces`

```cql
CREATE TABLE system_schema.scylla_keyspaces (
    keyspace_name text,
    consistency text,
    initial_tablets int,
    storage_options frozen<map<text, text>>,
    storage_type text,
    PRIMARY KEY (keyspace_name)
) WITH bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'scylla-specific information for keyspaces'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system_schema | table | scylla_tables`

```cql
CREATE TABLE system_schema.scylla_tables (
    keyspace_name text,
    table_name text,
    cdc frozen<map<text, text>>,
    committed_by_group0 boolean,
    in_memory boolean,
    large_data_guardrails_enabled boolean,
    partitioner text,
    storage_engine text,
    tablets frozen<map<text, text>>,
    version uuid,
    PRIMARY KEY (keyspace_name, table_name)
) WITH CLUSTERING ORDER BY (table_name ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system_schema | table | tables`

```cql
CREATE TABLE system_schema.tables (
    keyspace_name text,
    table_name text,
    bloom_filter_fp_chance double,
    caching frozen<map<text, text>>,
    comment text,
    compaction frozen<map<text, text>>,
    compression frozen<map<text, text>>,
    crc_check_chance double,
    dclocal_read_repair_chance double,
    default_time_to_live int,
    extensions frozen<map<text, blob>>,
    flags frozen<set<text>>,
    gc_grace_seconds int,
    id uuid,
    max_index_interval int,
    memtable_flush_period_in_ms int,
    min_index_interval int,
    read_repair_chance double,
    speculative_retry text,
    PRIMARY KEY (keyspace_name, table_name)
) WITH CLUSTERING ORDER BY (table_name ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'table definitions'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system_schema | table | triggers`

```cql
CREATE TABLE system_schema.triggers (
    keyspace_name text,
    table_name text,
    trigger_name text,
    options frozen<map<text, text>>,
    PRIMARY KEY (keyspace_name, table_name, trigger_name)
) WITH CLUSTERING ORDER BY (table_name ASC, trigger_name ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'trigger definitions'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system_schema | table | types`

```cql
CREATE TABLE system_schema.types (
    keyspace_name text,
    type_name text,
    field_names frozen<list<text>>,
    field_types frozen<list<text>>,
    PRIMARY KEY (keyspace_name, type_name)
) WITH CLUSTERING ORDER BY (type_name ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'user defined type definitions'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system_schema | table | view_virtual_columns`

```cql
CREATE TABLE system_schema.view_virtual_columns (
    keyspace_name text,
    table_name text,
    column_name text,
    clustering_order text,
    column_name_bytes blob,
    kind text,
    position int,
    type text,
    PRIMARY KEY (keyspace_name, table_name, column_name)
) WITH CLUSTERING ORDER BY (table_name ASC, column_name ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'column definitions'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system_schema | table | views`

```cql
CREATE TABLE system_schema.views (
    keyspace_name text,
    view_name text,
    base_table_id uuid,
    base_table_name text,
    bloom_filter_fp_chance double,
    caching frozen<map<text, text>>,
    comment text,
    compaction frozen<map<text, text>>,
    compression frozen<map<text, text>>,
    crc_check_chance double,
    dclocal_read_repair_chance double,
    default_time_to_live int,
    extensions frozen<map<text, blob>>,
    gc_grace_seconds int,
    id uuid,
    include_all_columns boolean,
    max_index_interval int,
    memtable_flush_period_in_ms int,
    min_index_interval int,
    read_repair_chance double,
    speculative_retry text,
    where_clause text,
    PRIMARY KEY (keyspace_name, view_name)
) WITH CLUSTERING ORDER BY (view_name ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'view definitions'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system_distributed | keyspace | system_distributed`

```cql
CREATE KEYSPACE system_distributed WITH replication = {'class': 'org.apache.cassandra.locator.SimpleStrategy', 'replication_factor': '3'} AND durable_writes = true AND tablets = {'enabled': false};
```
`system_distributed | table | cdc_generation_timestamps`

```cql
CREATE TABLE system_distributed.cdc_generation_timestamps (
    key text,
    time timestamp,
    expired timestamp,
    PRIMARY KEY (key, time)
) WITH CLUSTERING ORDER BY (time DESC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system_distributed | table | cdc_streams_descriptions_v2`

```cql
CREATE TABLE system_distributed.cdc_streams_descriptions_v2 (
    time timestamp,
    range_end bigint,
    streams frozen<set<blob>>,
    PRIMARY KEY (time, range_end)
) WITH CLUSTERING ORDER BY (range_end ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system_distributed | table | snapshot_keyspaces`

```cql
CREATE TABLE system_distributed.snapshot_keyspaces (
    snapshot_name text,
    keyspace_name text,
    keyspace_schema text,
    PRIMARY KEY (snapshot_name, keyspace_name)
) WITH CLUSTERING ORDER BY (keyspace_name ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system_distributed | table | snapshot_nodes`

```cql
CREATE TABLE system_distributed.snapshot_nodes (
    snapshot_name text,
    datacenter text,
    rack text,
    node uuid,
    PRIMARY KEY (snapshot_name, datacenter, rack, node)
) WITH CLUSTERING ORDER BY (datacenter ASC, rack ASC, node ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system_distributed | table | snapshot_remote_locations`

```cql
CREATE TABLE system_distributed.snapshot_remote_locations (
    snapshot_name text,
    datacenter text,
    bucket text,
    endpoint text,
    prefix text,
    state int,
    PRIMARY KEY (snapshot_name, datacenter)
) WITH CLUSTERING ORDER BY (datacenter ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system_distributed | table | snapshot_sstables`

```cql
CREATE TABLE system_distributed.snapshot_sstables (
    snapshot_name text,
    "keyspace" text,
    "table" text,
    datacenter text,
    rack text,
    first_token bigint,
    sstable_id uuid,
    data_size bigint,
    downloaded boolean,
    index_size bigint,
    last_token bigint,
    node uuid,
    prefix text,
    repaired_at bigint,
    state int,
    tablet bigint,
    toc_name text,
    PRIMARY KEY ((snapshot_name, "keyspace", "table", datacenter, rack), first_token, sstable_id)
) WITH CLUSTERING ORDER BY (first_token ASC, sstable_id ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system_distributed | table | snapshot_tables`

```cql
CREATE TABLE system_distributed.snapshot_tables (
    snapshot_name text,
    keyspace_name text,
    table_name text,
    base_table_id uuid,
    table_id uuid,
    table_schema text,
    tablet_layout text,
    type int,
    PRIMARY KEY (snapshot_name, keyspace_name, table_name)
) WITH CLUSTERING ORDER BY (keyspace_name ASC, table_name ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system_distributed | table | snapshot_tablets`

```cql
CREATE TABLE system_distributed.snapshot_tablets (
    snapshot_name text,
    keyspace_name text,
    table_name text,
    datacenter text,
    first_token bigint,
    last_token bigint,
    repair_time timestamp,
    repaired_at bigint,
    tablet_id bigint,
    PRIMARY KEY ((snapshot_name, keyspace_name, table_name, datacenter), first_token)
) WITH CLUSTERING ORDER BY (first_token ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system_distributed | table | snapshots`

```cql
CREATE TABLE system_distributed.snapshots (
    name text,
    created_at timestamp,
    expires_at timestamp,
    manifest_version text,
    namespace_version text,
    PRIMARY KEY (name)
) WITH bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system_distributed | table | view_build_status`

```cql
CREATE TABLE system_distributed.view_build_status (
    keyspace_name text,
    view_name text,
    host_id uuid,
    status text,
    PRIMARY KEY ((keyspace_name, view_name), host_id)
) WITH CLUSTERING ORDER BY (host_id ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`system_auth | keyspace | system_auth`

```cql
CREATE KEYSPACE system_auth WITH replication = {'class': 'org.apache.cassandra.locator.SimpleStrategy', 'replication_factor': '1'} AND durable_writes = true AND tablets = {'enabled': false};
```
`system_replicated_keys | keyspace | system_replicated_keys`

```cql
CREATE KEYSPACE system_replicated_keys WITH replication = {'class': 'org.apache.cassandra.locator.EverywhereStrategy'} AND durable_writes = true AND tablets = {'enabled': false};
```
`system_replicated_keys | table | encrypted_keys`

```cql
CREATE TABLE system_replicated_keys.encrypted_keys (
    key_file text,
    cipher text,
    strength int,
    key_id timeuuid,
    key text,
    PRIMARY KEY ((key_file, cipher), strength, key_id)
) WITH CLUSTERING ORDER BY (strength ASC, key_id ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE';

```
`audit | keyspace | audit` — create_statement identical to the one under `DESCRIBE SCHEMA`

`audit | table | audit_log` — create_statement identical to the one under `DESCRIBE SCHEMA`

`cql_ks | keyspace | cql_ks` — create_statement identical to the one under `DESCRIBE SCHEMA`

`cql_ks | table | t` — create_statement identical to the one under `DESCRIBE SCHEMA`

`ref | keyspace | ref` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | type | address` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | type | person` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | "QuotedTable"` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | "Quoted Idx"` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_all` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_b_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_c2_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_fl_l_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_local_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_m_entries_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_m_keys_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_s_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_st_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_txt_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | view | t_all_by_txt` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_cdc` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_cdc_scylla_cdc_log` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_comp2` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_counter` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_ics` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_id` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_opts` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_pgs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_simple` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | view | t_simple_by_v` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_tgc` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_twcs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_vec` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_vec_emb_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_vec_emb_vs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_vec_scylla_cdc_log` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref_vnodes | keyspace | ref_vnodes` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes`

`ref_vnodes | table | base` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes`

`ref_vnodes | index | base_v_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes`

`ref_vnodes | view | base_by_v` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes`

`ref_simple | keyspace | ref_simple` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_simple`

### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | keyspace | ref` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | type | address` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | type | person` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | "QuotedTable"` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | "QuotedIdx"` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_all` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_b_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_c2_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_fl_l_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_m_entries_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_m_keys_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_s_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_st_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_txt_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_txt_sai` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_cdc2` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_comp1` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_comp3` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_compact` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_compact2` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_counter` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_id` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_mt` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_opts` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_rr` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_simple` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_simple_v_sai` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_twcs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_ucs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_vec` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_vec_emb_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref_simple | keyspace | ref_simple` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_simple`

`ref_vnodes | keyspace | ref_vnodes` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes`

`ref_vnodes | table | base` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes`

`ref_vnodes | index | base_v_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes`

`system | keyspace | system`

```cql
CREATE KEYSPACE system WITH replication = {'class': 'LocalStrategy'}  AND durable_writes = true;
```
`system | table | available_ranges`

```cql
CREATE TABLE system.available_ranges (
    keyspace_name text PRIMARY KEY,
    ranges set<blob>
) WITH additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'available keyspace/ranges during bootstrap/replace that are ready to be served'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 0
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system | table | available_ranges_v2`

```cql
CREATE TABLE system.available_ranges_v2 (
    keyspace_name text PRIMARY KEY,
    full_ranges set<blob>,
    transient_ranges set<blob>
) WITH additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'available keyspace/ranges during bootstrap/replace that are ready to be served'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 0
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system | table | batches`

```cql
CREATE TABLE system.batches (
    id timeuuid PRIMARY KEY,
    version int,
    mutations list<blob>
) WITH additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'batches awaiting replay'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '2'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 0
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system | table | built_views`

```cql
CREATE TABLE system.built_views (
    keyspace_name text,
    view_name text,
    status_replicated boolean,
    PRIMARY KEY (keyspace_name, view_name)
) WITH CLUSTERING ORDER BY (view_name ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'built views'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 0
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system | table | compaction_history`

```cql
CREATE TABLE system.compaction_history (
    id timeuuid PRIMARY KEY,
    bytes_in bigint,
    bytes_out bigint,
    columnfamily_name text,
    compacted_at timestamp,
    compaction_properties frozen<map<text, text>>,
    keyspace_name text,
    rows_merged map<int, bigint>
) WITH additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'week-long compaction history'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 604800
    AND extensions = {}
    AND gc_grace_seconds = 0
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system | table | "IndexInfo"`

```cql
CREATE TABLE system."IndexInfo" (
    table_name text,
    index_name text,
    value blob,
    PRIMARY KEY (table_name, index_name)
) WITH CLUSTERING ORDER BY (index_name ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'built column indexes'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 0
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system | table | local`

```cql
CREATE TABLE system.local (
    key text PRIMARY KEY,
    bootstrapped text,
    broadcast_address inet,
    broadcast_port int,
    cluster_name text,
    cql_version text,
    data_center text,
    gossip_generation int,
    host_id uuid,
    listen_address inet,
    listen_port int,
    native_protocol_version text,
    partitioner text,
    rack text,
    release_version text,
    rpc_address inet,
    rpc_port int,
    schema_version uuid,
    tokens set<text>,
    truncated_at map<uuid, blob>
) WITH additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'information about the local node'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 0
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system | table | paxos`

```cql
CREATE TABLE system.paxos (
    row_key blob,
    cf_id uuid,
    in_progress_ballot timeuuid,
    in_progress_read_ballot timeuuid,
    most_recent_commit blob,
    most_recent_commit_at timeuuid,
    most_recent_commit_version int,
    proposal blob,
    proposal_ballot timeuuid,
    proposal_version int,
    PRIMARY KEY (row_key, cf_id)
) WITH CLUSTERING ORDER BY (cf_id ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'in-progress paxos proposals'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.LeveledCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 0
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system | index | "PaxosUncommittedIndex"`

```cql
CREATE CUSTOM INDEX "PaxosUncommittedIndex" ON system.paxos () USING 'org.apache.cassandra.service.paxos.uncommitted.PaxosUncommittedIndex';
```
`system | table | paxos_repair_history`

```cql
CREATE TABLE system.paxos_repair_history (
    keyspace_name text,
    table_name text,
    points frozen<list<frozen<tuple<blob, timeuuid>>>>,
    PRIMARY KEY (keyspace_name, table_name)
) WITH CLUSTERING ORDER BY (table_name ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'paxos repair history'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 0
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system | table | peer_events`

```cql
CREATE TABLE system.peer_events (
    peer inet PRIMARY KEY,
    hints_dropped map<timeuuid, int>
) WITH additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'events related to peers'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 0
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system | table | peer_events_v2`

```cql
CREATE TABLE system.peer_events_v2 (
    peer inet,
    peer_port int,
    hints_dropped map<timeuuid, int>,
    PRIMARY KEY (peer, peer_port)
) WITH CLUSTERING ORDER BY (peer_port ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'events related to peers'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 0
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system | table | peers`

```cql
CREATE TABLE system.peers (
    peer inet PRIMARY KEY,
    data_center text,
    host_id uuid,
    preferred_ip inet,
    rack text,
    release_version text,
    rpc_address inet,
    schema_version uuid,
    tokens set<text>
) WITH additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'information about known peers in the cluster'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 0
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system | table | peers_v2`

```cql
CREATE TABLE system.peers_v2 (
    peer inet,
    peer_port int,
    data_center text,
    host_id uuid,
    native_address inet,
    native_port int,
    preferred_ip inet,
    preferred_port int,
    rack text,
    release_version text,
    schema_version uuid,
    tokens set<text>,
    PRIMARY KEY (peer, peer_port)
) WITH CLUSTERING ORDER BY (peer_port ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'information about known peers in the cluster'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 0
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system | table | prepared_statements`

```cql
CREATE TABLE system.prepared_statements (
    prepared_id blob PRIMARY KEY,
    logged_keyspace text,
    query_string text
) WITH additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'prepared statements'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 0
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system | table | repairs`

```cql
CREATE TABLE system.repairs (
    parent_id timeuuid PRIMARY KEY,
    coordinator inet,
    coordinator_port int,
    last_update timestamp,
    repaired_at timestamp,
    started_at timestamp,
    state int,
    cfids set<uuid>,
    participants set<inet>,
    participants_wp set<text>,
    ranges set<blob>
) WITH additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'repairs'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 0
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system | table | size_estimates`

```cql
CREATE TABLE system.size_estimates (
    keyspace_name text,
    table_name text,
    range_start text,
    range_end text,
    mean_partition_size bigint,
    partitions_count bigint,
    PRIMARY KEY (keyspace_name, table_name, range_start, range_end)
) WITH CLUSTERING ORDER BY (table_name ASC, range_start ASC, range_end ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'per-table primary range size estimates, table is deprecated in favor of table_estimates'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 0
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system | table | sstable_activity`

```cql
CREATE TABLE system.sstable_activity (
    keyspace_name text,
    columnfamily_name text,
    generation int,
    rate_120m double,
    rate_15m double,
    PRIMARY KEY ((keyspace_name, columnfamily_name, generation))
) WITH additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'historic sstable read rates'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 0
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system | table | sstable_activity_v2`

```cql
CREATE TABLE system.sstable_activity_v2 (
    keyspace_name text,
    table_name text,
    id text,
    rate_120m double,
    rate_15m double,
    PRIMARY KEY ((keyspace_name, table_name, id))
) WITH additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'historic sstable read rates'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 0
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system | table | table_estimates`

```cql
CREATE TABLE system.table_estimates (
    keyspace_name text,
    table_name text,
    range_type text,
    range_start text,
    range_end text,
    mean_partition_size bigint,
    partitions_count bigint,
    PRIMARY KEY (keyspace_name, table_name, range_type, range_start, range_end)
) WITH CLUSTERING ORDER BY (table_name ASC, range_type ASC, range_start ASC, range_end ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'per-table range size estimates'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 0
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system | table | top_partitions`

```cql
CREATE TABLE system.top_partitions (
    keyspace_name text,
    table_name text,
    top_type text,
    last_update timestamp,
    top frozen<list<frozen<tuple<text, bigint>>>>,
    PRIMARY KEY (keyspace_name, table_name, top_type)
) WITH CLUSTERING ORDER BY (table_name ASC, top_type ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'Stores the top partitions'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 0
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system | table | transferred_ranges`

```cql
CREATE TABLE system.transferred_ranges (
    operation text,
    keyspace_name text,
    peer inet,
    ranges set<blob>,
    PRIMARY KEY ((operation, keyspace_name), peer)
) WITH CLUSTERING ORDER BY (peer ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'record of transferred ranges for streaming operation'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 0
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system | table | transferred_ranges_v2`

```cql
CREATE TABLE system.transferred_ranges_v2 (
    operation text,
    keyspace_name text,
    peer inet,
    peer_port int,
    ranges set<blob>,
    PRIMARY KEY ((operation, keyspace_name), peer, peer_port)
) WITH CLUSTERING ORDER BY (peer ASC, peer_port ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'record of transferred ranges for streaming operation'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 0
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system | table | view_builds_in_progress`

```cql
CREATE TABLE system.view_builds_in_progress (
    keyspace_name text,
    view_name text,
    start_token text,
    end_token text,
    keys_built bigint,
    last_token text,
    PRIMARY KEY (keyspace_name, view_name, start_token, end_token)
) WITH CLUSTERING ORDER BY (view_name ASC, start_token ASC, end_token ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'views builds current progress'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 0
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system_auth | keyspace | system_auth`

```cql
CREATE KEYSPACE system_auth WITH replication = {'class': 'SimpleStrategy', 'replication_factor': '1'}  AND durable_writes = true;
```
`system_auth | table | cidr_groups`

```cql
CREATE TABLE system_auth.cidr_groups (
    cidr_group text PRIMARY KEY,
    cidrs frozen<set<frozen<tuple<inet, smallint>>>>
) WITH additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'cidr groups to cidrs mapping'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 7776000
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system_auth | table | cidr_permissions`

```cql
CREATE TABLE system_auth.cidr_permissions (
    role text PRIMARY KEY,
    cidr_groups frozen<set<text>>
) WITH additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'user cidr permissions'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 7776000
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system_auth | table | identity_to_role`

```cql
CREATE TABLE system_auth.identity_to_role (
    identity text PRIMARY KEY,
    role text
) WITH additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'mtls authorized identities lookup table'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 7776000
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system_auth | table | network_permissions`

```cql
CREATE TABLE system_auth.network_permissions (
    role text PRIMARY KEY,
    dcs frozen<set<text>>
) WITH additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'user network permissions'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 7776000
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system_auth | table | resource_role_permissons_index`

```cql
CREATE TABLE system_auth.resource_role_permissons_index (
    resource text,
    role text,
    PRIMARY KEY (resource, role)
) WITH CLUSTERING ORDER BY (role ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'index of db roles with permissions granted on a resource'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 7776000
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system_auth | table | role_members`

```cql
CREATE TABLE system_auth.role_members (
    role text,
    member text,
    PRIMARY KEY (role, member)
) WITH CLUSTERING ORDER BY (member ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'role memberships lookup table'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 7776000
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system_auth | table | role_permissions`

```cql
CREATE TABLE system_auth.role_permissions (
    role text,
    resource text,
    permissions set<text>,
    PRIMARY KEY (role, resource)
) WITH CLUSTERING ORDER BY (resource ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'permissions granted to db roles'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 7776000
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system_auth | table | roles`

```cql
CREATE TABLE system_auth.roles (
    role text PRIMARY KEY,
    can_login boolean,
    is_superuser boolean,
    salted_hash text,
    member_of set<text>
) WITH additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'role definitions'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 7776000
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system_distributed | keyspace | system_distributed`

```cql
CREATE KEYSPACE system_distributed WITH replication = {'class': 'SimpleStrategy', 'replication_factor': '3'}  AND durable_writes = true;
```
`system_distributed | table | parent_repair_history`

```cql
CREATE TABLE system_distributed.parent_repair_history (
    parent_id timeuuid PRIMARY KEY,
    exception_message text,
    exception_stacktrace text,
    finished_at timestamp,
    keyspace_name text,
    started_at timestamp,
    columnfamily_names set<text>,
    options map<text, text>,
    requested_ranges set<text>,
    successful_ranges set<text>
) WITH additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'Repair history'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.TimeWindowCompactionStrategy', 'compaction_window_size': '1', 'compaction_window_unit': 'DAYS', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 2592000
    AND extensions = {}
    AND gc_grace_seconds = 864000
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system_distributed | table | partition_denylist`

```cql
CREATE TABLE system_distributed.partition_denylist (
    ks_name text,
    table_name text,
    key blob,
    PRIMARY KEY ((ks_name, table_name), key)
) WITH CLUSTERING ORDER BY (key ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'Partition keys which have been denied access'
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
`system_distributed | table | repair_history`

```cql
CREATE TABLE system_distributed.repair_history (
    keyspace_name text,
    columnfamily_name text,
    id timeuuid,
    coordinator inet,
    coordinator_port int,
    exception_message text,
    exception_stacktrace text,
    finished_at timestamp,
    parent_id timeuuid,
    range_begin text,
    range_end text,
    started_at timestamp,
    status text,
    participants set<inet>,
    participants_v2 set<text>,
    PRIMARY KEY ((keyspace_name, columnfamily_name), id)
) WITH CLUSTERING ORDER BY (id ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'Repair history'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.TimeWindowCompactionStrategy', 'compaction_window_size': '1', 'compaction_window_unit': 'DAYS', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 2592000
    AND extensions = {}
    AND gc_grace_seconds = 864000
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system_distributed | table | view_build_status`

```cql
CREATE TABLE system_distributed.view_build_status (
    keyspace_name text,
    view_name text,
    host_id uuid,
    status text,
    PRIMARY KEY ((keyspace_name, view_name), host_id)
) WITH CLUSTERING ORDER BY (host_id ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'Materialized View build status'
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
`system_schema | keyspace | system_schema`

```cql
CREATE KEYSPACE system_schema WITH replication = {'class': 'LocalStrategy'}  AND durable_writes = true;
```
`system_schema | table | aggregates`

```cql
CREATE TABLE system_schema.aggregates (
    keyspace_name text,
    aggregate_name text,
    argument_types frozen<list<text>>,
    final_func text,
    initcond text,
    return_type text,
    state_func text,
    state_type text,
    PRIMARY KEY (keyspace_name, aggregate_name, argument_types)
) WITH CLUSTERING ORDER BY (aggregate_name ASC, argument_types ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'user defined aggregate definitions'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 604800
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system_schema | table | column_masks`

```cql
CREATE TABLE system_schema.column_masks (
    keyspace_name text,
    table_name text,
    column_name text,
    function_argument_nulls frozen<list<boolean>>,
    function_argument_types frozen<list<text>>,
    function_argument_values frozen<list<text>>,
    function_keyspace text,
    function_name text,
    PRIMARY KEY (keyspace_name, table_name, column_name)
) WITH CLUSTERING ORDER BY (table_name ASC, column_name ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'column dynamic data masks'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 604800
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system_schema | table | columns`

```cql
CREATE TABLE system_schema.columns (
    keyspace_name text,
    table_name text,
    column_name text,
    clustering_order text,
    column_name_bytes blob,
    kind text,
    position int,
    type text,
    PRIMARY KEY (keyspace_name, table_name, column_name)
) WITH CLUSTERING ORDER BY (table_name ASC, column_name ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'column definitions'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 604800
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system_schema | table | dropped_columns`

```cql
CREATE TABLE system_schema.dropped_columns (
    keyspace_name text,
    table_name text,
    column_name text,
    dropped_time timestamp,
    kind text,
    type text,
    PRIMARY KEY (keyspace_name, table_name, column_name)
) WITH CLUSTERING ORDER BY (table_name ASC, column_name ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'dropped column registry'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 604800
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system_schema | table | functions`

```cql
CREATE TABLE system_schema.functions (
    keyspace_name text,
    function_name text,
    argument_types frozen<list<text>>,
    argument_names frozen<list<text>>,
    body text,
    called_on_null_input boolean,
    language text,
    return_type text,
    PRIMARY KEY (keyspace_name, function_name, argument_types)
) WITH CLUSTERING ORDER BY (function_name ASC, argument_types ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'user defined function definitions'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 604800
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system_schema | table | indexes`

```cql
CREATE TABLE system_schema.indexes (
    keyspace_name text,
    table_name text,
    index_name text,
    kind text,
    options frozen<map<text, text>>,
    PRIMARY KEY (keyspace_name, table_name, index_name)
) WITH CLUSTERING ORDER BY (table_name ASC, index_name ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'secondary index definitions'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 604800
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system_schema | table | keyspaces`

```cql
CREATE TABLE system_schema.keyspaces (
    keyspace_name text PRIMARY KEY,
    durable_writes boolean,
    replication frozen<map<text, text>>
) WITH additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'keyspace definitions'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 604800
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system_schema | table | tables`

```cql
CREATE TABLE system_schema.tables (
    keyspace_name text,
    table_name text,
    additional_write_policy text,
    allow_auto_snapshot boolean,
    bloom_filter_fp_chance double,
    caching frozen<map<text, text>>,
    cdc boolean,
    comment text,
    compaction frozen<map<text, text>>,
    compression frozen<map<text, text>>,
    crc_check_chance double,
    dclocal_read_repair_chance double,
    default_time_to_live int,
    extensions frozen<map<text, blob>>,
    flags frozen<set<text>>,
    gc_grace_seconds int,
    id uuid,
    incremental_backups boolean,
    max_index_interval int,
    memtable text,
    memtable_flush_period_in_ms int,
    min_index_interval int,
    read_repair text,
    read_repair_chance double,
    speculative_retry text,
    PRIMARY KEY (keyspace_name, table_name)
) WITH CLUSTERING ORDER BY (table_name ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'table definitions'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 604800
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system_schema | table | triggers`

```cql
CREATE TABLE system_schema.triggers (
    keyspace_name text,
    table_name text,
    trigger_name text,
    options frozen<map<text, text>>,
    PRIMARY KEY (keyspace_name, table_name, trigger_name)
) WITH CLUSTERING ORDER BY (table_name ASC, trigger_name ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'trigger definitions'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 604800
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system_schema | table | types`

```cql
CREATE TABLE system_schema.types (
    keyspace_name text,
    type_name text,
    field_names frozen<list<text>>,
    field_types frozen<list<text>>,
    PRIMARY KEY (keyspace_name, type_name)
) WITH CLUSTERING ORDER BY (type_name ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'user defined type definitions'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 604800
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system_schema | table | views`

```cql
CREATE TABLE system_schema.views (
    keyspace_name text,
    view_name text,
    additional_write_policy text,
    allow_auto_snapshot boolean,
    base_table_id uuid,
    base_table_name text,
    bloom_filter_fp_chance double,
    caching frozen<map<text, text>>,
    cdc boolean,
    comment text,
    compaction frozen<map<text, text>>,
    compression frozen<map<text, text>>,
    crc_check_chance double,
    dclocal_read_repair_chance double,
    default_time_to_live int,
    extensions frozen<map<text, blob>>,
    gc_grace_seconds int,
    id uuid,
    include_all_columns boolean,
    incremental_backups boolean,
    max_index_interval int,
    memtable text,
    memtable_flush_period_in_ms int,
    min_index_interval int,
    read_repair text,
    read_repair_chance double,
    speculative_retry text,
    where_clause text,
    PRIMARY KEY (keyspace_name, view_name)
) WITH CLUSTERING ORDER BY (view_name ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'view definitions'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 604800
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 3600000
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system_traces | keyspace | system_traces`

```cql
CREATE KEYSPACE system_traces WITH replication = {'class': 'SimpleStrategy', 'replication_factor': '2'}  AND durable_writes = true;
```
`system_traces | table | events`

```cql
CREATE TABLE system_traces.events (
    session_id uuid,
    event_id timeuuid,
    activity text,
    source inet,
    source_elapsed int,
    source_port int,
    thread text,
    PRIMARY KEY (session_id, event_id)
) WITH CLUSTERING ORDER BY (event_id ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'tracing events'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 0
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system_traces | table | sessions`

```cql
CREATE TABLE system_traces.sessions (
    session_id uuid PRIMARY KEY,
    client inet,
    command text,
    coordinator inet,
    coordinator_port int,
    duration int,
    request text,
    started_at timestamp,
    parameters map<text, text>
) WITH additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'tracing sessions'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 0
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`system_views | keyspace | system_views`

```cql
/*
Warning: Keyspace system_views is a virtual keyspace and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL KEYSPACE system_views;
*/
```
`system_views | table | batch_metrics`

```cql
/*
Warning: Table system_views.batch_metrics is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.batch_metrics (
    name text PRIMARY KEY,
    max bigint,
    p50th double,
    p999th double,
    p99th double
) WITH comment = 'Metrics specific to batch statements';
*/
```
`system_views | table | caches`

```cql
/*
Warning: Table system_views.caches is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.caches (
    name text PRIMARY KEY,
    capacity_bytes bigint,
    entry_count int,
    hit_count bigint,
    hit_ratio double,
    recent_hit_rate_per_second bigint,
    recent_request_rate_per_second bigint,
    request_count bigint,
    size_bytes bigint
) WITH comment = 'system caches';
*/
```
`system_views | table | cidr_filtering_metrics_counts`

```cql
/*
Warning: Table system_views.cidr_filtering_metrics_counts is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.cidr_filtering_metrics_counts (
    name text PRIMARY KEY,
    value bigint
) WITH comment = 'Count metrics specific to CIDR filtering';
*/
```
`system_views | table | cidr_filtering_metrics_latencies`

```cql
/*
Warning: Table system_views.cidr_filtering_metrics_latencies is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.cidr_filtering_metrics_latencies (
    name text PRIMARY KEY,
    max double,
    p50th double,
    p95th double,
    p999th double,
    p99th double
) WITH comment = 'Latency metrics specific to CIDR filtering';
*/
```
`system_views | table | clients`

```cql
/*
Warning: Table system_views.clients is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.clients (
    address inet,
    port int,
    client_options frozen<map<text, text>>,
    connection_stage text,
    driver_name text,
    driver_version text,
    hostname text,
    keyspace_name text,
    protocol_version int,
    request_count bigint,
    ssl_cipher_suite text,
    ssl_enabled boolean,
    ssl_protocol text,
    username text,
    PRIMARY KEY (address, port)
) WITH CLUSTERING ORDER BY (port ASC)
    AND comment = 'currently connected clients';
*/
```
`system_views | table | coordinator_read_latency`

```cql
/*
Warning: Table system_views.coordinator_read_latency is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.coordinator_read_latency (
    keyspace_name text,
    table_name text,
    count bigint,
    max_ms double,
    p50th_ms double,
    p99th_ms double,
    per_second double,
    PRIMARY KEY ((keyspace_name, table_name))
) WITH comment = '';
*/
```
`system_views | table | coordinator_scan_latency`

```cql
/*
Warning: Table system_views.coordinator_scan_latency is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.coordinator_scan_latency (
    keyspace_name text,
    table_name text,
    count bigint,
    max_ms double,
    p50th_ms double,
    p99th_ms double,
    per_second double,
    PRIMARY KEY ((keyspace_name, table_name))
) WITH comment = '';
*/
```
`system_views | table | coordinator_write_latency`

```cql
/*
Warning: Table system_views.coordinator_write_latency is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.coordinator_write_latency (
    keyspace_name text,
    table_name text,
    count bigint,
    max_ms double,
    p50th_ms double,
    p99th_ms double,
    per_second double,
    PRIMARY KEY ((keyspace_name, table_name))
) WITH comment = '';
*/
```
`system_views | table | cql_metrics`

```cql
/*
Warning: Table system_views.cql_metrics is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.cql_metrics (
    name text PRIMARY KEY,
    value double
) WITH comment = 'Metrics specific to CQL prepared statement caching';
*/
```
`system_views | table | credentials_cache_keys`

```cql
/*
Warning: Table system_views.credentials_cache_keys is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.credentials_cache_keys (
    role text PRIMARY KEY
) WITH comment = 'keys in the credentials cache';
*/
```
`system_views | table | disk_usage`

```cql
/*
Warning: Table system_views.disk_usage is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.disk_usage (
    keyspace_name text,
    table_name text,
    mebibytes bigint,
    PRIMARY KEY ((keyspace_name, table_name))
) WITH comment = '';
*/
```
`system_views | table | gossip_info`

```cql
/*
Warning: Table system_views.gossip_info is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.gossip_info (
    address inet,
    port int,
    dc text,
    dc_version int,
    disk_usage text,
    disk_usage_version int,
    generation int,
    heartbeat int,
    host_id text,
    host_id_version int,
    hostname text,
    index_status text,
    index_status_version int,
    internal_address_and_port text,
    internal_address_and_port_version int,
    internal_ip text,
    internal_ip_version int,
    load text,
    load_version int,
    native_address_and_port text,
    native_address_and_port_version int,
    net_version text,
    net_version_version int,
    rack text,
    rack_version int,
    release_version text,
    release_version_version int,
    removal_coordinator text,
    removal_coordinator_version int,
    rpc_address text,
    rpc_address_version int,
    rpc_ready text,
    rpc_ready_version int,
    "schema" text,
    schema_version int,
    severity text,
    severity_version int,
    sstable_versions text,
    sstable_versions_version int,
    status text,
    status_version int,
    status_with_port text,
    status_with_port_version int,
    tokens_version int,
    x1 text,
    x10 text,
    x10_version int,
    x1_version int,
    x2 text,
    x2_version int,
    x3 text,
    x3_version int,
    x4 text,
    x4_version int,
    x5 text,
    x5_version int,
    x6 text,
    x6_version int,
    x7 text,
    x7_version int,
    x8 text,
    x8_version int,
    x9 text,
    x9_version int,
    x_11_padding text,
    x_11_padding_version int,
    PRIMARY KEY (address, port)
) WITH CLUSTERING ORDER BY (port ASC)
    AND comment = 'lists the gossip information for the cluster';
*/
```
`system_views | table | internode_inbound`

```cql
/*
Warning: Table system_views.internode_inbound is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.internode_inbound (
    address inet,
    port int,
    dc text,
    rack text,
    corrupt_frames_recovered bigint,
    corrupt_frames_unrecovered bigint,
    error_bytes bigint,
    error_count bigint,
    expired_bytes bigint,
    expired_count bigint,
    processed_bytes bigint,
    processed_count bigint,
    received_bytes bigint,
    received_count bigint,
    scheduled_bytes bigint,
    scheduled_count bigint,
    throttled_count bigint,
    throttled_nanos bigint,
    using_bytes bigint,
    using_reserve_bytes bigint,
    PRIMARY KEY ((address, port), dc, rack)
) WITH CLUSTERING ORDER BY (dc ASC, rack ASC)
    AND comment = '';
*/
```
`system_views | table | internode_outbound`

```cql
/*
Warning: Table system_views.internode_outbound is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.internode_outbound (
    address inet,
    port int,
    dc text,
    rack text,
    active_connections bigint,
    connection_attempts bigint,
    error_bytes bigint,
    error_count bigint,
    expired_bytes bigint,
    expired_count bigint,
    overload_bytes bigint,
    overload_count bigint,
    pending_bytes bigint,
    pending_count bigint,
    sent_bytes bigint,
    sent_count bigint,
    successful_connection_attempts bigint,
    using_bytes bigint,
    using_reserve_bytes bigint,
    PRIMARY KEY ((address, port), dc, rack)
) WITH CLUSTERING ORDER BY (dc ASC, rack ASC)
    AND comment = '';
*/
```
`system_views | table | jmx_permissions_cache_keys`

```cql
/*
Warning: Table system_views.jmx_permissions_cache_keys is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.jmx_permissions_cache_keys (
    role text PRIMARY KEY
) WITH comment = 'keys in the JMX permissions cache';
*/
```
`system_views | table | local_read_latency`

```cql
/*
Warning: Table system_views.local_read_latency is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.local_read_latency (
    keyspace_name text,
    table_name text,
    count bigint,
    max_ms double,
    p50th_ms double,
    p99th_ms double,
    per_second double,
    PRIMARY KEY ((keyspace_name, table_name))
) WITH comment = '';
*/
```
`system_views | table | local_scan_latency`

```cql
/*
Warning: Table system_views.local_scan_latency is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.local_scan_latency (
    keyspace_name text,
    table_name text,
    count bigint,
    max_ms double,
    p50th_ms double,
    p99th_ms double,
    per_second double,
    PRIMARY KEY ((keyspace_name, table_name))
) WITH comment = '';
*/
```
`system_views | table | local_write_latency`

```cql
/*
Warning: Table system_views.local_write_latency is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.local_write_latency (
    keyspace_name text,
    table_name text,
    count bigint,
    max_ms double,
    p50th_ms double,
    p99th_ms double,
    per_second double,
    PRIMARY KEY ((keyspace_name, table_name))
) WITH comment = '';
*/
```
`system_views | table | max_partition_size`

```cql
/*
Warning: Table system_views.max_partition_size is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.max_partition_size (
    keyspace_name text,
    table_name text,
    mebibytes bigint,
    PRIMARY KEY ((keyspace_name, table_name))
) WITH comment = '';
*/
```
`system_views | table | max_sstable_duration`

```cql
/*
Warning: Table system_views.max_sstable_duration is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.max_sstable_duration (
    keyspace_name text,
    table_name text,
    max_sstable_duration bigint,
    PRIMARY KEY ((keyspace_name, table_name))
) WITH comment = '';
*/
```
`system_views | table | max_sstable_size`

```cql
/*
Warning: Table system_views.max_sstable_size is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.max_sstable_size (
    keyspace_name text,
    table_name text,
    mebibytes bigint,
    PRIMARY KEY ((keyspace_name, table_name))
) WITH comment = '';
*/
```
`system_views | table | network_permissions_cache_keys`

```cql
/*
Warning: Table system_views.network_permissions_cache_keys is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.network_permissions_cache_keys (
    role text PRIMARY KEY
) WITH comment = 'keys in the network permissions cache';
*/
```
`system_views | table | pending_hints`

```cql
/*
Warning: Table system_views.pending_hints is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.pending_hints (
    host_id uuid PRIMARY KEY,
    address inet,
    dc text,
    files int,
    newest timestamp,
    oldest timestamp,
    port int,
    rack text,
    status text
) WITH comment = 'Pending hints that this node has for other nodes';
*/
```
`system_views | table | permissions_cache_keys`

```cql
/*
Warning: Table system_views.permissions_cache_keys is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.permissions_cache_keys (
    role text,
    resource text,
    PRIMARY KEY ((role, resource))
) WITH comment = 'keys in the permissions cache';
*/
```
`system_views | table | queries`

```cql
/*
Warning: Table system_views.queries is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.queries (
    thread_id text PRIMARY KEY,
    queued_micros bigint,
    running_micros bigint,
    task text
) WITH comment = 'Lists currently running queries';
*/
```
`system_views | table | repair_jobs`

```cql
/*
Warning: Table system_views.repair_jobs is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.repair_jobs (
    id uuid PRIMARY KEY,
    completed boolean,
    duration_millis bigint,
    failure_cause text,
    keyspace_name text,
    last_updated_at timestamp,
    participants frozen<list<text>>,
    ranges frozen<list<text>>,
    repair_id timeuuid,
    session_id timeuuid,
    state_failure_timestamp timestamp,
    state_init_timestamp timestamp,
    state_skipped_timestamp timestamp,
    state_snapshot_complete_timestamp timestamp,
    state_snapshot_start_timestamp timestamp,
    state_start_timestamp timestamp,
    state_stream_start_timestamp timestamp,
    state_success_timestamp timestamp,
    state_validation_complete_timestamp timestamp,
    state_validation_start_timestamp timestamp,
    status text,
    success_message text,
    table_name text
) WITH comment = 'Repair job';
*/
```
`system_views | table | repair_participates`

```cql
/*
Warning: Table system_views.repair_participates is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.repair_participates (
    id timeuuid PRIMARY KEY,
    completed boolean,
    duration_millis bigint,
    failure_cause text,
    global boolean,
    incremental boolean,
    initiator text,
    last_updated_at timestamp,
    preview_kind text,
    ranges frozen<list<text>>,
    repaired_at timestamp,
    state_failure_timestamp timestamp,
    state_init_timestamp timestamp,
    state_skipped_timestamp timestamp,
    state_success_timestamp timestamp,
    success_message text,
    tables frozen<set<text>>,
    validations frozen<set<uuid>>
) WITH comment = 'Repair participate summary';
*/
```
`system_views | table | repair_sessions`

```cql
/*
Warning: Table system_views.repair_sessions is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.repair_sessions (
    id timeuuid PRIMARY KEY,
    completed boolean,
    duration_millis bigint,
    failure_cause text,
    jobs frozen<set<uuid>>,
    keyspace_name text,
    last_updated_at timestamp,
    participants frozen<list<text>>,
    ranges frozen<list<text>>,
    repair_id timeuuid,
    state_failure_timestamp timestamp,
    state_init_timestamp timestamp,
    state_jobs_start_timestamp timestamp,
    state_skipped_timestamp timestamp,
    state_start_timestamp timestamp,
    state_success_timestamp timestamp,
    status text,
    success_message text,
    table_names frozen<list<text>>
) WITH comment = 'Repair session';
*/
```
`system_views | table | repair_validations`

```cql
/*
Warning: Table system_views.repair_validations is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.repair_validations (
    id uuid PRIMARY KEY,
    bytes_read bigint,
    completed boolean,
    duration_millis bigint,
    estimated_partitions bigint,
    estimated_total_bytes bigint,
    failure_cause text,
    initiator text,
    keyspace_name text,
    last_updated_at timestamp,
    partitions_processed bigint,
    progress_percentage float,
    ranges frozen<list<text>>,
    repair_id timeuuid,
    session_id timeuuid,
    state_accept_timestamp timestamp,
    state_failure_timestamp timestamp,
    state_init_timestamp timestamp,
    state_sending_trees_timestamp timestamp,
    state_skipped_timestamp timestamp,
    state_start_timestamp timestamp,
    state_success_timestamp timestamp,
    status text,
    success_message text,
    table_name text
) WITH comment = 'Repair validation';
*/
```
`system_views | table | repairs`

```cql
/*
Warning: Table system_views.repairs is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.repairs (
    id timeuuid PRIMARY KEY,
    command_id int,
    completed boolean,
    duration_millis bigint,
    failure_cause text,
    keyspace_name text,
    last_updated_at timestamp,
    options_column_families frozen<set<text>>,
    options_data_centers frozen<set<text>>,
    options_force_repair boolean,
    options_hosts frozen<set<text>>,
    options_ignore_unreplicated_keyspaces boolean,
    options_incremental boolean,
    options_job_threads int,
    options_optimise_streams boolean,
    options_parallelism text,
    options_preview_kind text,
    options_primary_range boolean,
    options_pull_repair boolean,
    options_ranges frozen<set<text>>,
    options_subrange_repair boolean,
    options_trace boolean,
    participants frozen<list<text>>,
    ranges frozen<list<frozen<list<text>>>>,
    sessions frozen<set<timeuuid>>,
    state_failure_timestamp timestamp,
    state_init_timestamp timestamp,
    state_prepare_complete_timestamp timestamp,
    state_prepare_start_timestamp timestamp,
    state_repair_complete_timestamp timestamp,
    state_repair_start_timestamp timestamp,
    state_setup_timestamp timestamp,
    state_skipped_timestamp timestamp,
    state_start_timestamp timestamp,
    state_success_timestamp timestamp,
    status text,
    success_message text,
    table_names frozen<list<text>>,
    type text,
    unfiltered_ranges frozen<list<frozen<list<text>>>>
) WITH comment = 'Repair summary';
*/
```
`system_views | table | roles_cache_keys`

```cql
/*
Warning: Table system_views.roles_cache_keys is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.roles_cache_keys (
    role text PRIMARY KEY
) WITH comment = 'keys in the roles cache';
*/
```
`system_views | table | rows_per_read`

```cql
/*
Warning: Table system_views.rows_per_read is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.rows_per_read (
    keyspace_name text,
    table_name text,
    count bigint,
    max double,
    p50th double,
    p99th double,
    PRIMARY KEY ((keyspace_name, table_name))
) WITH comment = '';
*/
```
`system_views | table | sai_column_indexes`

```cql
/*
Warning: Table system_views.sai_column_indexes is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.sai_column_indexes (
    keyspace_name text,
    index_name text,
    analyzer text,
    column_name text,
    is_building boolean,
    is_queryable boolean,
    is_string boolean,
    table_name text,
    PRIMARY KEY (keyspace_name, index_name)
) WITH CLUSTERING ORDER BY (index_name ASC)
    AND comment = 'Storage-attached column index metadata';
*/
```
`system_views | table | sai_sstable_index_segments`

```cql
/*
Warning: Table system_views.sai_sstable_index_segments is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.sai_sstable_index_segments (
    keyspace_name text,
    index_name text,
    sstable_name text,
    segment_row_id_offset bigint,
    cell_count bigint,
    column_name text,
    component_metadata frozen<map<text, frozen<map<text, text>>>>,
    end_token text,
    max_sstable_row_id bigint,
    max_term text,
    min_sstable_row_id bigint,
    min_term text,
    start_token text,
    table_name text,
    PRIMARY KEY (keyspace_name, index_name, sstable_name, segment_row_id_offset)
) WITH CLUSTERING ORDER BY (index_name ASC, sstable_name ASC, segment_row_id_offset ASC)
    AND comment = 'SSTable index segment metadata';
*/
```
`system_views | table | sai_sstable_indexes`

```cql
/*
Warning: Table system_views.sai_sstable_indexes is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.sai_sstable_indexes (
    keyspace_name text,
    index_name text,
    sstable_name text,
    cell_count bigint,
    column_name text,
    end_token text,
    format_version text,
    max_row_id bigint,
    min_row_id bigint,
    per_column_disk_size bigint,
    per_table_disk_size bigint,
    start_token text,
    table_name text,
    PRIMARY KEY (keyspace_name, index_name, sstable_name)
) WITH CLUSTERING ORDER BY (index_name ASC, sstable_name ASC)
    AND comment = 'SSTable index metadata';
*/
```
`system_views | table | settings`

```cql
/*
Warning: Table system_views.settings is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.settings (
    name text PRIMARY KEY,
    value text
) WITH comment = 'current settings';
*/
```
`system_views | table | snapshots`

```cql
/*
Warning: Table system_views.snapshots is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.snapshots (
    name text,
    keyspace_name text,
    table_name text,
    created_at timestamp,
    ephemeral boolean,
    expires_at timestamp,
    size_on_disk bigint,
    true_size bigint,
    PRIMARY KEY (name, keyspace_name, table_name)
) WITH CLUSTERING ORDER BY (keyspace_name ASC, table_name ASC)
    AND comment = 'available snapshots';
*/
```
`system_views | table | sstable_tasks`

```cql
/*
Warning: Table system_views.sstable_tasks is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.sstable_tasks (
    keyspace_name text,
    table_name text,
    task_id timeuuid,
    completion_ratio double,
    kind text,
    progress bigint,
    sstables int,
    target_directory text,
    total bigint,
    total_compressed bigint,
    unit text,
    PRIMARY KEY (keyspace_name, table_name, task_id)
) WITH CLUSTERING ORDER BY (table_name ASC, task_id ASC)
    AND comment = 'current sstable tasks';
*/
```
`system_views | table | streaming`

```cql
/*
Warning: Table system_views.streaming is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.streaming (
    id timeuuid PRIMARY KEY,
    bytes_received bigint,
    bytes_sent bigint,
    bytes_to_receive bigint,
    bytes_to_send bigint,
    duration_millis bigint,
    failure_cause text,
    files_received bigint,
    files_sent bigint,
    files_to_receive bigint,
    files_to_send bigint,
    follower boolean,
    last_updated_at timestamp,
    operation text,
    peers frozen<list<text>>,
    progress_percentage float,
    status text,
    status_failure_timestamp timestamp,
    status_init_timestamp timestamp,
    status_start_timestamp timestamp,
    status_success_timestamp timestamp,
    success_message text
) WITH comment = '';
*/
```
`system_views | table | system_logs`

```cql
/*
Warning: Table system_views.system_logs is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.system_logs (
    timestamp timestamp,
    order_in_millisecond int,
    level text,
    logger text,
    message text,
    PRIMARY KEY (timestamp, order_in_millisecond)
) WITH CLUSTERING ORDER BY (order_in_millisecond ASC)
    AND comment = 'Cassandra logs';
*/
```
`system_views | table | system_properties`

```cql
/*
Warning: Table system_views.system_properties is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.system_properties (
    name text PRIMARY KEY,
    value text
) WITH comment = 'Cassandra relevant system properties';
*/
```
`system_views | table | thread_pools`

```cql
/*
Warning: Table system_views.thread_pools is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.thread_pools (
    name text PRIMARY KEY,
    active_tasks int,
    active_tasks_limit int,
    blocked_tasks bigint,
    blocked_tasks_all_time bigint,
    completed_tasks bigint,
    pending_tasks int
) WITH comment = '';
*/
```
`system_views | table | tombstones_per_read`

```cql
/*
Warning: Table system_views.tombstones_per_read is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_views.tombstones_per_read (
    keyspace_name text,
    table_name text,
    count bigint,
    max double,
    p50th double,
    p99th double,
    PRIMARY KEY ((keyspace_name, table_name))
) WITH comment = '';
*/
```
`system_virtual_schema | keyspace | system_virtual_schema`

```cql
/*
Warning: Keyspace system_virtual_schema is a virtual keyspace and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL KEYSPACE system_virtual_schema;
*/
```
`system_virtual_schema | table | columns`

```cql
/*
Warning: Table system_virtual_schema.columns is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_virtual_schema.columns (
    keyspace_name text,
    table_name text,
    column_name text,
    clustering_order text,
    column_name_bytes blob,
    kind text,
    position int,
    type text,
    PRIMARY KEY (keyspace_name, table_name, column_name)
) WITH CLUSTERING ORDER BY (table_name ASC, column_name ASC)
    AND comment = 'virtual column definitions';
*/
```
`system_virtual_schema | table | keyspaces`

```cql
/*
Warning: Table system_virtual_schema.keyspaces is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_virtual_schema.keyspaces (
    keyspace_name text PRIMARY KEY
) WITH comment = 'virtual keyspace definitions';
*/
```
`system_virtual_schema | table | tables`

```cql
/*
Warning: Table system_virtual_schema.tables is a virtual table and cannot be recreated with CQL.
Structure, for reference:
VIRTUAL TABLE system_virtual_schema.tables (
    keyspace_name text,
    table_name text,
    comment text,
    PRIMARY KEY (keyspace_name, table_name)
) WITH CLUSTERING ORDER BY (table_name ASC)
    AND comment = 'virtual table definitions';
*/
```
## `DESC TABLES`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`

```
audit | table | audit_log
cql_ks | table | t
ref | table | "QuotedTable"
ref | table | t_all
ref | table | t_cdc
ref | table | t_comp2
ref | table | t_counter
ref | table | t_ics
ref | table | t_id
ref | table | t_opts
ref | table | t_pgs
ref | table | t_simple
ref | table | t_tgc
ref | table | t_twcs
ref | table | t_vec
ref_vnodes | table | base
system | table | "IndexInfo"
system | table | batchlog
system | table | batchlog_v2
system | table | built_views
system | table | cdc_generations_v3
system | table | cdc_local
system | table | cdc_streams
system | table | cdc_streams_history
system | table | cdc_streams_state
system | table | cdc_timestamps
system | table | client_routes
system | table | clients
system | table | cluster_status
system | table | commitlog_cleanups
system | table | compaction_history
system | table | compactions_in_progress
system | table | config
system | table | corrupt_data
system | table | dicts
system | table | discovery
system | table | group0_history
system | table | hints
system | table | large_cells
system | table | large_partitions
system | table | large_rows
system | table | load_per_node
system | table | local
system | table | paxos
system | table | peer_events
system | table | peers
system | table | protocol_servers
system | table | raft
system | table | raft_snapshot_config
system | table | raft_snapshots
system | table | raft_state
system | table | range_xfers
system | table | repair_history
system | table | repair_tasks
system | table | role_attributes
system | table | role_members
system | table | role_permissions
system | table | roles
system | table | runtime_info
system | table | scylla_local
system | table | scylla_table_schema_history
system | table | scylla_views_builds_in_progress
system | table | service_levels_v2
system | table | size_estimates
system | table | snapshots
system | table | sstable_activity
system | table | sstables
system | table | tablet_sizes
system | table | tablets
system | table | token_ring
system | table | topology
system | table | topology_requests
system | table | truncated
system | table | versions
system | table | view_build_status_v2
system | table | view_building_tasks
system | table | views_builds_in_progress
system_distributed | table | cdc_generation_timestamps
system_distributed | table | cdc_streams_descriptions_v2
system_distributed | table | snapshot_keyspaces
system_distributed | table | snapshot_nodes
system_distributed | table | snapshot_remote_locations
system_distributed | table | snapshot_sstables
system_distributed | table | snapshot_tables
system_distributed | table | snapshot_tablets
system_distributed | table | snapshots
system_distributed | table | view_build_status
system_replicated_keys | table | encrypted_keys
system_schema | table | aggregates
system_schema | table | columns
system_schema | table | computed_columns
system_schema | table | dropped_columns
system_schema | table | functions
system_schema | table | indexes
system_schema | table | keyspaces
system_schema | table | scylla_aggregates
system_schema | table | scylla_keyspaces
system_schema | table | scylla_tables
system_schema | table | tables
system_schema | table | triggers
system_schema | table | types
system_schema | table | view_virtual_columns
system_schema | table | views
system_traces | table | events
system_traces | table | node_slow_log
system_traces | table | node_slow_log_time_idx
system_traces | table | sessions
system_traces | table | sessions_time_idx
```
### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`

```
ref | table | "QuotedTable"
ref | table | t_all
ref | table | t_cdc2
ref | table | t_comp1
ref | table | t_comp3
ref | table | t_compact
ref | table | t_compact2
ref | table | t_counter
ref | table | t_id
ref | table | t_mt
ref | table | t_opts
ref | table | t_rr
ref | table | t_simple
ref | table | t_twcs
ref | table | t_ucs
ref | table | t_vec
ref_vnodes | table | base
system | table | available_ranges
system | table | available_ranges_v2
system | table | batches
system | table | built_views
system | table | compaction_history
system | table | "IndexInfo"
system | table | local
system | table | paxos
system | table | paxos_repair_history
system | table | peer_events
system | table | peer_events_v2
system | table | peers
system | table | peers_v2
system | table | prepared_statements
system | table | repairs
system | table | size_estimates
system | table | sstable_activity
system | table | sstable_activity_v2
system | table | table_estimates
system | table | top_partitions
system | table | transferred_ranges
system | table | transferred_ranges_v2
system | table | view_builds_in_progress
system_auth | table | cidr_groups
system_auth | table | cidr_permissions
system_auth | table | identity_to_role
system_auth | table | network_permissions
system_auth | table | resource_role_permissons_index
system_auth | table | role_members
system_auth | table | role_permissions
system_auth | table | roles
system_distributed | table | parent_repair_history
system_distributed | table | partition_denylist
system_distributed | table | repair_history
system_distributed | table | view_build_status
system_schema | table | aggregates
system_schema | table | column_masks
system_schema | table | columns
system_schema | table | dropped_columns
system_schema | table | functions
system_schema | table | indexes
system_schema | table | keyspaces
system_schema | table | tables
system_schema | table | triggers
system_schema | table | types
system_schema | table | views
system_traces | table | events
system_traces | table | sessions
system_views | table | batch_metrics
system_views | table | caches
system_views | table | cidr_filtering_metrics_counts
system_views | table | cidr_filtering_metrics_latencies
system_views | table | clients
system_views | table | coordinator_read_latency
system_views | table | coordinator_scan_latency
system_views | table | coordinator_write_latency
system_views | table | cql_metrics
system_views | table | credentials_cache_keys
system_views | table | disk_usage
system_views | table | gossip_info
system_views | table | internode_inbound
system_views | table | internode_outbound
system_views | table | jmx_permissions_cache_keys
system_views | table | local_read_latency
system_views | table | local_scan_latency
system_views | table | local_write_latency
system_views | table | max_partition_size
system_views | table | max_sstable_duration
system_views | table | max_sstable_size
system_views | table | network_permissions_cache_keys
system_views | table | pending_hints
system_views | table | permissions_cache_keys
system_views | table | queries
system_views | table | repair_jobs
system_views | table | repair_participates
system_views | table | repair_sessions
system_views | table | repair_validations
system_views | table | repairs
system_views | table | roles_cache_keys
system_views | table | rows_per_read
system_views | table | sai_column_indexes
system_views | table | sai_sstable_index_segments
system_views | table | sai_sstable_indexes
system_views | table | settings
system_views | table | snapshots
system_views | table | sstable_tasks
system_views | table | streaming
system_views | table | system_logs
system_views | table | system_properties
system_views | table | thread_pools
system_views | table | tombstones_per_read
system_virtual_schema | table | columns
system_virtual_schema | table | keyspaces
system_virtual_schema | table | tables
```
## `DESCRIBE TABLE system.local`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`system | table | local` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`system | table | local` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

## `DESCRIBE TABLE system.peers`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`system | table | peers` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`system | table | peers` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

## `DESC KEYSPACE system_auth`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`system_auth | keyspace | system_auth` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`system_auth | keyspace | system_auth` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system_auth | table | cidr_groups` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system_auth | table | cidr_permissions` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system_auth | table | identity_to_role` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system_auth | table | network_permissions` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system_auth | table | resource_role_permissons_index` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system_auth | table | role_members` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system_auth | table | role_permissions` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system_auth | table | roles` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

## `DESCRIBE KEYSPACE system`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`system | keyspace | system` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | type | sstableinfo` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | type | tablet_task_info` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | type | repair_scheduler_config` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | "IndexInfo"` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | batchlog` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | batchlog_v2` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | built_views` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | cdc_generations_v3` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | cdc_local` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | cdc_streams` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | cdc_streams_history` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | cdc_streams_state` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | cdc_timestamps` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | client_routes` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | clients` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | cluster_status` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | commitlog_cleanups` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | compaction_history` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | compactions_in_progress` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | config` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | corrupt_data` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | dicts` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | discovery` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | group0_history` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | hints` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | large_cells` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | large_partitions` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | large_rows` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | load_per_node` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | local` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | paxos` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | peer_events` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | peers` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | protocol_servers` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | raft` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | raft_snapshot_config` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | raft_snapshots` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | raft_state` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | range_xfers` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | repair_history` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | repair_tasks` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | role_attributes` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | role_members` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | role_permissions` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | roles` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | runtime_info` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | scylla_local` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | scylla_table_schema_history` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | scylla_views_builds_in_progress` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | service_levels_v2` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | size_estimates` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | snapshots` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | sstable_activity` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | sstables` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | tablet_sizes` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | tablets` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | token_ring` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | topology` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | topology_requests` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | truncated` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | versions` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | view_build_status_v2` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | view_building_tasks` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | views_builds_in_progress` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`system | keyspace | system` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | available_ranges` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | available_ranges_v2` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | batches` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | built_views` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | compaction_history` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | "IndexInfo"` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | local` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | paxos` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | index | "PaxosUncommittedIndex"` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | paxos_repair_history` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | peer_events` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | peer_events_v2` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | peers` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | peers_v2` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | prepared_statements` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | repairs` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | size_estimates` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | sstable_activity` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | sstable_activity_v2` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | table_estimates` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | top_partitions` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | transferred_ranges` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | transferred_ranges_v2` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

`system | table | view_builds_in_progress` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`

## `DESCRIBE TABLE system_views.settings`

### ScyllaDB 2026.3.3

`0x2200` 'system_views' not found in keyspaces

### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`system_views | table | settings` — create_statement identical to the one under `DESCRIBE FULL SCHEMA`
