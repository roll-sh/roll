<!-- Generated from our own observations of the reference images (see conformance.md for the capture method). Do not edit by hand. -->

# Protocol versions, OPTIONS/SUPPORTED and handshake errors

Raw-socket probes (no driver): one frame per fresh connection unless noted. "stream" is the stream id of the response; the request used stream 1. "conn" is the connection state 1.5 s after the response (`open`, `closed`, `reset`).

## SUPPORTED

**ScyllaDB 2026.3.3, OPTIONS v3** (response version byte `0x83`):

```
CLIENT_OPTIONS: [""]
COMPRESSION: ["lz4", "snappy"]
CQL_VERSION: ["3.3.1"]
SCYLLA_HOST_ID: ["<host id, volatile>"]
SCYLLA_LWT_ADD_METADATA_MARK: ["LWT_OPTIMIZATION_META_BIT_MASK=2147483648"]
SCYLLA_NR_SHARDS: ["1"]
SCYLLA_PARTITIONER: ["org.apache.cassandra.dht.Murmur3Partitioner"]
SCYLLA_RATE_LIMIT_ERROR: ["ERROR_CODE=61440"]
SCYLLA_SHARD: ["0"]
SCYLLA_SHARDING_ALGORITHM: ["biased-token-round-robin"]
SCYLLA_SHARDING_IGNORE_MSB: ["12"]
SCYLLA_SHARD_AWARE_PORT: ["19042"]
SCYLLA_USE_METADATA_ID: [""]
TABLETS_ROUTING_V1: [""]
```
**ScyllaDB 2026.3.3, OPTIONS v4** (response version byte `0x84`):

```
CLIENT_OPTIONS: [""]
COMPRESSION: ["lz4", "snappy"]
CQL_VERSION: ["3.3.1"]
SCYLLA_HOST_ID: ["<host id, volatile>"]
SCYLLA_LWT_ADD_METADATA_MARK: ["LWT_OPTIMIZATION_META_BIT_MASK=2147483648"]
SCYLLA_NR_SHARDS: ["1"]
SCYLLA_PARTITIONER: ["org.apache.cassandra.dht.Murmur3Partitioner"]
SCYLLA_RATE_LIMIT_ERROR: ["ERROR_CODE=61440"]
SCYLLA_SHARD: ["0"]
SCYLLA_SHARDING_ALGORITHM: ["biased-token-round-robin"]
SCYLLA_SHARDING_IGNORE_MSB: ["12"]
SCYLLA_SHARD_AWARE_PORT: ["19042"]
SCYLLA_USE_METADATA_ID: [""]
TABLETS_ROUTING_V1: [""]
```
**Cassandra 5.0.9, OPTIONS v3** (response version byte `0x83`):

```
PROTOCOL_VERSIONS: ["3/v3", "4/v4", "5/v5", "6/v6-beta"]
COMPRESSION: ["snappy", "lz4"]
CQL_VERSION: ["3.4.7"]
```
**Cassandra 5.0.9, OPTIONS v4** (response version byte `0x84`):

```
PROTOCOL_VERSIONS: ["3/v3", "4/v4", "5/v5", "6/v6-beta"]
COMPRESSION: ["snappy", "lz4"]
CQL_VERSION: ["3.4.7"]
```
**Cassandra 5.0.9, OPTIONS v5** (response version byte `0x85`):

```
PROTOCOL_VERSIONS: ["3/v3", "4/v4", "5/v5", "6/v6-beta"]
COMPRESSION: ["snappy", "lz4"]
CQL_VERSION: ["3.4.7"]
```
**Cassandra 5.0.9, OPTIONS v6 USE_BETA** (response version byte `0x86`):

```
PROTOCOL_VERSIONS: ["3/v3", "4/v4", "5/v5", "6/v6-beta"]
COMPRESSION: ["snappy", "lz4"]
CQL_VERSION: ["3.4.7"]
```
Volatile: `SCYLLA_HOST_ID` is the node's host id. `SCYLLA_SHARD` is the shard that accepted the connection. Everything else is fixed for the image and flags.

## Accepted protocol versions

