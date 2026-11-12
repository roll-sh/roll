<!-- Generated from our own observations of the reference images (see conformance.md for the capture method). Do not edit by hand. -->

# Server-side DESCRIBE

Raw result rows of server-side `DESCRIBE` statements sent through the Python driver (protocol v4 on ScyllaDB, v5 on Cassandra) for the sample schema in [system-schema.md](system-schema.md). Each row is shown as `keyspace_name | type | name` followed by `create_statement` verbatim in a fenced block (whitespace is exact). Statements over system keyspaces are in [describe-system.md](describe-system.md).

## `DESCRIBE KEYSPACE ref`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | keyspace | ref`

```cql
CREATE KEYSPACE ref WITH replication = {'class': 'org.apache.cassandra.locator.NetworkTopologyStrategy', 'datacenter1': '1'} AND durable_writes = true AND tablets = {'enabled': true};
```
`ref | type | address`

```cql
CREATE TYPE ref.address (
    street text,
    zip int,
    tags frozen<set<text>>
);
```
`ref | type | person`

```cql
CREATE TYPE ref.person (
    name text,
    home frozen<address>,
    "Quoted Field" int
);
```
`ref | table | "QuotedTable"`

```cql
CREATE TABLE ref."QuotedTable" (
    "Key" int,
    "Clu Col" text,
    "camelCase" text,
    "select" int,
    PRIMARY KEY ("Key", "Clu Col")
) WITH CLUSTERING ORDER BY ("Clu Col" ASC)
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
`ref | index | "Quoted Idx"`

```cql
CREATE INDEX "Quoted Idx" ON ref."QuotedTable"("camelCase") WITH bloom_filter_fp_chance = 0.01
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
`ref | table | t_all`

```cql
CREATE TABLE ref.t_all (
    a int,
    b text,
    c1 timestamp,
    c2 uuid,
    s text STATIC,
    addr address,
    addrs list<frozen<address>>,
    asc_ ascii,
    big bigint,
    bl blob,
    bo boolean,
    da date,
    de decimal,
    do double,
    du duration,
    faddr frozen<address>,
    fl float,
    fl_l frozen<list<int>>,
    fl_m frozen<map<text, int>>,
    fl_s frozen<set<text>>,
    ftup frozen<tuple<int, frozen<tuple<text, bigint>>>>,
    ine inet,
    l list<int>,
    m map<text, int>,
    nested map<text, frozen<list<int>>>,
    sm smallint,
    st set<text>,
    ti time,
    tin tinyint,
    tu timeuuid,
    tup frozen<tuple<int, text, float>>,
    txt text,
    vc text,
    vi varint,
    who frozen<person>,
    PRIMARY KEY ((a, b), c1, c2)
) WITH CLUSTERING ORDER BY (c1 DESC, c2 ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'sample table'
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
`ref | index | t_all_b_idx`

```cql
CREATE INDEX t_all_b_idx ON ref.t_all(b) WITH bloom_filter_fp_chance = 0.01
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
`ref | index | t_all_c2_idx`

```cql
CREATE INDEX t_all_c2_idx ON ref.t_all(c2) WITH bloom_filter_fp_chance = 0.01
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
`ref | index | t_all_fl_l_idx`

```cql
CREATE INDEX t_all_fl_l_idx ON ref.t_all(full(fl_l)) WITH bloom_filter_fp_chance = 0.01
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
`ref | index | t_all_local_idx`

```cql
CREATE INDEX t_all_local_idx ON ref.t_all((a, b), vi) WITH bloom_filter_fp_chance = 0.01
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
    AND scylla_tags = {'system:synchronous_view_updates': 'true'}
    AND tombstone_gc = {'mode': 'timeout', 'propagation_delay_in_seconds': '3600'}
    AND synchronous_updates = true;

```
`ref | index | t_all_m_entries_idx`

```cql
CREATE INDEX t_all_m_entries_idx ON ref.t_all(entries(m)) WITH bloom_filter_fp_chance = 0.01
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
`ref | index | t_all_m_keys_idx`

```cql
CREATE INDEX t_all_m_keys_idx ON ref.t_all(keys(m)) WITH bloom_filter_fp_chance = 0.01
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
`ref | index | t_all_s_idx`

```cql
CREATE INDEX t_all_s_idx ON ref.t_all(s) WITH bloom_filter_fp_chance = 0.01
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
`ref | index | t_all_st_idx`

```cql
CREATE INDEX t_all_st_idx ON ref.t_all(values(st)) WITH bloom_filter_fp_chance = 0.01
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
`ref | index | t_all_txt_idx`

```cql
CREATE INDEX t_all_txt_idx ON ref.t_all(txt) WITH bloom_filter_fp_chance = 0.01
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
`ref | view | t_all_by_txt`

```cql
CREATE MATERIALIZED VIEW ref.t_all_by_txt AS
    SELECT txt, a, b, c1, c2
    FROM ref.t_all
    WHERE txt IS NOT null AND a IS NOT null AND b IS NOT null AND c1 IS NOT null AND c2 IS NOT null
    PRIMARY KEY (txt, a, b, c1, c2)
    WITH CLUSTERING ORDER BY (a DESC, b ASC, c1 ASC, c2 DESC)
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
`ref | table | t_cdc`

```cql
CREATE TABLE ref.t_cdc (
    id int,
    v int,
    PRIMARY KEY (id)
) WITH bloom_filter_fp_chance = 0.01
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
    AND cdc = {'delta': 'full', 'enabled': 'true', 'postimage': 'false', 'preimage': 'false', 'ttl': '86400'}
    AND tombstone_gc = {'mode': 'repair', 'propagation_delay_in_seconds': '3600'};

```
`ref | table | t_cdc_scylla_cdc_log`

```cql
ALTER TABLE ref.t_cdc_scylla_cdc_log WITH bloom_filter_fp_chance = 0.01
    AND caching = {'enabled': 'false', 'keys': 'NONE', 'rows_per_partition': 'NONE'}
    AND comment = 'CDC log for ref.t_cdc'
    AND compaction = {'class': 'TimeWindowCompactionStrategy', 'compaction_window_size': '60', 'compaction_window_unit': 'MINUTES', 'expired_sstable_check_frequency_seconds': '1800'}
    AND compression = {'sstable_compression': 'LZ4WithDictsCompressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 0
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE'
    AND tombstone_gc = {'mode': 'timeout', 'propagation_delay_in_seconds': '3600'};

```
`ref | table | t_comp2`

```cql
CREATE TABLE ref.t_comp2 (
    id int,
    PRIMARY KEY (id)
) WITH bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'chunk_length_in_kb': '64', 'sstable_compression': 'org.apache.cassandra.io.compress.DeflateCompressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE'
    AND tombstone_gc = {'mode': 'repair', 'propagation_delay_in_seconds': '3600'};

```
`ref | table | t_counter`

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
`ref | table | t_ics`

```cql
CREATE TABLE ref.t_ics (
    id int,
    PRIMARY KEY (id)
) WITH bloom_filter_fp_chance = 0.01
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
`ref | table | t_id`

```cql
CREATE TABLE ref.t_id (
    id int,
    PRIMARY KEY (id)
) WITH bloom_filter_fp_chance = 0.01
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
`ref | table | t_opts`

```cql
CREATE TABLE ref.t_opts (
    id int,
    v text,
    PRIMARY KEY (id)
) WITH bloom_filter_fp_chance = 0.1
    AND caching = {'keys': 'ALL', 'rows_per_partition': '10'}
    AND comment = ''
    AND compaction = {'class': 'LeveledCompactionStrategy', 'sstable_size_in_mb': '64'}
    AND compression = {'sstable_compression': 'LZ4WithDictsCompressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 3600
    AND gc_grace_seconds = 3600
    AND max_index_interval = 1024
    AND memtable_flush_period_in_ms = 60000
    AND min_index_interval = 64
    AND speculative_retry = '99.0PERCENTILE'
    AND tombstone_gc = {'mode': 'repair', 'propagation_delay_in_seconds': '3600'};

```
`ref | table | t_pgs`

```cql
CREATE TABLE ref.t_pgs (
    id int,
    PRIMARY KEY (id)
) WITH bloom_filter_fp_chance = 0.01
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
    AND paxos_grace_seconds = 100
    AND tombstone_gc = {'mode': 'repair', 'propagation_delay_in_seconds': '3600'};

```
`ref | table | t_simple`

```cql
CREATE TABLE ref.t_simple (
    id int,
    v text,
    PRIMARY KEY (id)
) WITH bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'altered'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'LZ4WithDictsCompressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 7200
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE'
    AND tombstone_gc = {'mode': 'repair', 'propagation_delay_in_seconds': '3600'};

```
`ref | view | t_simple_by_v`

```cql
CREATE MATERIALIZED VIEW ref.t_simple_by_v AS
    SELECT v, id
    FROM ref.t_simple
    WHERE v IS NOT null AND id IS NOT null
    PRIMARY KEY (v, id)
    WITH CLUSTERING ORDER BY (id ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'mv'
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
`ref | table | t_tgc`

```cql
CREATE TABLE ref.t_tgc (
    id int,
    PRIMARY KEY (id)
) WITH bloom_filter_fp_chance = 0.01
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
    AND tombstone_gc = {'mode': 'timeout', 'propagation_delay_in_seconds': '3600'};

```
`ref | table | t_twcs`

```cql
CREATE TABLE ref.t_twcs (
    id int,
    at timestamp,
    v text,
    PRIMARY KEY (id, at)
) WITH CLUSTERING ORDER BY (at ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'TimeWindowCompactionStrategy', 'compaction_window_size': '6', 'compaction_window_unit': 'HOURS'}
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
`ref | table | t_vec`