| probe | ScyllaDB 2026.3.3 | Cassandra 5.0.9 |
|---|---|---|
| OPTIONS v1 | ERROR, version byte `0x84`, stream 0, `0x000A` Invalid or unsupported protocol version: 1, header `840000000000000030`, conn closed | no response: `ConnectionResetError(104, 'Connection reset by peer')` |
| OPTIONS v2 | ERROR, version byte `0x84`, stream 0, `0x000A` Invalid or unsupported protocol version: 2, header `840000000000000030`, conn closed | no response: `ConnectionResetError(104, 'Connection reset by peer')` |
| OPTIONS v3 | SUPPORTED, version byte `0x83`, stream 1, conn open | SUPPORTED, version byte `0x83`, stream 1, conn open |
| OPTIONS v4 | SUPPORTED, version byte `0x84`, stream 1, conn open | SUPPORTED, version byte `0x84`, stream 1, conn open |
| OPTIONS v5 | ERROR, version byte `0x84`, stream 0, `0x000A` Invalid or unsupported protocol version: 5, header `840000000000000030`, conn closed | SUPPORTED, version byte `0x85`, stream 1, conn open |
| OPTIONS v6 | ERROR, version byte `0x84`, stream 0, `0x000A` Invalid or unsupported protocol version: 6, header `840000000000000030`, conn closed | ERROR, version byte `0x86`, stream 1, `0x000A` Beta version of the protocol used (6/v6-beta), but USE_BETA flag is unset, header `86100001000000004f`, conn open |
| OPTIONS v7 | ERROR, version byte `0x84`, stream 0, `0x000A` Invalid or unsupported protocol version: 7, header `840000000000000030`, conn closed | ERROR, version byte `0x85`, stream 1, `0x000A` Invalid or unsupported protocol version (7); supported versions are (3/v3, 4/v4, 5/v5, 6/v6-beta), header `850000010000000067`, conn open |
| OPTIONS v127 | ERROR, version byte `0x84`, stream 0, `0x000A` Invalid or unsupported protocol version: 127, header `840000000000000032`, conn closed | ERROR, version byte `0x85`, stream 1, `0x000A` Invalid or unsupported protocol version (127); supported versions are (3/v3, 4/v4, 5/v5, 6/v6-beta), header `850000010000000069`, conn open |
| OPTIONS v5 USE_BETA | ERROR, version byte `0x84`, stream 0, `0x000A` Invalid or unsupported protocol version: 5, header `840000000000000030`, conn closed | SUPPORTED, version byte `0x85`, stream 1, conn open |
| OPTIONS v6 USE_BETA | ERROR, version byte `0x84`, stream 0, `0x000A` Invalid or unsupported protocol version: 6, header `840000000000000030`, conn closed | SUPPORTED, version byte `0x86`, stream 1, conn open |
| OPTIONS response-direction byte 0x84 | ERROR, version byte `0x84`, stream 0, `0x000A` Invalid or unsupported protocol version: 132, header `840000000000000032`, conn closed | ERROR, version byte `0x85`, stream 1, `0x000A` Wrong protocol direction (expected REQUEST, got RESPONSE) for opcode 5 (OPTIONS), header `850000010000000056`, conn open |
| STARTUP v2 CQL_VERSION=3.0.0 | ERROR, version byte `0x84`, stream 0, `0x000A` Invalid or unsupported protocol version: 2, header `840000000000000030`, conn closed | ERROR, version byte `0x82`, stream 1, `0x000A` Invalid or unsupported protocol version (2); supported versions are (3/v3, 4/v4, 5/v5, 6/v6-beta), header `8200010000000067`, conn open |
| STARTUP v3 CQL_VERSION=3.0.0 | READY, version byte `0x83`, stream 1, conn open | READY, version byte `0x83`, stream 1, conn open |
| STARTUP v4 CQL_VERSION=3.0.0 | READY, version byte `0x84`, stream 1, conn open | READY, version byte `0x84`, stream 1, conn open |
| STARTUP v5 CQL_VERSION=3.0.0 | ERROR, version byte `0x84`, stream 0, `0x000A` Invalid or unsupported protocol version: 5, header `840000000000000030`, conn closed | READY, version byte `0x85`, stream 1, conn open |
| STARTUP v6 CQL_VERSION=3.0.0 | ERROR, version byte `0x84`, stream 0, `0x000A` Invalid or unsupported protocol version: 6, header `840000000000000030`, conn closed | ERROR, version byte `0x86`, stream 1, `0x000A` Beta version of the protocol used (6/v6-beta), but USE_BETA flag is unset, header `86100001000000004f`, conn open |
| STARTUP v5 USE_BETA | ERROR, version byte `0x84`, stream 0, `0x000A` Invalid or unsupported protocol version: 5, header `840000000000000030`, conn closed | READY, version byte `0x85`, stream 1, conn open |
| STARTUP v6 USE_BETA | ERROR, version byte `0x84`, stream 0, `0x000A` Invalid or unsupported protocol version: 6, header `840000000000000030`, conn closed | READY, version byte `0x86`, stream 1, conn open |
| STARTUP v4 empty map | READY, version byte `0x84`, stream 1, conn open | ERROR, version byte `0x84`, stream 1, `0x000A` Missing value CQL_VERSION in STARTUP message, header `840000010000000032`, conn open |
| STARTUP v4 CQL_VERSION=2.0.0 | READY, version byte `0x84`, stream 1, conn open | ERROR, version byte `0x84`, stream 1, `0x000A` CQL version 2.0.0 is not supported by the binary protocol (supported version are >= 3.0.0), header `840000010000000060`, conn open |
| STARTUP v4 CQL_VERSION=3.4.7 | READY, version byte `0x84`, stream 1, conn open | READY, version byte `0x84`, stream 1, conn open |
| STARTUP v4 CQL_VERSION=abc | READY, version byte `0x84`, stream 1, conn open | ERROR, version byte `0x84`, stream 1, `0x000A` Invalid version value: abc, header `840000010000000020`, conn open |
| STARTUP v4 COMPRESSION=zstd | ERROR, version byte `0x84`, stream 1, `0x000A` Unknown compression algorithm: zstd, header `840000010000000029`, conn open | ERROR, version byte `0x84`, stream 1, `0x000A` Unknown compression algorithm: zstd, header `840000010000000029`, conn open |
| STARTUP v4 COMPRESSION=lz4 | READY, version byte `0x84`, stream 1, conn open | READY, version byte `0x84`, stream 1, conn open |
| STARTUP v4 COMPRESSION=snappy | READY, version byte `0x84`, stream 1, conn open | READY, version byte `0x84`, stream 1, conn open |
| STARTUP v4 unknown option | READY, version byte `0x84`, stream 1, conn open | READY, version byte `0x84`, stream 1, conn open |
| STARTUP v4 DRIVER_NAME etc | READY, version byte `0x84`, stream 1, conn open | READY, version byte `0x84`, stream 1, conn open |
| STARTUP v4 compressed flag set | ERROR, version byte `0x84`, stream 0, `0x000A` Unknown compression algorithm, header `840000000000000023`, conn closed | READY, version byte `0x84`, stream 1, conn open |
| STARTUP v4 truncated body | ERROR, version byte `0x84`, stream 1, `0x000A` truncated frame: expected 2 bytes, length is 0, header `840000010000000034`, conn open | no response: `ConnectionResetError(104, 'Connection reset by peer')` |
| QUERY v4 before STARTUP | ERROR, version byte `0x84`, stream 1, `0x000A` Unexpected message 7, expecting STARTUP or OPTIONS, header `840000010000000038`, conn open | ERROR, version byte `0x84`, stream 1, `0x000A` Unexpected message QUERY, expecting STARTUP or OPTIONS, header `84000001000000003c`, conn reset:ConnectionResetError |
| unknown opcode 0x2a v4 | ERROR, version byte `0x84`, stream 1, `0x000A` Unexpected message 42, expecting STARTUP or OPTIONS, header `840000010000000039`, conn open | ERROR, version byte `0x85`, stream 1, `0x000A` Unknown opcode 42, header `850000010000000017`, conn open |
| REGISTER v4 before STARTUP | ERROR, version byte `0x84`, stream 1, `0x000A` Unexpected message 11, expecting STARTUP or OPTIONS, header `840000010000000039`, conn open | ERROR, version byte `0x84`, stream 1, `0x000A` Unexpected message REGISTER, expecting STARTUP or OPTIONS, header `84000001000000003f`, conn reset:ConnectionResetError |
| AUTH_RESPONSE v4 before STARTUP | ERROR, version byte `0x84`, stream 1, `0x000A` Unexpected message 15, expecting STARTUP or OPTIONS, header `840000010000000039`, conn open | ERROR, version byte `0x84`, stream 1, `0x000A` Unexpected message AUTH_RESPONSE, expecting STARTUP or OPTIONS, header `840000010000000044`, conn reset:ConnectionResetError |
| response opcode READY sent by client | ERROR, version byte `0x84`, stream 1, `0x000A` Unexpected message 2, expecting STARTUP or OPTIONS, header `840000010000000038`, conn open | ERROR, version byte `0x85`, stream 1, `0x000A` Wrong protocol direction (expected RESPONSE, got REQUEST) for opcode 2 (READY), header `850000010000000054`, conn open |
| frame length 300MB | ERROR, version byte `0x84`, stream 0, `0x000A` Initial message size too large (314572800), rejecting as invalid, header `840000000000000046`, conn closed | no response: `TimeoutError('timed out')` |
| negative stream id OPTIONS | SUPPORTED, version byte `0x84`, stream -5, conn open | SUPPORTED, version byte `0x84`, stream -5, conn open |
| post-STARTUP v4: STARTUP | READY, version byte `0x84`, stream 1 | READY, version byte `0x84`, stream 1 |
| post-STARTUP v4: QUERY ok system.local | RESULT, version byte `0x84`, stream 1 | RESULT, version byte `0x84`, stream 1 |
| post-STARTUP v4: EXECUTE unknown id | ERROR, version byte `0x84`, stream 1, `0x2500` No prepared statement with ID deadbeef found., header `840000010000000039` | ERROR, version byte `0x84`, stream 1, `0x2500` Prepared query with ID deadbeef not found (either the query was not prepared on this host (maybe the host has been restarted?) or you have prepared too many queries and it has been evicted from the internal cache), header `8400000100000000e1` |
| post-STARTUP v4: QUERY syntax error | ERROR, version byte `0x84`, stream 1, `0x2000` line 1:0 no viable alternative at input 'SELEC', header `840000010000000035` | ERROR, version byte `0x84`, stream 1, `0x2000` line 1:0 no viable alternative at input 'SELEC' ([SELEC]...), header `840000010000000042` |
| post-STARTUP v4: QUERY unknown keyspace | ERROR, version byte `0x84`, stream 1, `0x2200` Keyspace nope does not exist, header `840000010000000022` | ERROR, version byte `0x84`, stream 1, `0x2200` keyspace nope does not exist, header `840000010000000022` |
| post-STARTUP v4: QUERY consistency 0x0042 | ERROR, version byte `0x84`, stream 1, `0x000A` Unknown code 66 for a consistency level, header `84000001000000002d` | ERROR, version byte `0x84`, stream 1, `0x000A` Unknown code 66 for a consistency level, header `84000001000000002d` |
| post-STARTUP v4: QUERY consistency SERIAL on read | ERROR, version byte `0x84`, stream 1, `0x2200` SERIAL/LOCAL_SERIAL consistency may only be requested for one partition at a time, header `840000010000000057` | RESULT, version byte `0x84`, stream 1 |
| post-STARTUP v4: QUERY consistency ANY on read | ERROR, version byte `0x84`, stream 1, `0x2200` ANY ConsistencyLevel is only supported for writes, header `840000010000000037` | ERROR, version byte `0x84`, stream 1, `0x2200` ANY ConsistencyLevel is only supported for writes, header `840000010000000037` |
| post-STARTUP v4: QUERY page_size 0 | RESULT, version byte `0x84`, stream 1 | ERROR, version byte `0x84`, stream 1, `0x000A` The page size cannot be 0, header `84000001000000001f` |
| post-STARTUP v4: QUERY page_size -1 | RESULT, version byte `0x84`, stream 1 | RESULT, version byte `0x84`, stream 1 |
| post-STARTUP v4: QUERY bogus paging state | ERROR, version byte `0x84`, stream 1, `0x000A` Invalid value for the paging state, header `840000010000000028` | ERROR, version byte `0x84`, stream 1, `0x000A` Invalid value for the paging state, header `840000010000000028` |
| post-STARTUP v4: QUERY with tracing | RESULT, version byte `0x84`, stream 1 | RESULT, version byte `0x84`, stream 1 |
| post-STARTUP v4: QUERY values but no markers | ERROR, version byte `0x84`, stream 1, `0x2200` Invalid amount of bind variables: expected 0 received 1, header `84000001000000003d` | ERROR, version byte `0x84`, stream 1, `0x2200` Invalid amount of bind variables, header `840000010000000026` |
| post-STARTUP v4: QUERY unknown flag bits 0x80 | ERROR, version byte `0x84`, stream 1, `0x0000` Bit mask contains invalid enumeration indices., header `840000010000000034` | ERROR, version byte `0x84`, stream 1, `0x000A` Not enough bytes to read an UTF8 serialized string preceded by its 2 bytes length, header `840000010000000057` |
| post-STARTUP v4: PREPARE syntax error | ERROR, version byte `0x84`, stream 1, `0x2000` line 1:0 no viable alternative at input 'SELEC', header `840000010000000035` | ERROR, version byte `0x84`, stream 1, `0x2000` line 1:0 no viable alternative at input 'SELEC' ([SELEC]...), header `840000010000000042` |
| post-STARTUP v4: PREPARE ok | RESULT, version byte `0x84`, stream 1 | RESULT, version byte `0x84`, stream 1 |
| post-STARTUP v4: BATCH empty logged | RESULT, version byte `0x84`, stream 1 | RESULT, version byte `0x84`, stream 1 |
| post-STARTUP v4: BATCH type 7 | RESULT, version byte `0x84`, stream 1 | ERROR, version byte `0x84`, stream 1, `0x000A` Invalid BATCH message type 7, header `840000010000000022` |
| post-STARTUP v4: BATCH with SELECT | ERROR, version byte `0x84`, stream 1, `0x2200` Invalid statement in batch: only UPDATE, INSERT and DELETE statements are allowed., header `840000010000000058` | ERROR, version byte `0x84`, stream 1, `0x2200` Invalid statement in batch: only UPDATE, INSERT and DELETE statements are allowed., header `840000010000000058` |
| post-STARTUP v4: STARTUP again | ERROR, version byte `0x84`, stream 1, `0x000A` Unexpected message STARTUP, the connection is already initialized, header `840000010000000047` | ERROR, version byte `0x84`, stream 1, `0x000A` Unexpected message STARTUP, the connection is already initialized, header `840000010000000047` |
| post-STARTUP v4: OPTIONS after STARTUP | SUPPORTED, version byte `0x84`, stream 1 | SUPPORTED, version byte `0x84`, stream 1 |
| post-STARTUP v4: REGISTER unknown event | ERROR, version byte `0x84`, stream 1, `0x000A` Invalid value 'NOPE' for Event.Type, header `840000010000000029` | ERROR, version byte `0x84`, stream 1, `0x000A` Invalid value 'NOPE' for Type, header `840000010000000023` |
| post-STARTUP v4: REGISTER all events | READY, version byte `0x84`, stream 1 | READY, version byte `0x84`, stream 1 |
| post-STARTUP v4: AUTH_RESPONSE after READY | ERROR, version byte `0x84`, stream 1, `0x0000` Should not reach, header `840000010000000016` | AUTH_SUCCESS, version byte `0x84`, stream 1 |
| post-STARTUP v4: truncated QUERY body | ERROR, version byte `0x84`, stream 1, `0x000A` truncated frame: expected 4 bytes, length is 2, header `840000010000000034` | ERROR, version byte `0x84`, stream 1, `0x000A` Not enough bytes to read an UTF8 serialized string preceded by its 4 bytes length, header `840000010000000057` |
| post-STARTUP v3: STARTUP | READY, version byte `0x83`, stream 1 | READY, version byte `0x83`, stream 1 |
| post-STARTUP v3: QUERY ok system.local | RESULT, version byte `0x83`, stream 1 | RESULT, version byte `0x83`, stream 1 |
| post-STARTUP v3: EXECUTE unknown id | ERROR, version byte `0x83`, stream 1, `0x2500` No prepared statement with ID deadbeef found., header `830000010000000039` | ERROR, version byte `0x83`, stream 1, `0x2500` Prepared query with ID deadbeef not found (either the query was not prepared on this host (maybe the host has been restarted?) or you have prepared too many queries and it has been evicted from the internal cache), header `8300000100000000e1` |
| post-STARTUP v3: QUERY syntax error | ERROR, version byte `0x83`, stream 1, `0x2000` line 1:0 no viable alternative at input 'SELEC', header `830000010000000035` | ERROR, version byte `0x83`, stream 1, `0x2000` line 1:0 no viable alternative at input 'SELEC' ([SELEC]...), header `830000010000000042` |
| post-STARTUP v3: QUERY unknown keyspace | ERROR, version byte `0x83`, stream 1, `0x2200` Keyspace nope does not exist, header `830000010000000022` | ERROR, version byte `0x83`, stream 1, `0x2200` keyspace nope does not exist, header `830000010000000022` |

Protocol v1/v2 probes with alternative framings:

| probe | ScyllaDB 2026.3.3 | Cassandra 5.0.9 |
|---|---|---|
| OPTIONS v1 9-byte header | ERROR, version byte `0x84`, stream 0, `0x000A` Invalid or unsupported protocol version: 1, header `840000000000000030`, conn closed | ERROR, version byte `0x81`, stream 0, `0x000A` Invalid or unsupported protocol version (1); supported versions are (3/v3, 4/v4, 5/v5, 6/v6-beta), header `8100000000000067`, conn open |
| OPTIONS v1 8-byte header + 1 pad byte | ERROR, version byte `0x84`, stream 0, `0x000A` Invalid or unsupported protocol version: 1, header `840000000000000030`, conn closed | ERROR, version byte `0x81`, stream 1, `0x000A` Invalid or unsupported protocol version (1); supported versions are (3/v3, 4/v4, 5/v5, 6/v6-beta), header `8100010000000067`, conn open |
| STARTUP v1 8-byte header | ERROR, version byte `0x84`, stream 0, `0x000A` Invalid or unsupported protocol version: 1, header `840000000000000030`, conn closed | ERROR, version byte `0x81`, stream 1, `0x000A` Invalid or unsupported protocol version (1); supported versions are (3/v3, 4/v4, 5/v5, 6/v6-beta), header `8100010000000067`, conn open |
| OPTIONS v2 9-byte header | ERROR, version byte `0x84`, stream 0, `0x000A` Invalid or unsupported protocol version: 2, header `840000000000000030`, conn closed | ERROR, version byte `0x82`, stream 0, `0x000A` Invalid or unsupported protocol version (2); supported versions are (3/v3, 4/v4, 5/v5, 6/v6-beta), header `8200000000000067`, conn open |
| OPTIONS v2 8-byte header + 1 pad byte | ERROR, version byte `0x84`, stream 0, `0x000A` Invalid or unsupported protocol version: 2, header `840000000000000030`, conn closed | ERROR, version byte `0x82`, stream 1, `0x000A` Invalid or unsupported protocol version (2); supported versions are (3/v3, 4/v4, 5/v5, 6/v6-beta), header `8200010000000067`, conn open |
| STARTUP v2 8-byte header | ERROR, version byte `0x84`, stream 0, `0x000A` Invalid or unsupported protocol version: 2, header `840000000000000030`, conn closed | ERROR, version byte `0x82`, stream 1, `0x000A` Invalid or unsupported protocol version (2); supported versions are (3/v3, 4/v4, 5/v5, 6/v6-beta), header `8200010000000067`, conn open |
| first byte only 0x01 | ERROR, version byte `0x84`, stream 0, `0x000A` Invalid or unsupported protocol version: 1, header `840000000000000030`, conn closed | no response: `ConnectionResetError(104, 'Connection reset by peer')` |

Notes:

- ScyllaDB answers every unsupported version with a v4-framed ERROR (`0x84`, 9-byte header) on stream 0 and closes the connection. Cassandra answers in the client's framing (v1/v2 use the 8-byte header, version byte `0x81`/`0x82`; v6 without USE_BETA and v7+ get `0x85`/`0x86` frames), echoes the stream id when it could parse one, and keeps the connection open after a version error.
- Cassandra 5.0 accepts v5 without the beta flag and v6 only with USE_BETA (`0x10`).
- ScyllaDB ignores the CQL_VERSION value in STARTUP (missing, `2.0.0` and `abc` are all accepted); Cassandra validates it.