```cql
CREATE TABLE ref.t_vec (
    id int,
    emb vector<float, 3>,
    tags vector<text, 2>,
    PRIMARY KEY (id)
) WITH bloom_filter_fp_chance = 0.01
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
`ref | index | t_vec_emb_idx`

```cql
CREATE CUSTOM INDEX t_vec_emb_idx ON ref.t_vec(emb) USING 'vector_index' WITH OPTIONS = {'similarity_function': 'COSINE'}
```
`ref | index | t_vec_emb_vs`

```cql
CREATE CUSTOM INDEX t_vec_emb_vs ON ref.t_vec(emb) USING 'vector_index'
```
`ref | table | t_vec_scylla_cdc_log`

```cql
ALTER TABLE ref.t_vec_scylla_cdc_log WITH bloom_filter_fp_chance = 0.01
    AND caching = {'enabled': 'false', 'keys': 'NONE', 'rows_per_partition': 'NONE'}
    AND comment = 'CDC log for ref.t_vec'
    AND compaction = {'class': 'TimeWindowCompactionStrategy', 'compaction_window_size': '60', 'compaction_window_unit': 'MINUTES', 'expired_sstable_check_frequency_seconds': '1800'}
    AND compression = {'sstable_compression': 'LZ4WithDictsCompressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 0
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE'
    AND tombstone_gc = {'mode': 'timeout', 'propagation_delay_in_seconds': '3600'};

```
### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | keyspace | ref`

```cql
CREATE KEYSPACE ref WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': '1'}  AND durable_writes = true;
```
`ref | type | address`

```cql
CREATE TYPE ref.address (
    street text,
    zip int,
    tags frozen<set<text>>
);
```
`ref | type | person`

```cql
CREATE TYPE ref.person (
    name text,
    home frozen<address>,
    "Quoted Field" int
);
```
`ref | table | "QuotedTable"`

```cql
CREATE TABLE ref."QuotedTable" (
    "Key" int,
    "Clu Col" text,
    "camelCase" text,
    "select" int,
    PRIMARY KEY ("Key", "Clu Col")
) WITH CLUSTERING ORDER BY ("Clu Col" ASC)
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
`ref | index | "QuotedIdx"`

```cql
CREATE INDEX "QuotedIdx" ON ref."QuotedTable" ("camelCase");
```
`ref | table | t_all`

```cql
CREATE TABLE ref.t_all (
    a int,
    b text,
    c1 timestamp,
    c2 uuid,
    s text static,
    asc_ ascii,
    big bigint,
    bl blob,
    bo boolean,
    da date,
    de decimal,
    do double,
    du duration,
    faddr frozen<address>,
    fl float,
    fl_l frozen<list<int>>,
    fl_m frozen<map<text, int>>,
    fl_s frozen<set<text>>,
    ftup frozen<tuple<int, frozen<tuple<text, bigint>>>>,
    ine inet,
    sm smallint,
    ti time,
    tin tinyint,
    tu timeuuid,
    tup frozen<tuple<int, text, float>>,
    txt text,
    vc text,
    vi varint,
    who frozen<person>,
    addr address,
    addrs list<frozen<address>>,
    l list<int>,
    m map<text, int>,
    nested map<text, frozen<list<int>>>,
    st set<text>,
    PRIMARY KEY ((a, b), c1, c2)
) WITH CLUSTERING ORDER BY (c1 DESC, c2 ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'sample table'
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
`ref | index | t_all_b_idx`

```cql
CREATE INDEX t_all_b_idx ON ref.t_all (b);
```
`ref | index | t_all_c2_idx`

```cql
CREATE INDEX t_all_c2_idx ON ref.t_all (c2);
```
`ref | index | t_all_fl_l_idx`

```cql
CREATE INDEX t_all_fl_l_idx ON ref.t_all (full(fl_l));
```
`ref | index | t_all_m_entries_idx`

```cql
CREATE INDEX t_all_m_entries_idx ON ref.t_all (entries(m));
```
`ref | index | t_all_m_keys_idx`

```cql
CREATE INDEX t_all_m_keys_idx ON ref.t_all (keys(m));
```
`ref | index | t_all_s_idx`

```cql
CREATE INDEX t_all_s_idx ON ref.t_all (s);
```
`ref | index | t_all_st_idx`

```cql
CREATE INDEX t_all_st_idx ON ref.t_all (values(st));
```
`ref | index | t_all_txt_idx`

```cql
CREATE INDEX t_all_txt_idx ON ref.t_all (txt);
```
`ref | index | t_all_txt_sai`

```cql
CREATE CUSTOM INDEX t_all_txt_sai ON ref.t_all (vc) USING 'StorageAttachedIndex' WITH OPTIONS = {'case_sensitive': 'false', 'normalize': 'true'};
```
`ref | table | t_cdc2`

```cql
CREATE TABLE ref.t_cdc2 (
    id int PRIMARY KEY,
    v int
) WITH additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = true
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
`ref | table | t_comp1`

```cql
CREATE TABLE ref.t_comp1 (
    id int PRIMARY KEY
) WITH additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = ''
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.ZstdCompressor'}
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
`ref | table | t_comp3`

```cql
CREATE TABLE ref.t_comp3 (
    id int PRIMARY KEY
) WITH additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = ''
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'enabled': 'false'}
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
`ref | table | t_compact`

```cql
/*
Warning: Table ref.t_compact omitted because it has constructs not compatible with CQL (was created via legacy API).
Approximate structure, for reference:
(this should not be used to reproduce this schema)

CREATE TABLE ref.t_compact (
    id int,
    v int,
    PRIMARY KEY (id)
) WITH COMPACT STORAGE
    AND CLUSTERING ORDER BY (column1 ASC)
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
*/
```
`ref | table | t_compact2`

```cql
/*
Warning: Table ref.t_compact2 omitted because it has constructs not compatible with CQL (was created via legacy API).
Approximate structure, for reference:
(this should not be used to reproduce this schema)

CREATE TABLE ref.t_compact2 (
    id int,
    ck int,
    v int,
    PRIMARY KEY (id, ck)
) WITH COMPACT STORAGE
    AND CLUSTERING ORDER BY (ck ASC)
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
*/
```
`ref | table | t_counter`

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
`ref | table | t_id`

```cql
CREATE TABLE ref.t_id (
    id int PRIMARY KEY
) WITH additional_write_policy = '99p'
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
`ref | table | t_mt`

```cql
CREATE TABLE ref.t_mt (
    id int PRIMARY KEY
) WITH additional_write_policy = '99p'
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
`ref | table | t_opts`

```cql
CREATE TABLE ref.t_opts (
    id int PRIMARY KEY,
    v text
) WITH additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.1
    AND caching = {'keys': 'ALL', 'rows_per_partition': '10'}
    AND cdc = false
    AND comment = ''
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.LeveledCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4', 'sstable_size_in_mb': '64'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 0.5
    AND default_time_to_live = 3600
    AND extensions = {}
    AND gc_grace_seconds = 3600
    AND incremental_backups = true
    AND max_index_interval = 1024
    AND memtable_flush_period_in_ms = 60000
    AND min_index_interval = 64
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`ref | table | t_rr`

```cql
CREATE TABLE ref.t_rr (
    id int PRIMARY KEY
) WITH additional_write_policy = '90p'
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
    AND read_repair = 'NONE'
    AND speculative_retry = '99p';
```
`ref | table | t_simple`

```cql
CREATE TABLE ref.t_simple (
    id int PRIMARY KEY,
    v text
) WITH additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'altered'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 7200
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`ref | index | t_simple_v_sai`

```cql
CREATE CUSTOM INDEX t_simple_v_sai ON ref.t_simple (v) USING 'sai';
```
`ref | table | t_twcs`

```cql
CREATE TABLE ref.t_twcs (
    id int,
    at timestamp,
    v text,
    PRIMARY KEY (id, at)
) WITH CLUSTERING ORDER BY (at ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = ''
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.TimeWindowCompactionStrategy', 'compaction_window_size': '6', 'compaction_window_unit': 'HOURS', 'max_threshold': '32', 'min_threshold': '4'}
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
`ref | table | t_ucs`

```cql
CREATE TABLE ref.t_ucs (
    id int PRIMARY KEY
) WITH additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = ''
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.UnifiedCompactionStrategy'}
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
`ref | table | t_vec`

```cql
CREATE TABLE ref.t_vec (
    id int PRIMARY KEY,
    emb vector<float, 3>,
    tags vector<text, 2>
) WITH additional_write_policy = '99p'
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
`ref | index | t_vec_emb_idx`

```cql
CREATE CUSTOM INDEX t_vec_emb_idx ON ref.t_vec (emb) USING 'sai' WITH OPTIONS = {'similarity_function': 'COSINE'};
```
## `DESCRIBE KEYSPACE ref_simple`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref_simple | keyspace | ref_simple`

```cql
CREATE KEYSPACE ref_simple WITH replication = {'class': 'org.apache.cassandra.locator.SimpleStrategy', 'replication_factor': '1'} AND durable_writes = false AND tablets = {'enabled': false};
```
### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref_simple | keyspace | ref_simple`

```cql
CREATE KEYSPACE ref_simple WITH replication = {'class': 'SimpleStrategy', 'replication_factor': '1'}  AND durable_writes = false;
```
## `DESCRIBE KEYSPACE ref_vnodes`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref_vnodes | keyspace | ref_vnodes`

```cql
CREATE KEYSPACE ref_vnodes WITH replication = {'class': 'org.apache.cassandra.locator.NetworkTopologyStrategy', 'datacenter1': '1'} AND durable_writes = true AND tablets = {'enabled': false};
```
`ref_vnodes | table | base`

```cql
CREATE TABLE ref_vnodes.base (
    id int,
    v text,
    PRIMARY KEY (id)
) WITH bloom_filter_fp_chance = 0.01
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
`ref_vnodes | index | base_v_idx`

```cql
CREATE INDEX base_v_idx ON ref_vnodes.base(v) WITH bloom_filter_fp_chance = 0.01
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
`ref_vnodes | view | base_by_v`

```cql
CREATE MATERIALIZED VIEW ref_vnodes.base_by_v AS
    SELECT *
    FROM ref_vnodes.base
    WHERE v IS NOT null AND id IS NOT null
    PRIMARY KEY (v, id)
    WITH CLUSTERING ORDER BY (id ASC)
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
### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref_vnodes | keyspace | ref_vnodes`

```cql
CREATE KEYSPACE ref_vnodes WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': '1'}  AND durable_writes = true;
```
`ref_vnodes | table | base`

```cql
CREATE TABLE ref_vnodes.base (
    id int PRIMARY KEY,
    v text
) WITH additional_write_policy = '99p'
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
`ref_vnodes | index | base_v_idx`

```cql
CREATE INDEX base_v_idx ON ref_vnodes.base (v);
```
## `DESCRIBE TABLE ref.t_all`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

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

### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

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

## `DESCRIBE TABLE ref.t_simple`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | table | t_simple` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | view | t_simple_by_v` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | table | t_simple` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_simple_v_sai` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

## `DESCRIBE TABLE ref.t_counter`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | table | t_counter` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | table | t_counter` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

## `DESCRIBE TABLE ref.t_compact`

### ScyllaDB 2026.3.3

`0x2200` Table 't_compact' not found in keyspace 'ref'

### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | table | t_compact` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

## `DESCRIBE TABLE ref.t_compact2`

### ScyllaDB 2026.3.3

`0x2200` Table 't_compact2' not found in keyspace 'ref'

### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | table | t_compact2` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

## `DESCRIBE TABLE ref.t_opts`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | table | t_opts` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | table | t_opts` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

## `DESCRIBE TABLE ref.t_vec`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | table | t_vec` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_vec_emb_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_vec_emb_vs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_vec_scylla_cdc_log` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | table | t_vec` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_vec_emb_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

## `DESCRIBE TABLE ref.t_cdc`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | table | t_cdc` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_cdc_scylla_cdc_log` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

### Cassandra 5.0.9

`0x2200` Table 't_cdc' not found in keyspace 'ref'

## `DESCRIBE TABLE ref."QuotedTable"`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | table | "QuotedTable"` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | "Quoted Idx"` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | table | "QuotedTable"` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | "QuotedIdx"` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

## `DESCRIBE TABLE ref.t_mask`

### ScyllaDB 2026.3.3

`0x2200` Table 't_mask' not found in keyspace 'ref'

### Cassandra 5.0.9

`0x2200` Table 't_mask' not found in keyspace 'ref'

## `DESCRIBE TABLE ref.t_twcs`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | table | t_twcs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | table | t_twcs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

## `DESCRIBE TABLE ref.t_comp1`

### ScyllaDB 2026.3.3

`0x2200` Table 't_comp1' not found in keyspace 'ref'

### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | table | t_comp1` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

## `DESCRIBE TABLE ref.t_comp2`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | table | t_comp2` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

### Cassandra 5.0.9

`0x2200` Table 't_comp2' not found in keyspace 'ref'

## `DESCRIBE TABLE ref.t_comp3`

### ScyllaDB 2026.3.3

`0x2200` Table 't_comp3' not found in keyspace 'ref'

### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | table | t_comp3` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

## `DESCRIBE SCHEMA`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`audit | keyspace | audit`

```cql
CREATE KEYSPACE audit WITH replication = {'class': 'org.apache.cassandra.locator.NetworkTopologyStrategy', 'datacenter1': '3'} AND durable_writes = true AND tablets = {'enabled': false};
```
`audit | table | audit_log`

```cql
CREATE TABLE audit.audit_log (
    date timestamp,
    node inet,
    event_time timeuuid,
    category text,
    consistency text,
    error boolean,
    keyspace_name text,
    operation text,
    source inet,
    table_name text,
    username text,
    PRIMARY KEY ((date, node), event_time)
) WITH CLUSTERING ORDER BY (event_time ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'enabled': 'false', 'keys': 'NONE', 'rows_per_partition': 'NONE'}
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
`cql_ks | keyspace | cql_ks`

```cql
CREATE KEYSPACE cql_ks WITH replication = {'class': 'org.apache.cassandra.locator.NetworkTopologyStrategy', 'datacenter1': '1'} AND durable_writes = true AND tablets = {'enabled': true};
```
`cql_ks | table | t`

```cql
CREATE TABLE cql_ks.t (
    k int,
    c int,
    l list<int>,
    m map<text, int>,
    s set<int>,
    v int,
    PRIMARY KEY (k, c)
) WITH CLUSTERING ORDER BY (c ASC)
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

## `DESCRIBE CLUSTER`

### ScyllaDB 2026.3.3

Result columns: `cluster varchar`, `partitioner varchar`, `snitch varchar`

```
Test Cluster | org.apache.cassandra.dht.Murmur3Partitioner | org.apache.cassandra.locator.SimpleSnitch
```
### Cassandra 5.0.9

Result columns: `cluster varchar`, `partitioner varchar`, `snitch varchar`

```
Test Cluster | Murmur3Partitioner | DynamicEndpointSnitch
```
## `DESCRIBE KEYSPACES`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`

```
audit | keyspace | audit
cql_ks | keyspace | cql_ks
ref | keyspace | ref
ref_simple | keyspace | ref_simple
ref_vnodes | keyspace | ref_vnodes
system | keyspace | system
system_auth | keyspace | system_auth
system_distributed | keyspace | system_distributed
system_replicated_keys | keyspace | system_replicated_keys
system_schema | keyspace | system_schema
system_traces | keyspace | system_traces
```
### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`

```
ref | keyspace | ref
ref_simple | keyspace | ref_simple
ref_vnodes | keyspace | ref_vnodes
system | keyspace | system
system_auth | keyspace | system_auth
system_distributed | keyspace | system_distributed
system_schema | keyspace | system_schema
system_traces | keyspace | system_traces
system_views | keyspace | system_views
system_virtual_schema | keyspace | system_virtual_schema
```
## `DESC TYPES`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`

```
ref | type | address
ref | type | person
system | type | sstableinfo
system | type | tablet_task_info
system | type | repair_scheduler_config
```
### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`

```
ref | type | address
ref | type | person
```
## `DESC TYPE ref.address`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | type | address` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | type | address` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

## `DESC TYPE ref.person`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | type | person` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | type | person` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

## `DESC FUNCTIONS`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`

```

```
### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`

```

```
## `DESC AGGREGATES`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`

```

```
### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`

```

```
## `DESC INDEX ref.t_all_txt_idx`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | index | t_all_txt_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | index | t_all_txt_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

## `DESC INDEX ref.t_all_local_idx`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | index | t_all_local_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

### Cassandra 5.0.9

`0x2200` Table for existing index 't_all_local_idx' not found in 'ref'

## `DESC MATERIALIZED VIEW ref.t_simple_by_v`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | view | t_simple_by_v` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

### Cassandra 5.0.9

`0x2200` Materialized view 't_simple_by_v' not found in 'ref'

## `DESC MATERIALIZED VIEW ref.t_all_by_txt`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | view | t_all_by_txt` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

### Cassandra 5.0.9

`0x2200` Materialized view 't_all_by_txt' not found in 'ref'

## `DESC MATERIALIZED VIEW ref_vnodes.base_by_v`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref_vnodes | view | base_by_v` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes`

### Cassandra 5.0.9

`0x2200` Materialized view 'base_by_v' not found in 'ref_vnodes'

## `DESCRIBE TABLE ref.t_all WITH INTERNALS`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | table | t_all`

```cql
CREATE TABLE ref.t_all (
    a int,
    b text,
    c1 timestamp,
    c2 uuid,
    s text STATIC,
    addr address,
    addrs list<frozen<address>>,
    asc_ ascii,
    big bigint,
    bl blob,
    bo boolean,
    da date,
    de decimal,
    do double,
    du duration,
    faddr frozen<address>,
    fl float,
    fl_l frozen<list<int>>,
    fl_m frozen<map<text, int>>,
    fl_s frozen<set<text>>,
    ftup frozen<tuple<int, frozen<tuple<text, bigint>>>>,
    ine inet,
    l list<int>,
    m map<text, int>,
    nested map<text, frozen<list<int>>>,
    sm smallint,
    st set<text>,
    ti time,
    tin tinyint,
    tu timeuuid,
    tup frozen<tuple<int, text, float>>,
    txt text,
    vc text,
    vi varint,
    who frozen<person>,
    PRIMARY KEY ((a, b), c1, c2)
) WITH ID = 16ac6620-ccb5-11f1-974f-a4b4b432c093
AND CLUSTERING ORDER BY (c1 DESC, c2 ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'sample table'
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
`ref | index | t_all_b_idx`

```cql
CREATE INDEX t_all_b_idx ON ref.t_all(b) WITH ID = 179f2a91-ccb5-11f1-974f-a4b4b432c093
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
`ref | index | t_all_c2_idx`

```cql
CREATE INDEX t_all_c2_idx ON ref.t_all(c2) WITH ID = 17aebaf0-ccb5-11f1-974f-a4b4b432c093
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
`ref | index | t_all_fl_l_idx`

```cql
CREATE INDEX t_all_fl_l_idx ON ref.t_all(full(fl_l)) WITH ID = 17858810-ccb5-11f1-974f-a4b4b432c093
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
`ref | index | t_all_local_idx`

```cql
CREATE INDEX t_all_local_idx ON ref.t_all((a, b), vi) WITH ID = 17c2b820-ccb5-11f1-974f-a4b4b432c093
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
    AND scylla_tags = {'system:synchronous_view_updates': 'true'}
    AND tombstone_gc = {'mode': 'timeout', 'propagation_delay_in_seconds': '3600'}
    AND synchronous_updates = true;

```
`ref | index | t_all_m_entries_idx`

```cql
CREATE INDEX t_all_m_entries_idx ON ref.t_all(entries(m)) WITH ID = 177a1660-ccb5-11f1-974f-a4b4b432c093
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
`ref | index | t_all_m_keys_idx`

```cql
CREATE INDEX t_all_m_keys_idx ON ref.t_all(keys(m)) WITH ID = 1758abb0-ccb5-11f1-974f-a4b4b432c093
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
`ref | index | t_all_s_idx`

```cql
CREATE INDEX t_all_s_idx ON ref.t_all(s) WITH ID = 17951870-ccb5-11f1-974f-a4b4b432c093
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
`ref | index | t_all_st_idx`

```cql
CREATE INDEX t_all_st_idx ON ref.t_all(values(st)) WITH ID = 17694d80-ccb5-11f1-974f-a4b4b432c093
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
`ref | index | t_all_txt_idx`

```cql
CREATE INDEX t_all_txt_idx ON ref.t_all(txt) WITH ID = 17491b50-ccb5-11f1-974f-a4b4b432c093
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
`ref | view | t_all_by_txt`

```cql
CREATE MATERIALIZED VIEW ref.t_all_by_txt AS
    SELECT txt, a, b, c1, c2
    FROM ref.t_all
    WHERE txt IS NOT null AND a IS NOT null AND b IS NOT null AND c1 IS NOT null AND c2 IS NOT null
    PRIMARY KEY (txt, a, b, c1, c2)
    WITH ID = 1803dfd0-ccb5-11f1-974f-a4b4b432c093
AND CLUSTERING ORDER BY (a DESC, b ASC, c1 ASC, c2 DESC)
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
### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | table | t_all`

```cql
CREATE TABLE ref.t_all (
    a int,
    b text,
    c1 timestamp,
    c2 uuid,
    s text static,
    asc_ ascii,
    big bigint,
    bl blob,
    bo boolean,
    da date,
    de decimal,
    do double,
    du duration,
    faddr frozen<address>,
    fl float,
    fl_l frozen<list<int>>,
    fl_m frozen<map<text, int>>,
    fl_s frozen<set<text>>,
    ftup frozen<tuple<int, frozen<tuple<text, bigint>>>>,
    ine inet,
    sm smallint,
    ti time,
    tin tinyint,
    tu timeuuid,
    tup frozen<tuple<int, text, float>>,
    txt text,
    vc text,
    vi varint,
    who frozen<person>,
    addr address,
    addrs list<frozen<address>>,
    l list<int>,
    m map<text, int>,
    nested map<text, frozen<list<int>>>,
    st set<text>,
    PRIMARY KEY ((a, b), c1, c2)
) WITH ID = 5ff8be00-ccb5-11f1-8c36-33d044eb584e
    AND CLUSTERING ORDER BY (c1 DESC, c2 ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'sample table'
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
`ref | index | t_all_b_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_c2_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_fl_l_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_m_entries_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_m_keys_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_s_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_st_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_txt_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_txt_sai` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

## `DESCRIBE KEYSPACE ref WITH INTERNALS`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | keyspace | ref` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | type | address` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | type | person` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | "QuotedTable"`

```cql
CREATE TABLE ref."QuotedTable" (
    "Key" int,
    "Clu Col" text,
    "camelCase" text,
    "select" int,
    PRIMARY KEY ("Key", "Clu Col")
) WITH ID = 16b084d0-ccb5-11f1-974f-a4b4b432c093
AND CLUSTERING ORDER BY ("Clu Col" ASC)
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
`ref | index | "Quoted Idx"`

```cql
CREATE INDEX "Quoted Idx" ON ref."QuotedTable"("camelCase") WITH ID = 17d79fb1-ccb5-11f1-974f-a4b4b432c093
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
`ref | table | t_all` — create_statement identical to the one under `DESCRIBE TABLE ref.t_all WITH INTERNALS`

`ref | index | t_all_b_idx` — create_statement identical to the one under `DESCRIBE TABLE ref.t_all WITH INTERNALS`

`ref | index | t_all_c2_idx` — create_statement identical to the one under `DESCRIBE TABLE ref.t_all WITH INTERNALS`

`ref | index | t_all_fl_l_idx` — create_statement identical to the one under `DESCRIBE TABLE ref.t_all WITH INTERNALS`

`ref | index | t_all_local_idx` — create_statement identical to the one under `DESCRIBE TABLE ref.t_all WITH INTERNALS`

`ref | index | t_all_m_entries_idx` — create_statement identical to the one under `DESCRIBE TABLE ref.t_all WITH INTERNALS`

`ref | index | t_all_m_keys_idx` — create_statement identical to the one under `DESCRIBE TABLE ref.t_all WITH INTERNALS`

`ref | index | t_all_s_idx` — create_statement identical to the one under `DESCRIBE TABLE ref.t_all WITH INTERNALS`

`ref | index | t_all_st_idx` — create_statement identical to the one under `DESCRIBE TABLE ref.t_all WITH INTERNALS`

`ref | index | t_all_txt_idx` — create_statement identical to the one under `DESCRIBE TABLE ref.t_all WITH INTERNALS`

`ref | view | t_all_by_txt` — create_statement identical to the one under `DESCRIBE TABLE ref.t_all WITH INTERNALS`

`ref | table | t_cdc`

```cql
CREATE TABLE ref.t_cdc (
    id int,
    v int,
    PRIMARY KEY (id)
) WITH ID = 16effed0-ccb5-11f1-974f-a4b4b432c093
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
    AND cdc = {'delta': 'full', 'enabled': 'true', 'postimage': 'false', 'preimage': 'false', 'ttl': '86400'}
    AND tombstone_gc = {'mode': 'repair', 'propagation_delay_in_seconds': '3600'};

```
`ref | table | t_cdc_scylla_cdc_log` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_comp2`

```cql
CREATE TABLE ref.t_comp2 (
    id int,
    PRIMARY KEY (id)
) WITH ID = 16cc4a30-ccb5-11f1-974f-a4b4b432c093
AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'chunk_length_in_kb': '64', 'sstable_compression': 'org.apache.cassandra.io.compress.DeflateCompressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE'
    AND tombstone_gc = {'mode': 'repair', 'propagation_delay_in_seconds': '3600'};

```
`ref | table | t_counter`

```cql
CREATE TABLE ref.t_counter (
    id int,
    k text,
    c counter,
    c2 counter,
    PRIMARY KEY (id, k)
) WITH ID = 16b5dc00-ccb5-11f1-974f-a4b4b432c093
AND CLUSTERING ORDER BY (k ASC)
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
`ref | table | t_ics`

```cql
CREATE TABLE ref.t_ics (
    id int,
    PRIMARY KEY (id)
) WITH ID = 16dbb380-ccb5-11f1-974f-a4b4b432c093
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
`ref | table | t_id`

```cql
CREATE TABLE ref.t_id (
    id int,
    PRIMARY KEY (id)
) WITH ID = 5a1c395e-b41f-11e5-9f22-ba0be0483c18
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
`ref | table | t_opts`

```cql
CREATE TABLE ref.t_opts (
    id int,
    v text,
    PRIMARY KEY (id)
) WITH ID = 16bd5610-ccb5-11f1-974f-a4b4b432c093
AND bloom_filter_fp_chance = 0.1
    AND caching = {'keys': 'ALL', 'rows_per_partition': '10'}
    AND comment = ''
    AND compaction = {'class': 'LeveledCompactionStrategy', 'sstable_size_in_mb': '64'}
    AND compression = {'sstable_compression': 'LZ4WithDictsCompressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 3600
    AND gc_grace_seconds = 3600
    AND max_index_interval = 1024
    AND memtable_flush_period_in_ms = 60000
    AND min_index_interval = 64
    AND speculative_retry = '99.0PERCENTILE'
    AND tombstone_gc = {'mode': 'repair', 'propagation_delay_in_seconds': '3600'};

```
`ref | table | t_pgs`

```cql
CREATE TABLE ref.t_pgs (
    id int,
    PRIMARY KEY (id)
) WITH ID = 17097a40-ccb5-11f1-974f-a4b4b432c093
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
    AND paxos_grace_seconds = 100
    AND tombstone_gc = {'mode': 'repair', 'propagation_delay_in_seconds': '3600'};

```
`ref | table | t_simple`

```cql
CREATE TABLE ref.t_simple (
    id int,
    v text,
    dropped_list list<text>,
    dropped_col int,
    PRIMARY KEY (id)
) WITH ID = 16a84770-ccb5-11f1-974f-a4b4b432c093
AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'altered'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'LZ4WithDictsCompressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 7200
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE'
    AND tombstone_gc = {'mode': 'repair', 'propagation_delay_in_seconds': '3600'};

ALTER TABLE ref.t_simple DROP dropped_list USING TIMESTAMP 1792521357525283;
ALTER TABLE ref.t_simple DROP dropped_col USING TIMESTAMP 1792521357407993;
```
`ref | view | t_simple_by_v`

```cql
CREATE MATERIALIZED VIEW ref.t_simple_by_v AS
    SELECT v, id
    FROM ref.t_simple
    WHERE v IS NOT null AND id IS NOT null
    PRIMARY KEY (v, id)
    WITH ID = 17f560e0-ccb5-11f1-974f-a4b4b432c093
AND CLUSTERING ORDER BY (id ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'mv'
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
`ref | table | t_tgc`

```cql
CREATE TABLE ref.t_tgc (
    id int,
    PRIMARY KEY (id)
) WITH ID = 16e59e90-ccb5-11f1-974f-a4b4b432c093
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
    AND tombstone_gc = {'mode': 'timeout', 'propagation_delay_in_seconds': '3600'};

```
`ref | table | t_twcs`

```cql
CREATE TABLE ref.t_twcs (
    id int,
    at timestamp,
    v text,
    PRIMARY KEY (id, at)
) WITH ID = 16c32270-ccb5-11f1-974f-a4b4b432c093
AND CLUSTERING ORDER BY (at ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'TimeWindowCompactionStrategy', 'compaction_window_size': '6', 'compaction_window_unit': 'HOURS'}
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
`ref | table | t_vec`

```cql
CREATE TABLE ref.t_vec (
    id int,
    emb vector<float, 3>,
    tags vector<text, 2>,
    PRIMARY KEY (id)
) WITH ID = 183d3f50-ccb5-11f1-974f-a4b4b432c093
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
`ref | index | t_vec_emb_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_vec_emb_vs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_vec_scylla_cdc_log` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | keyspace | ref` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | type | address` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | type | person` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | "QuotedTable"`

```cql
CREATE TABLE ref."QuotedTable" (
    "Key" int,
    "Clu Col" text,
    "camelCase" text,
    "select" int,
    PRIMARY KEY ("Key", "Clu Col")
) WITH ID = 60237780-ccb5-11f1-8c36-33d044eb584e
    AND CLUSTERING ORDER BY ("Clu Col" ASC)
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
`ref | index | "QuotedIdx"` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_all` — create_statement identical to the one under `DESCRIBE TABLE ref.t_all WITH INTERNALS`

`ref | index | t_all_b_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_c2_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_fl_l_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_m_entries_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_m_keys_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_s_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_st_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_txt_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_txt_sai` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_cdc2`

```cql
CREATE TABLE ref.t_cdc2 (
    id int PRIMARY KEY,
    v int
) WITH ID = 619bbfa0-ccb5-11f1-8c36-33d044eb584e
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = true
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
`ref | table | t_comp1`

```cql
CREATE TABLE ref.t_comp1 (
    id int PRIMARY KEY
) WITH ID = 61329d90-ccb5-11f1-8c36-33d044eb584e
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = ''
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.ZstdCompressor'}
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
`ref | table | t_comp3`

```cql
CREATE TABLE ref.t_comp3 (
    id int PRIMARY KEY
) WITH ID = 61573c90-ccb5-11f1-8c36-33d044eb584e
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = ''
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'enabled': 'false'}
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
`ref | table | t_compact`

```cql
/*
Warning: Table ref.t_compact omitted because it has constructs not compatible with CQL (was created via legacy API).
Approximate structure, for reference:
(this should not be used to reproduce this schema)

CREATE TABLE ref.t_compact (
    id int,
    v int,
    PRIMARY KEY (id)
) WITH COMPACT STORAGE
    AND ID = 606b7d00-ccb5-11f1-8c36-33d044eb584e
    AND CLUSTERING ORDER BY (column1 ASC)
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
*/
```
`ref | table | t_compact2`

```cql
/*
Warning: Table ref.t_compact2 omitted because it has constructs not compatible with CQL (was created via legacy API).
Approximate structure, for reference:
(this should not be used to reproduce this schema)

CREATE TABLE ref.t_compact2 (
    id int,
    ck int,
    v int,
    PRIMARY KEY (id, ck)
) WITH COMPACT STORAGE
    AND ID = 60a5edf0-ccb5-11f1-8c36-33d044eb584e
    AND CLUSTERING ORDER BY (ck ASC)
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
*/
```
`ref | table | t_counter`

```cql
CREATE TABLE ref.t_counter (
    id int,
    k text,
    c counter,
    c2 counter,
    PRIMARY KEY (id, k)
) WITH ID = 6045a580-ccb5-11f1-8c36-33d044eb584e
    AND CLUSTERING ORDER BY (k ASC)
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
`ref | table | t_id`

```cql
CREATE TABLE ref.t_id (
    id int PRIMARY KEY
) WITH ID = 5a1c395e-b41f-11e5-9f22-ba0be0483c18
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
`ref | table | t_mt`

```cql
CREATE TABLE ref.t_mt (
    id int PRIMARY KEY
) WITH ID = 61e63620-ccb5-11f1-8c36-33d044eb584e
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
`ref | table | t_opts`

```cql
CREATE TABLE ref.t_opts (
    id int PRIMARY KEY,
    v text
) WITH ID = 60d31870-ccb5-11f1-8c36-33d044eb584e
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.1
    AND caching = {'keys': 'ALL', 'rows_per_partition': '10'}
    AND cdc = false
    AND comment = ''
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.LeveledCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4', 'sstable_size_in_mb': '64'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 0.5
    AND default_time_to_live = 3600
    AND extensions = {}
    AND gc_grace_seconds = 3600
    AND incremental_backups = true
    AND max_index_interval = 1024
    AND memtable_flush_period_in_ms = 60000
    AND min_index_interval = 64
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`ref | table | t_rr`

```cql
CREATE TABLE ref.t_rr (
    id int PRIMARY KEY
) WITH ID = 61bfe970-ccb5-11f1-8c36-33d044eb584e
    AND additional_write_policy = '90p'
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
    AND read_repair = 'NONE'
    AND speculative_retry = '99p';
```
`ref | table | t_simple`

```cql
CREATE TABLE ref.t_simple (
    id int PRIMARY KEY,
    v text,
    dropped_col int,
    dropped_list list<text>
) WITH ID = 5fd2e680-ccb5-11f1-8c36-33d044eb584e
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'altered'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 7200
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
ALTER TABLE ref.t_simple DROP dropped_col USING TIMESTAMP 1792521483501000;
ALTER TABLE ref.t_simple DROP dropped_list USING TIMESTAMP 1792521484005000;
```
`ref | index | t_simple_v_sai` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_twcs`

```cql
CREATE TABLE ref.t_twcs (
    id int,
    at timestamp,
    v text,
    PRIMARY KEY (id, at)
) WITH ID = 60fe6e30-ccb5-11f1-8c36-33d044eb584e
    AND CLUSTERING ORDER BY (at ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = ''
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.TimeWindowCompactionStrategy', 'compaction_window_size': '6', 'compaction_window_unit': 'HOURS', 'max_threshold': '32', 'min_threshold': '4'}
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
`ref | table | t_ucs`

```cql
CREATE TABLE ref.t_ucs (
    id int PRIMARY KEY
) WITH ID = 6175e820-ccb5-11f1-8c36-33d044eb584e
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = ''
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.UnifiedCompactionStrategy'}
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
`ref | table | t_vec`

```cql
CREATE TABLE ref.t_vec (
    id int PRIMARY KEY,
    emb vector<float, 3>,
    tags vector<text, 2>
) WITH ID = 65e09540-ccb5-11f1-8c36-33d044eb584e
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
`ref | index | t_vec_emb_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

## `DESCRIBE SCHEMA WITH INTERNALS`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`audit | keyspace | audit` — create_statement identical to the one under `DESCRIBE SCHEMA`

`audit | table | audit_log`

```cql
CREATE TABLE audit.audit_log (
    date timestamp,
    node inet,
    event_time timeuuid,
    category text,
    consistency text,
    error boolean,
    keyspace_name text,
    operation text,
    source inet,
    table_name text,
    username text,
    PRIMARY KEY ((date, node), event_time)
) WITH ID = dfc81bcc-633a-3940-a2b4-4510234e9d1b
AND CLUSTERING ORDER BY (event_time ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'enabled': 'false', 'keys': 'NONE', 'rows_per_partition': 'NONE'}
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
`cql_ks | keyspace | cql_ks` — create_statement identical to the one under `DESCRIBE SCHEMA`

`cql_ks | table | t`

```cql
CREATE TABLE cql_ks.t (
    k int,
    c int,
    l list<int>,
    m map<text, int>,
    s set<int>,
    v int,
    PRIMARY KEY (k, c)
) WITH ID = 06b66680-ccb5-11f1-974f-a4b4b432c093
AND CLUSTERING ORDER BY (c ASC)
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
`ref | keyspace | ref` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | type | address` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | type | person` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | "QuotedTable"` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | index | "Quoted Idx"` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_all` — create_statement identical to the one under `DESCRIBE TABLE ref.t_all WITH INTERNALS`

`ref | index | t_all_b_idx` — create_statement identical to the one under `DESCRIBE TABLE ref.t_all WITH INTERNALS`

`ref | index | t_all_c2_idx` — create_statement identical to the one under `DESCRIBE TABLE ref.t_all WITH INTERNALS`

`ref | index | t_all_fl_l_idx` — create_statement identical to the one under `DESCRIBE TABLE ref.t_all WITH INTERNALS`

`ref | index | t_all_local_idx` — create_statement identical to the one under `DESCRIBE TABLE ref.t_all WITH INTERNALS`

`ref | index | t_all_m_entries_idx` — create_statement identical to the one under `DESCRIBE TABLE ref.t_all WITH INTERNALS`

`ref | index | t_all_m_keys_idx` — create_statement identical to the one under `DESCRIBE TABLE ref.t_all WITH INTERNALS`

`ref | index | t_all_s_idx` — create_statement identical to the one under `DESCRIBE TABLE ref.t_all WITH INTERNALS`

`ref | index | t_all_st_idx` — create_statement identical to the one under `DESCRIBE TABLE ref.t_all WITH INTERNALS`

`ref | index | t_all_txt_idx` — create_statement identical to the one under `DESCRIBE TABLE ref.t_all WITH INTERNALS`

`ref | view | t_all_by_txt` — create_statement identical to the one under `DESCRIBE TABLE ref.t_all WITH INTERNALS`

`ref | table | t_cdc` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_cdc_scylla_cdc_log` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_comp2` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_counter` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_ics` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_id` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_opts` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_pgs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_simple` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | view | t_simple_by_v` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_tgc` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_twcs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_vec` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | index | t_vec_emb_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_vec_emb_vs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_vec_scylla_cdc_log` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref_vnodes | keyspace | ref_vnodes` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes`

`ref_vnodes | table | base`

```cql
CREATE TABLE ref_vnodes.base (
    id int,
    v text,
    PRIMARY KEY (id)
) WITH ID = 186a69d0-ccb5-11f1-974f-a4b4b432c093
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
`ref_vnodes | index | base_v_idx`

```cql
CREATE INDEX base_v_idx ON ref_vnodes.base(v) WITH ID = 18839721-ccb5-11f1-974f-a4b4b432c093
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
`ref_vnodes | view | base_by_v`

```cql
CREATE MATERIALIZED VIEW ref_vnodes.base_by_v AS
    SELECT *
    FROM ref_vnodes.base
    WHERE v IS NOT null AND id IS NOT null
    PRIMARY KEY (v, id)
    WITH ID = 1874ca10-ccb5-11f1-974f-a4b4b432c093
AND CLUSTERING ORDER BY (id ASC)
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
`ref_simple | keyspace | ref_simple` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_simple`

`None | service_level | driver`

```cql
CREATE SERVICE LEVEL IF NOT EXISTS driver WITH WORKLOAD_TYPE = 'batch' AND SHARES = 200;
```
`None | service_level | driver`

```cql
ALTER SERVICE LEVEL driver WITH WORKLOAD_TYPE = 'batch' AND SHARES = 200;
```
### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | keyspace | ref` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | type | address` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | type | person` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | "QuotedTable"` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | index | "QuotedIdx"` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_all` — create_statement identical to the one under `DESCRIBE TABLE ref.t_all WITH INTERNALS`

`ref | index | t_all_b_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_c2_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_fl_l_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_m_entries_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_m_keys_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_s_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_st_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_txt_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_txt_sai` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_cdc2` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_comp1` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_comp3` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_compact` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_compact2` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_counter` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_id` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_mt` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_opts` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_rr` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_simple` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | index | t_simple_v_sai` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_twcs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_ucs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_vec` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | index | t_vec_emb_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref_simple | keyspace | ref_simple` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_simple`

`ref_vnodes | keyspace | ref_vnodes` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes`

`ref_vnodes | table | base`

```cql
CREATE TABLE ref_vnodes.base (
    id int PRIMARY KEY,
    v text
) WITH ID = 66541790-ccb5-11f1-8c36-33d044eb584e
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
`ref_vnodes | index | base_v_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes`

## `DESC ONLY KEYSPACE ref`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | keyspace | ref` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | keyspace | ref` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

## `DESC TABLE ref.nope`

### ScyllaDB 2026.3.3

`0x2200` Table 'nope' not found in keyspace 'ref'

### Cassandra 5.0.9

`0x2200` Table 'nope' not found in keyspace 'ref'

## `DESC KEYSPACE nope`

### ScyllaDB 2026.3.3

`0x2200` 'nope' not found in keyspaces

### Cassandra 5.0.9

`0x2200` 'nope' not found in keyspaces

## `DESC TYPE ref.nope`

### ScyllaDB 2026.3.3

`0x2200` Type 'nope' not found in keyspace 'ref'

### Cassandra 5.0.9

`0x2200` User defined type 'nope' not found in 'ref'

## `DESC INDEX ref.nope`

### ScyllaDB 2026.3.3

`0x2200` Table for existing index 'nope' not found in 'ref'

### Cassandra 5.0.9

`0x2200` Table for existing index 'nope' not found in 'ref'

## `DESC MATERIALIZED VIEW ref.nope`

### ScyllaDB 2026.3.3

`0x2200` Materialized view 'nope' not found in keyspace 'ref'

### Cassandra 5.0.9

`0x2200` Materialized view 'nope' not found in 'ref'

## `DESC ref.t_simple`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | table | t_simple` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | view | t_simple_by_v` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

### Cassandra 5.0.9

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

`ref | table | t_simple` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_simple_v_sai` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

## `DESC ref`

### ScyllaDB 2026.3.3

Result columns: `keyspace_name varchar`, `type varchar`, `name varchar`, `create_statement varchar`

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

## `DESC nope`

### ScyllaDB 2026.3.3

`0x2200` 'nope' not found in keyspaces

### Cassandra 5.0.9

`0x2200` 'nope' not found in keyspaces

## `DESC TABLE t_simple`

### ScyllaDB 2026.3.3

`0x2200` No keyspace specified and no current keyspace

### Cassandra 5.0.9

`0x2200` No keyspace specified and no current keyspace

## `DESC FUNCTION ref.acc`

### ScyllaDB 2026.3.3

`0x2200` Function 'acc' not found in keyspace 'ref'

### Cassandra 5.0.9

`0x2200` User defined function 'acc' not found in 'ref'

## `DESC AGGREGATE ref.sum1`

### ScyllaDB 2026.3.3

`0x2200` Aggregate 'sum1' not found in keyspace 'ref'

### Cassandra 5.0.9

`0x2200` User defined aggregate 'sum1' not found in 'ref'

## `DESC FUNCTION system.now`

### ScyllaDB 2026.3.3

`0x2200` Function 'now' not found in keyspace 'system'

### Cassandra 5.0.9

`0x2200` User defined function 'now' not found in 'system'

## `DESC FUNCTION ref.nope`

### ScyllaDB 2026.3.3

`0x2200` Function 'nope' not found in keyspace 'ref'

### Cassandra 5.0.9

`0x2200` User defined function 'nope' not found in 'ref'

## `DESCRIBE TABLE ref.t_all WITH INTERNALS AND PASSWORDS`

### ScyllaDB 2026.3.3

`0x2200` Option WITH PASSWORDS is only allowed with DESC SCHEMA

### Cassandra 5.0.9

`0x2000` line 1:40 mismatched input 'AND' expecting EOF (...TABLE ref.t_all WITH INTERNALS [AND]...)

## `DESCRIBE SCHEMA WITH INTERNALS AND PASSWORDS`

### ScyllaDB 2026.3.3

`0x2100` DESCRIBE SCHEMA WITH INTERNALS AND PASSWORDS can only be issued by a superuser

### Cassandra 5.0.9

`0x2000` line 1:31 mismatched input 'AND' expecting EOF (DESCRIBE SCHEMA WITH INTERNALS [AND]...)

## `DESCRIBE SCHEMA WITH PASSWORDS`

### ScyllaDB 2026.3.3

`0x2000` line 1:21 : Missing K_INTERNALS

### Cassandra 5.0.9

`0x2000` line 1:21 mismatched input 'PASSWORDS' expecting K_INTERNALS (DESCRIBE SCHEMA WITH [PASSWORDS])

# Part 2 — with UDFs, MVs and auth enabled

The same sample schema on a node started with the suites' settings (PasswordAuthenticator, UDFs, materialized views). Only the statements whose output depends on those features are repeated.

## `DESCRIBE KEYSPACE ref` (features enabled)

### ScyllaDB 2026.3.3

`ref | keyspace | ref`

```cql
CREATE KEYSPACE ref WITH replication = {'class': 'org.apache.cassandra.locator.NetworkTopologyStrategy', 'datacenter1': ['rack1']} AND durable_writes = true AND tablets = {'enabled': true};
```
`ref | type | address` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | type | person` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | function | acc_lua`

```cql
CREATE FUNCTION ref.acc_lua(s int, x int)
CALLED ON NULL INPUT
RETURNS int
LANGUAGE lua
AS $$return s + x$$;
```
`ref | function | plus1_lua`

```cql
CREATE FUNCTION ref.plus1_lua(x int)
RETURNS NULL ON NULL INPUT
RETURNS int
LANGUAGE lua
AS $$return x + 1$$;
```
`ref | aggregate | sum1_lua`

```cql
CREATE AGGREGATE ref.sum1_lua(int)
SFUNC acc_lua
STYPE int
INITCOND 0;
```
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

### Cassandra 5.0.9

`ref | keyspace | ref` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | type | address` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | type | person` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | function | acc(int, int)`

```cql
CREATE FUNCTION ref.acc(s int, x int)
    CALLED ON NULL INPUT
    RETURNS int
    LANGUAGE java
    AS $$return s + x;$$;
```
`ref | function | plus1(int)`

```cql
CREATE FUNCTION ref.plus1(x int)
    RETURNS NULL ON NULL INPUT
    RETURNS int
    LANGUAGE java
    AS $$return x + 1;$$;
```
`ref | aggregate | sum1(int)`

```cql
CREATE AGGREGATE ref.sum1(int)
    SFUNC acc
    STYPE int
    INITCOND 0;
```
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

`ref | materialized_view | t_all_by_txt`

```cql
CREATE MATERIALIZED VIEW ref.t_all_by_txt AS
    SELECT txt, a, b, c1, c2
    FROM ref.t_all
    WHERE txt IS NOT NULL AND a IS NOT NULL AND b IS NOT NULL AND c1 IS NOT NULL AND c2 IS NOT NULL
    PRIMARY KEY (txt, a, b, c1, c2)
 WITH CLUSTERING ORDER BY (a DESC, b ASC, c1 ASC, c2 DESC)
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
    AND extensions = {}
    AND gc_grace_seconds = 864000
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`ref | table | t_cdc2` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_comp1` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_comp3` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_compact` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_compact2` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_counter` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_id` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_mask`

```cql
CREATE TABLE ref.t_mask (
    id int PRIMARY KEY,
    email text MASKED WITH system.mask_inner(1, 1),
    ssn text MASKED WITH system.mask_default()
) WITH additional_write_policy = '99p'
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
`ref | table | t_mt` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_opts` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_opts_sasi`

```cql
CREATE CUSTOM INDEX t_opts_sasi ON ref.t_opts (v) USING 'org.apache.cassandra.index.sasi.SASIIndex';
```
`ref | table | t_rr` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_simple` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_simple_v_sai` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | materialized_view | t_simple_by_v`

```cql
CREATE MATERIALIZED VIEW ref.t_simple_by_v AS
    SELECT v, id
    FROM ref.t_simple
    WHERE v IS NOT NULL AND id IS NOT NULL
    PRIMARY KEY (v, id)
 WITH CLUSTERING ORDER BY (id ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'mv'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND extensions = {}
    AND gc_grace_seconds = 864000
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`ref | table | t_twcs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_ucs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_vec` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_vec_emb_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

## `DESCRIBE KEYSPACE ref_simple` (features enabled)

### ScyllaDB 2026.3.3

`ref_simple | keyspace | ref_simple` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_simple`

### Cassandra 5.0.9

`ref_simple | keyspace | ref_simple` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_simple`

## `DESCRIBE KEYSPACE ref_vnodes` (features enabled)

### ScyllaDB 2026.3.3

`ref_vnodes | keyspace | ref_vnodes` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes`

`ref_vnodes | table | base` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes`

`ref_vnodes | index | base_v_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes`

`ref_vnodes | view | base_by_v` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes`

### Cassandra 5.0.9

`ref_vnodes | keyspace | ref_vnodes` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes`

`ref_vnodes | table | base` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes`

`ref_vnodes | index | base_v_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes`

`ref_vnodes | materialized_view | base_by_v`

```cql
CREATE MATERIALIZED VIEW ref_vnodes.base_by_v AS
    SELECT *
    FROM ref_vnodes.base
    WHERE v IS NOT NULL AND id IS NOT NULL
    PRIMARY KEY (v, id)
 WITH CLUSTERING ORDER BY (id ASC)
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
    AND extensions = {}
    AND gc_grace_seconds = 864000
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
## `DESCRIBE SCHEMA` (features enabled)

### ScyllaDB 2026.3.3

`audit | keyspace | audit` — create_statement identical to the one under `DESCRIBE SCHEMA`

`audit | table | audit_log` — create_statement identical to the one under `DESCRIBE SCHEMA`

`ref | keyspace | ref` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | type | address` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | type | person` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | function | acc_lua` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | function | plus1_lua` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | aggregate | sum1_lua` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

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

`ref | keyspace | ref` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | type | address` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | type | person` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | function | acc(int, int)` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | function | plus1(int)` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | aggregate | sum1(int)` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

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

`ref | materialized_view | t_all_by_txt` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | table | t_cdc2` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_comp1` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_comp3` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_compact` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_compact2` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_counter` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_id` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_mask` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | table | t_mt` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_opts` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_opts_sasi` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | table | t_rr` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_simple` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_simple_v_sai` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | materialized_view | t_simple_by_v` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | table | t_twcs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_ucs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_vec` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_vec_emb_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref_simple | keyspace | ref_simple` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_simple`

`ref_vnodes | keyspace | ref_vnodes` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes`

`ref_vnodes | table | base` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes`

`ref_vnodes | index | base_v_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes`

`ref_vnodes | materialized_view | base_by_v` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes (features enabled)`

## `DESC FUNCTIONS` (features enabled)

### ScyllaDB 2026.3.3

```
ref | function | acc_lua
ref | function | plus1_lua
```
### Cassandra 5.0.9

```
ref | function | acc(int, int)
ref | function | plus1(int)
```
## `DESC AGGREGATES` (features enabled)

### ScyllaDB 2026.3.3

```
ref | aggregate | sum1_lua
```
### Cassandra 5.0.9

```
ref | aggregate | sum1(int)
```
## `DESC MATERIALIZED VIEW ref.t_simple_by_v` (features enabled)

### ScyllaDB 2026.3.3

`ref | view | t_simple_by_v` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

### Cassandra 5.0.9

`ref | materialized_view | t_simple_by_v` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

## `DESC MATERIALIZED VIEW ref.t_all_by_txt` (features enabled)

### ScyllaDB 2026.3.3

`ref | view | t_all_by_txt` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

### Cassandra 5.0.9

`ref | materialized_view | t_all_by_txt` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

## `DESC MATERIALIZED VIEW ref_vnodes.base_by_v` (features enabled)

### ScyllaDB 2026.3.3

`ref_vnodes | view | base_by_v` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes`

### Cassandra 5.0.9

`ref_vnodes | materialized_view | base_by_v` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes (features enabled)`

## `DESCRIBE KEYSPACE ref WITH INTERNALS` (features enabled)

### ScyllaDB 2026.3.3

`ref | keyspace | ref` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | type | address` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | type | person` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | function | acc_lua` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | function | plus1_lua` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | aggregate | sum1_lua` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | table | "QuotedTable"`

```cql
CREATE TABLE ref."QuotedTable" (
    "Key" int,
    "Clu Col" text,
    "camelCase" text,
    "select" int,
    PRIMARY KEY ("Key", "Clu Col")
) WITH ID = da806cc0-ccc1-11f1-b647-c0eb11f359c7
AND CLUSTERING ORDER BY ("Clu Col" ASC)
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
`ref | index | "Quoted Idx"`

```cql
CREATE INDEX "Quoted Idx" ON ref."QuotedTable"("camelCase") WITH ID = db4d80c1-ccc1-11f1-b647-c0eb11f359c7
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
`ref | table | t_all`

```cql
CREATE TABLE ref.t_all (
    a int,
    b text,
    c1 timestamp,
    c2 uuid,
    s text STATIC,
    addr address,
    addrs list<frozen<address>>,
    asc_ ascii,
    big bigint,
    bl blob,
    bo boolean,
    da date,
    de decimal,
    do double,
    du duration,
    faddr frozen<address>,
    fl float,
    fl_l frozen<list<int>>,
    fl_m frozen<map<text, int>>,
    fl_s frozen<set<text>>,
    ftup frozen<tuple<int, frozen<tuple<text, bigint>>>>,
    ine inet,
    l list<int>,
    m map<text, int>,
    nested map<text, frozen<list<int>>>,
    sm smallint,
    st set<text>,
    ti time,
    tin tinyint,
    tu timeuuid,
    tup frozen<tuple<int, text, float>>,
    txt text,
    vc text,
    vi varint,
    who frozen<person>,
    PRIMARY KEY ((a, b), c1, c2)
) WITH ID = da7c7520-ccc1-11f1-b647-c0eb11f359c7
AND CLUSTERING ORDER BY (c1 DESC, c2 ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'sample table'
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
`ref | index | t_all_b_idx`

```cql
CREATE INDEX t_all_b_idx ON ref.t_all(b) WITH ID = db2b2bb1-ccc1-11f1-b647-c0eb11f359c7
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
`ref | index | t_all_c2_idx`

```cql
CREATE INDEX t_all_c2_idx ON ref.t_all(c2) WITH ID = db34efb1-ccc1-11f1-b647-c0eb11f359c7
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
`ref | index | t_all_fl_l_idx`

```cql
CREATE INDEX t_all_fl_l_idx ON ref.t_all(full(fl_l)) WITH ID = db1580d0-ccc1-11f1-b647-c0eb11f359c7
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
`ref | index | t_all_local_idx`

```cql
CREATE INDEX t_all_local_idx ON ref.t_all((a, b), vi) WITH ID = db40fda1-ccc1-11f1-b647-c0eb11f359c7
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
    AND scylla_tags = {'system:synchronous_view_updates': 'true'}
    AND tombstone_gc = {'mode': 'timeout', 'propagation_delay_in_seconds': '3600'}
    AND synchronous_updates = true;

```
`ref | index | t_all_m_entries_idx`

```cql
CREATE INDEX t_all_m_entries_idx ON ref.t_all(entries(m)) WITH ID = db0be3e1-ccc1-11f1-b647-c0eb11f359c7
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
`ref | index | t_all_m_keys_idx`

```cql
CREATE INDEX t_all_m_keys_idx ON ref.t_all(keys(m)) WITH ID = daf6ae30-ccc1-11f1-b647-c0eb11f359c7
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
`ref | index | t_all_s_idx`

```cql
CREATE INDEX t_all_s_idx ON ref.t_all(s) WITH ID = db1fe110-ccc1-11f1-b647-c0eb11f359c7
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
`ref | index | t_all_st_idx`

```cql
CREATE INDEX t_all_st_idx ON ref.t_all(values(st)) WITH ID = db00c050-ccc1-11f1-b647-c0eb11f359c7
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
`ref | index | t_all_txt_idx`

```cql
CREATE INDEX t_all_txt_idx ON ref.t_all(txt) WITH ID = daed1140-ccc1-11f1-b647-c0eb11f359c7
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
`ref | view | t_all_by_txt`

```cql
CREATE MATERIALIZED VIEW ref.t_all_by_txt AS
    SELECT txt, a, b, c1, c2
    FROM ref.t_all
    WHERE txt IS NOT null AND a IS NOT null AND b IS NOT null AND c1 IS NOT null AND c2 IS NOT null
    PRIMARY KEY (txt, a, b, c1, c2)
    WITH ID = db6ec460-ccc1-11f1-b647-c0eb11f359c7
AND CLUSTERING ORDER BY (a DESC, b ASC, c1 ASC, c2 DESC)
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
`ref | table | t_cdc`

```cql
CREATE TABLE ref.t_cdc (
    id int,
    v int,
    PRIMARY KEY (id)
) WITH ID = daa7cae0-ccc1-11f1-b647-c0eb11f359c7
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
    AND cdc = {'delta': 'full', 'enabled': 'true', 'postimage': 'false', 'preimage': 'false', 'ttl': '86400'}
    AND tombstone_gc = {'mode': 'repair', 'propagation_delay_in_seconds': '3600'};

```
`ref | table | t_cdc_scylla_cdc_log` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_comp2`

```cql
CREATE TABLE ref.t_comp2 (
    id int,
    PRIMARY KEY (id)
) WITH ID = da93f4c0-ccc1-11f1-b647-c0eb11f359c7
AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'chunk_length_in_kb': '64', 'sstable_compression': 'org.apache.cassandra.io.compress.DeflateCompressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 864000
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE'
    AND tombstone_gc = {'mode': 'repair', 'propagation_delay_in_seconds': '3600'};

```
`ref | table | t_counter`

```cql
CREATE TABLE ref.t_counter (
    id int,
    k text,
    c counter,
    c2 counter,
    PRIMARY KEY (id, k)
) WITH ID = da854ec0-ccc1-11f1-b647-c0eb11f359c7
AND CLUSTERING ORDER BY (k ASC)
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
`ref | table | t_ics`

```cql
CREATE TABLE ref.t_ics (
    id int,
    PRIMARY KEY (id)
) WITH ID = da983a80-ccc1-11f1-b647-c0eb11f359c7
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
`ref | table | t_id` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_opts`

```cql
CREATE TABLE ref.t_opts (
    id int,
    v text,
    PRIMARY KEY (id)
) WITH ID = da8a57d0-ccc1-11f1-b647-c0eb11f359c7
AND bloom_filter_fp_chance = 0.1
    AND caching = {'keys': 'ALL', 'rows_per_partition': '10'}
    AND comment = ''
    AND compaction = {'class': 'LeveledCompactionStrategy', 'sstable_size_in_mb': '64'}
    AND compression = {'sstable_compression': 'LZ4WithDictsCompressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 3600
    AND gc_grace_seconds = 3600
    AND max_index_interval = 1024
    AND memtable_flush_period_in_ms = 60000
    AND min_index_interval = 64
    AND speculative_retry = '99.0PERCENTILE'
    AND tombstone_gc = {'mode': 'repair', 'propagation_delay_in_seconds': '3600'};

```
`ref | table | t_pgs`

```cql
CREATE TABLE ref.t_pgs (
    id int,
    PRIMARY KEY (id)
) WITH ID = dabf2370-ccc1-11f1-b647-c0eb11f359c7
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
    AND paxos_grace_seconds = 100
    AND tombstone_gc = {'mode': 'repair', 'propagation_delay_in_seconds': '3600'};

```
`ref | table | t_simple`

```cql
CREATE TABLE ref.t_simple (
    id int,
    v text,
    dropped_list list<text>,
    dropped_col int,
    PRIMARY KEY (id)
) WITH ID = da723bf0-ccc1-11f1-b647-c0eb11f359c7
AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'altered'
    AND compaction = {'class': 'IncrementalCompactionStrategy'}
    AND compression = {'sstable_compression': 'LZ4WithDictsCompressor'}
    AND crc_check_chance = 1
    AND default_time_to_live = 0
    AND gc_grace_seconds = 7200
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND speculative_retry = '99.0PERCENTILE'
    AND tombstone_gc = {'mode': 'repair', 'propagation_delay_in_seconds': '3600'};

ALTER TABLE ref.t_simple DROP dropped_list USING TIMESTAMP 1792526839764863;
ALTER TABLE ref.t_simple DROP dropped_col USING TIMESTAMP 1792526839678096;
```
`ref | view | t_simple_by_v`

```cql
CREATE MATERIALIZED VIEW ref.t_simple_by_v AS
    SELECT v, id
    FROM ref.t_simple
    WHERE v IS NOT null AND id IS NOT null
    PRIMARY KEY (v, id)
    WITH ID = db63eef0-ccc1-11f1-b647-c0eb11f359c7
AND CLUSTERING ORDER BY (id ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = 'mv'
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
`ref | table | t_tgc`

```cql
CREATE TABLE ref.t_tgc (
    id int,
    PRIMARY KEY (id)
) WITH ID = daa077e0-ccc1-11f1-b647-c0eb11f359c7
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
    AND tombstone_gc = {'mode': 'timeout', 'propagation_delay_in_seconds': '3600'};

```
`ref | table | t_twcs`

```cql
CREATE TABLE ref.t_twcs (
    id int,
    at timestamp,
    v text,
    PRIMARY KEY (id, at)
) WITH ID = da8f39d0-ccc1-11f1-b647-c0eb11f359c7
AND CLUSTERING ORDER BY (at ASC)
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'ALL'}
    AND comment = ''
    AND compaction = {'class': 'TimeWindowCompactionStrategy', 'compaction_window_size': '6', 'compaction_window_unit': 'HOURS'}
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
`ref | table | t_vec`

```cql
CREATE TABLE ref.t_vec (
    id int,
    emb vector<float, 3>,
    tags vector<text, 2>,
    PRIMARY KEY (id)
) WITH ID = db9e11c0-ccc1-11f1-b647-c0eb11f359c7
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
`ref | index | t_vec_emb_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_vec_emb_vs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_vec_scylla_cdc_log` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

### Cassandra 5.0.9

`ref | keyspace | ref` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | type | address` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | type | person` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | function | acc(int, int)` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | function | plus1(int)` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | aggregate | sum1(int)` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | table | "QuotedTable"`

```cql
CREATE TABLE ref."QuotedTable" (
    "Key" int,
    "Clu Col" text,
    "camelCase" text,
    "select" int,
    PRIMARY KEY ("Key", "Clu Col")
) WITH ID = e4f48c70-cccd-11f1-9c21-2f5cfa694e51
    AND CLUSTERING ORDER BY ("Clu Col" ASC)
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
`ref | index | "QuotedIdx"` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_all`

```cql
CREATE TABLE ref.t_all (
    a int,
    b text,
    c1 timestamp,
    c2 uuid,
    s text static,
    asc_ ascii,
    big bigint,
    bl blob,
    bo boolean,
    da date,
    de decimal,
    do double,
    du duration,
    faddr frozen<address>,
    fl float,
    fl_l frozen<list<int>>,
    fl_m frozen<map<text, int>>,
    fl_s frozen<set<text>>,
    ftup frozen<tuple<int, frozen<tuple<text, bigint>>>>,
    ine inet,
    sm smallint,
    ti time,
    tin tinyint,
    tu timeuuid,
    tup frozen<tuple<int, text, float>>,
    txt text,
    vc text,
    vi varint,
    who frozen<person>,
    addr address,
    addrs list<frozen<address>>,
    l list<int>,
    m map<text, int>,
    nested map<text, frozen<list<int>>>,
    st set<text>,
    PRIMARY KEY ((a, b), c1, c2)
) WITH ID = e4deba80-cccd-11f1-9c21-2f5cfa694e51
    AND CLUSTERING ORDER BY (c1 DESC, c2 ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'sample table'
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
`ref | index | t_all_b_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_c2_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_fl_l_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_m_entries_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_m_keys_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_s_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_st_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_txt_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_txt_sai` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | materialized_view | t_all_by_txt`

```cql
CREATE MATERIALIZED VIEW ref.t_all_by_txt AS
    SELECT txt, a, b, c1, c2
    FROM ref.t_all
    WHERE txt IS NOT NULL AND a IS NOT NULL AND b IS NOT NULL AND c1 IS NOT NULL AND c2 IS NOT NULL
    PRIMARY KEY (txt, a, b, c1, c2)
 WITH ID = e787ba70-cccd-11f1-9c21-2f5cfa694e51
    AND CLUSTERING ORDER BY (a DESC, b ASC, c1 ASC, c2 DESC)
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
    AND extensions = {}
    AND gc_grace_seconds = 864000
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`ref | table | t_cdc2`

```cql
CREATE TABLE ref.t_cdc2 (
    id int PRIMARY KEY,
    v int
) WITH ID = e5af7800-cccd-11f1-9c21-2f5cfa694e51
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = true
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
`ref | table | t_comp1`

```cql
CREATE TABLE ref.t_comp1 (
    id int PRIMARY KEY
) WITH ID = e5704c20-cccd-11f1-9c21-2f5cfa694e51
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = ''
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.ZstdCompressor'}
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
`ref | table | t_comp3`

```cql
CREATE TABLE ref.t_comp3 (
    id int PRIMARY KEY
) WITH ID = e585cff0-cccd-11f1-9c21-2f5cfa694e51
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = ''
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'enabled': 'false'}
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
`ref | table | t_compact`

```cql
/*
Warning: Table ref.t_compact omitted because it has constructs not compatible with CQL (was created via legacy API).
Approximate structure, for reference:
(this should not be used to reproduce this schema)

CREATE TABLE ref.t_compact (
    id int,
    v int,
    PRIMARY KEY (id)
) WITH COMPACT STORAGE
    AND ID = e51fe230-cccd-11f1-9c21-2f5cfa694e51
    AND CLUSTERING ORDER BY (column1 ASC)
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
*/
```
`ref | table | t_compact2`

```cql
/*
Warning: Table ref.t_compact2 omitted because it has constructs not compatible with CQL (was created via legacy API).
Approximate structure, for reference:
(this should not be used to reproduce this schema)

CREATE TABLE ref.t_compact2 (
    id int,
    ck int,
    v int,
    PRIMARY KEY (id, ck)
) WITH COMPACT STORAGE
    AND ID = e531e390-cccd-11f1-9c21-2f5cfa694e51
    AND CLUSTERING ORDER BY (ck ASC)
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
*/
```
`ref | table | t_counter`

```cql
CREATE TABLE ref.t_counter (
    id int,
    k text,
    c counter,
    c2 counter,
    PRIMARY KEY (id, k)
) WITH ID = e50a1040-cccd-11f1-9c21-2f5cfa694e51
    AND CLUSTERING ORDER BY (k ASC)
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
`ref | table | t_id` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_mask`

```cql
CREATE TABLE ref.t_mask (
    id int PRIMARY KEY,
    email text MASKED WITH system.mask_inner(1, 1),
    ssn text MASKED WITH system.mask_default()
) WITH ID = e814df40-cccd-11f1-9c21-2f5cfa694e51
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
`ref | table | t_mt`

```cql
CREATE TABLE ref.t_mt (
    id int PRIMARY KEY
) WITH ID = e5d7c080-cccd-11f1-9c21-2f5cfa694e51
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
`ref | table | t_opts`

```cql
CREATE TABLE ref.t_opts (
    id int PRIMARY KEY,
    v text
) WITH ID = e54878d0-cccd-11f1-9c21-2f5cfa694e51
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.1
    AND caching = {'keys': 'ALL', 'rows_per_partition': '10'}
    AND cdc = false
    AND comment = ''
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.LeveledCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4', 'sstable_size_in_mb': '64'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 0.5
    AND default_time_to_live = 3600
    AND extensions = {}
    AND gc_grace_seconds = 3600
    AND incremental_backups = true
    AND max_index_interval = 1024
    AND memtable_flush_period_in_ms = 60000
    AND min_index_interval = 64
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`ref | index | t_opts_sasi` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | table | t_rr`

```cql
CREATE TABLE ref.t_rr (
    id int PRIMARY KEY
) WITH ID = e5c3ea60-cccd-11f1-9c21-2f5cfa694e51
    AND additional_write_policy = '90p'
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
    AND read_repair = 'NONE'
    AND speculative_retry = '99p';
```
`ref | table | t_simple`

```cql
CREATE TABLE ref.t_simple (
    id int PRIMARY KEY,
    v text,
    dropped_col int,
    dropped_list list<text>
) WITH ID = e4c8e890-cccd-11f1-9c21-2f5cfa694e51
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'altered'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND default_time_to_live = 0
    AND extensions = {}
    AND gc_grace_seconds = 7200
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
ALTER TABLE ref.t_simple DROP dropped_col USING TIMESTAMP 1792532012562000;
ALTER TABLE ref.t_simple DROP dropped_list USING TIMESTAMP 1792532012785000;
```
`ref | index | t_simple_v_sai` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | materialized_view | t_simple_by_v`

```cql
CREATE MATERIALIZED VIEW ref.t_simple_by_v AS
    SELECT v, id
    FROM ref.t_simple
    WHERE v IS NOT NULL AND id IS NOT NULL
    PRIMARY KEY (v, id)
 WITH ID = e7751cd0-cccd-11f1-9c21-2f5cfa694e51
    AND CLUSTERING ORDER BY (id ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = 'mv'
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy', 'max_threshold': '32', 'min_threshold': '4'}
    AND compression = {'chunk_length_in_kb': '16', 'class': 'org.apache.cassandra.io.compress.LZ4Compressor'}
    AND memtable = 'default'
    AND crc_check_chance = 1.0
    AND extensions = {}
    AND gc_grace_seconds = 864000
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
`ref | table | t_twcs`

```cql
CREATE TABLE ref.t_twcs (
    id int,
    at timestamp,
    v text,
    PRIMARY KEY (id, at)
) WITH ID = e55dd590-cccd-11f1-9c21-2f5cfa694e51
    AND CLUSTERING ORDER BY (at ASC)
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = ''
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.TimeWindowCompactionStrategy', 'compaction_window_size': '6', 'compaction_window_unit': 'HOURS', 'max_threshold': '32', 'min_threshold': '4'}
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
`ref | table | t_ucs`

```cql
CREATE TABLE ref.t_ucs (
    id int PRIMARY KEY
) WITH ID = e59d2880-cccd-11f1-9c21-2f5cfa694e51
    AND additional_write_policy = '99p'
    AND allow_auto_snapshot = true
    AND bloom_filter_fp_chance = 0.01
    AND caching = {'keys': 'ALL', 'rows_per_partition': 'NONE'}
    AND cdc = false
    AND comment = ''
    AND compaction = {'class': 'org.apache.cassandra.db.compaction.UnifiedCompactionStrategy'}
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
`ref | table | t_vec`

```cql
CREATE TABLE ref.t_vec (
    id int PRIMARY KEY,
    emb vector<float, 3>,
    tags vector<text, 2>
) WITH ID = e7f04040-cccd-11f1-9c21-2f5cfa694e51
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
`ref | index | t_vec_emb_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

## `DESCRIBE SCHEMA WITH INTERNALS` (features enabled)

### ScyllaDB 2026.3.3

`audit | keyspace | audit` — create_statement identical to the one under `DESCRIBE SCHEMA`

`audit | table | audit_log` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS`

`ref | keyspace | ref` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | type | address` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | type | person` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | function | acc_lua` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | function | plus1_lua` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | aggregate | sum1_lua` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | table | "QuotedTable"` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | "Quoted Idx"` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_all` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | t_all_b_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | t_all_c2_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | t_all_fl_l_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | t_all_local_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | t_all_m_entries_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | t_all_m_keys_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | t_all_s_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | t_all_st_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | t_all_txt_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | view | t_all_by_txt` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_cdc` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_cdc_scylla_cdc_log` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_comp2` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_counter` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_ics` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_id` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_opts` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_pgs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_simple` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | view | t_simple_by_v` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_tgc` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_twcs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_vec` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | t_vec_emb_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_vec_emb_vs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_vec_scylla_cdc_log` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref_vnodes | keyspace | ref_vnodes` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes`

`ref_vnodes | table | base`

```cql
CREATE TABLE ref_vnodes.base (
    id int,
    v text,
    PRIMARY KEY (id)
) WITH ID = dbb87790-ccc1-11f1-b647-c0eb11f359c7
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
`ref_vnodes | index | base_v_idx`

```cql
CREATE INDEX base_v_idx ON ref_vnodes.base(v) WITH ID = dbcd5f21-ccc1-11f1-b647-c0eb11f359c7
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
`ref_vnodes | view | base_by_v`

```cql
CREATE MATERIALIZED VIEW ref_vnodes.base_by_v AS
    SELECT *
    FROM ref_vnodes.base
    WHERE v IS NOT null AND id IS NOT null
    PRIMARY KEY (v, id)
    WITH ID = dbc2b0c0-ccc1-11f1-b647-c0eb11f359c7
AND CLUSTERING ORDER BY (id ASC)
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
`ref_simple | keyspace | ref_simple` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_simple`

`None | role | cassandra`

```cql
CREATE ROLE IF NOT EXISTS cassandra WITH LOGIN = true AND SUPERUSER = true;
```
`None | grant_permission | cassandra`

```cql
GRANT ALTER ON KEYSPACE ref TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT ALTER ON KEYSPACE ref_simple TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT ALTER ON KEYSPACE ref_vnodes TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT ALTER ON ref."QuotedTable" TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT ALTER ON ref.t_all TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT ALTER ON ref.t_cdc TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT ALTER ON ref.t_comp2 TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT ALTER ON ref.t_counter TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT ALTER ON ref.t_ics TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT ALTER ON ref.t_id TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT ALTER ON ref.t_opts TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT ALTER ON ref.t_pgs TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT ALTER ON ref.t_simple TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT ALTER ON ref.t_tgc TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT ALTER ON ref.t_twcs TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT ALTER ON ref.t_vec TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT ALTER ON ref_vnodes.base TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT AUTHORIZE ON KEYSPACE ref TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT AUTHORIZE ON KEYSPACE ref_simple TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT AUTHORIZE ON KEYSPACE ref_vnodes TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT AUTHORIZE ON ref."QuotedTable" TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT AUTHORIZE ON ref.t_all TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT AUTHORIZE ON ref.t_cdc TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT AUTHORIZE ON ref.t_comp2 TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT AUTHORIZE ON ref.t_counter TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT AUTHORIZE ON ref.t_ics TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT AUTHORIZE ON ref.t_id TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT AUTHORIZE ON ref.t_opts TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT AUTHORIZE ON ref.t_pgs TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT AUTHORIZE ON ref.t_simple TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT AUTHORIZE ON ref.t_tgc TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT AUTHORIZE ON ref.t_twcs TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT AUTHORIZE ON ref.t_vec TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT AUTHORIZE ON ref_vnodes.base TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT CREATE ON KEYSPACE ref TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT CREATE ON KEYSPACE ref_simple TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT CREATE ON KEYSPACE ref_vnodes TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT DROP ON KEYSPACE ref TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT DROP ON KEYSPACE ref_simple TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT DROP ON KEYSPACE ref_vnodes TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT DROP ON ref."QuotedTable" TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT DROP ON ref.t_all TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT DROP ON ref.t_cdc TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT DROP ON ref.t_comp2 TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT DROP ON ref.t_counter TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT DROP ON ref.t_ics TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT DROP ON ref.t_id TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT DROP ON ref.t_opts TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT DROP ON ref.t_pgs TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT DROP ON ref.t_simple TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT DROP ON ref.t_tgc TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT DROP ON ref.t_twcs TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT DROP ON ref.t_vec TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT DROP ON ref_vnodes.base TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT MODIFY ON KEYSPACE ref TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT MODIFY ON KEYSPACE ref_simple TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT MODIFY ON KEYSPACE ref_vnodes TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT MODIFY ON ref."QuotedTable" TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT MODIFY ON ref.t_all TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT MODIFY ON ref.t_cdc TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT MODIFY ON ref.t_comp2 TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT MODIFY ON ref.t_counter TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT MODIFY ON ref.t_ics TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT MODIFY ON ref.t_id TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT MODIFY ON ref.t_opts TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT MODIFY ON ref.t_pgs TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT MODIFY ON ref.t_simple TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT MODIFY ON ref.t_tgc TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT MODIFY ON ref.t_twcs TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT MODIFY ON ref.t_vec TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT MODIFY ON ref_vnodes.base TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT SELECT ON KEYSPACE ref TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT SELECT ON KEYSPACE ref_simple TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT SELECT ON KEYSPACE ref_vnodes TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT SELECT ON ref."QuotedTable" TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT SELECT ON ref.t_all TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT SELECT ON ref.t_cdc TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT SELECT ON ref.t_comp2 TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT SELECT ON ref.t_counter TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT SELECT ON ref.t_ics TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT SELECT ON ref.t_id TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT SELECT ON ref.t_opts TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT SELECT ON ref.t_pgs TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT SELECT ON ref.t_simple TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT SELECT ON ref.t_tgc TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT SELECT ON ref.t_twcs TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT SELECT ON ref.t_vec TO cassandra;
```
`None | grant_permission | cassandra`

```cql
GRANT SELECT ON ref_vnodes.base TO cassandra;
```
`None | service_level | driver` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS`

`None | service_level | driver` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS`

### Cassandra 5.0.9

`ref | keyspace | ref` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | type | address` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | type | person` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | function | acc(int, int)` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | function | plus1(int)` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | aggregate | sum1(int)` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | table | "QuotedTable"` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | "QuotedIdx"` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_all` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | t_all_b_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_c2_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_fl_l_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_m_entries_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_m_keys_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_s_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_st_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_txt_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_all_txt_sai` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | materialized_view | t_all_by_txt` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_cdc2` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_comp1` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_comp3` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_compact` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_compact2` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_counter` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_id` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_mask` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_mt` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_opts` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | t_opts_sasi` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | table | t_rr` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_simple` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | t_simple_v_sai` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | materialized_view | t_simple_by_v` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_twcs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_ucs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_vec` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | t_vec_emb_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref_simple | keyspace | ref_simple` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_simple`

`ref_vnodes | keyspace | ref_vnodes` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes`

`ref_vnodes | table | base`

```cql
CREATE TABLE ref_vnodes.base (
    id int PRIMARY KEY,
    v text
) WITH ID = e8299fc0-cccd-11f1-9c21-2f5cfa694e51
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
`ref_vnodes | index | base_v_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes`

`ref_vnodes | materialized_view | base_by_v`

```cql
CREATE MATERIALIZED VIEW ref_vnodes.base_by_v AS
    SELECT *
    FROM ref_vnodes.base
    WHERE v IS NOT NULL AND id IS NOT NULL
    PRIMARY KEY (v, id)
 WITH ID = e83eae60-cccd-11f1-9c21-2f5cfa694e51
    AND CLUSTERING ORDER BY (id ASC)
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
    AND extensions = {}
    AND gc_grace_seconds = 864000
    AND incremental_backups = true
    AND max_index_interval = 2048
    AND memtable_flush_period_in_ms = 0
    AND min_index_interval = 128
    AND read_repair = 'BLOCKING'
    AND speculative_retry = '99p';
```
## `DESC ONLY KEYSPACE ref` (features enabled)

### ScyllaDB 2026.3.3

`ref | keyspace | ref` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

### Cassandra 5.0.9

`ref | keyspace | ref` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

## `DESC MATERIALIZED VIEW ref.nope` (features enabled)

### ScyllaDB 2026.3.3

`0x2200` Materialized view 'nope' not found in keyspace 'ref'

### Cassandra 5.0.9

`0x2200` Materialized view 'nope' not found in 'ref'

## `DESC FUNCTION ref.acc` (features enabled)

### ScyllaDB 2026.3.3

`0x2200` Function 'acc' not found in keyspace 'ref'

### Cassandra 5.0.9

`ref | function | acc(int, int)` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

## `DESC AGGREGATE ref.sum1` (features enabled)

### ScyllaDB 2026.3.3

`0x2200` Aggregate 'sum1' not found in keyspace 'ref'

### Cassandra 5.0.9

`ref | aggregate | sum1(int)` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

## `DESC FUNCTION system.now` (features enabled)

### ScyllaDB 2026.3.3

`0x2200` Function 'now' not found in keyspace 'system'

### Cassandra 5.0.9

`0x2200` User defined function 'now' not found in 'system'

## `DESC FUNCTION ref.nope` (features enabled)

### ScyllaDB 2026.3.3

`0x2200` Function 'nope' not found in keyspace 'ref'

### Cassandra 5.0.9

`0x2200` User defined function 'nope' not found in 'ref'

## `DESCRIBE TABLE ref.t_all WITH INTERNALS AND PASSWORDS` (features enabled)

### ScyllaDB 2026.3.3

`0x2200` Option WITH PASSWORDS is only allowed with DESC SCHEMA

### Cassandra 5.0.9

`0x2000` line 1:40 mismatched input 'AND' expecting EOF (...TABLE ref.t_all WITH INTERNALS [AND]...)

## `DESCRIBE SCHEMA WITH INTERNALS AND PASSWORDS` (features enabled)

### ScyllaDB 2026.3.3

`audit | keyspace | audit` — create_statement identical to the one under `DESCRIBE SCHEMA`

`audit | table | audit_log` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS`

`ref | keyspace | ref` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | type | address` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | type | person` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | function | acc_lua` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | function | plus1_lua` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | aggregate | sum1_lua` — create_statement identical to the one under `DESCRIBE KEYSPACE ref (features enabled)`

`ref | table | "QuotedTable"` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | "Quoted Idx"` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_all` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | t_all_b_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | t_all_c2_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | t_all_fl_l_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | t_all_local_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | t_all_m_entries_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | t_all_m_keys_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | t_all_s_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | t_all_st_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | t_all_txt_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | view | t_all_by_txt` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_cdc` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_cdc_scylla_cdc_log` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_comp2` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_counter` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_ics` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_id` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS`

`ref | table | t_opts` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_pgs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_simple` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | view | t_simple_by_v` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_tgc` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_twcs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | table | t_vec` — create_statement identical to the one under `DESCRIBE KEYSPACE ref WITH INTERNALS (features enabled)`

`ref | index | t_vec_emb_idx` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | index | t_vec_emb_vs` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref | table | t_vec_scylla_cdc_log` — create_statement identical to the one under `DESCRIBE KEYSPACE ref`

`ref_vnodes | keyspace | ref_vnodes` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_vnodes`

`ref_vnodes | table | base` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`ref_vnodes | index | base_v_idx` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`ref_vnodes | view | base_by_v` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`ref_simple | keyspace | ref_simple` — create_statement identical to the one under `DESCRIBE KEYSPACE ref_simple`

`None | role | cassandra`

```cql
CREATE ROLE IF NOT EXISTS cassandra WITH HASHED PASSWORD = '<salted hash>' AND LOGIN = true AND SUPERUSER = true;
```
`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | grant_permission | cassandra` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS (features enabled)`

`None | service_level | driver` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS`

`None | service_level | driver` — create_statement identical to the one under `DESCRIBE SCHEMA WITH INTERNALS`

### Cassandra 5.0.9

`0x2000` line 1:31 mismatched input 'AND' expecting EOF (DESCRIBE SCHEMA WITH INTERNALS [AND]...)

## `DESCRIBE SCHEMA WITH PASSWORDS` (features enabled)

### ScyllaDB 2026.3.3

`0x2000` line 1:21 : Missing K_INTERNALS

### Cassandra 5.0.9

`0x2000` line 1:21 mismatched input 'PASSWORDS' expecting K_INTERNALS (DESCRIBE SCHEMA WITH [PASSWORDS])
