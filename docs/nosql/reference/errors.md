<!-- Generated from our own observations of the reference images (see conformance.md for the capture method). Do not edit by hand. -->

# Errors and warnings

756 cases.

Every statement was sent as a QUERY over a raw protocol v4 connection (consistency ONE, page size 5000 unless noted) to the default-configured reference, so codes, messages, extra error fields and warnings are exactly what the server sent. The prepared-statement section uses the Python driver. Results appear in execution order: earlier statements in a section can change later ones (for example ALTERs). Long statements are abbreviated as `… (N chars)`.

Error codes: `0x0000` server, `0x000A` protocol, `0x0100` bad credentials, `0x1000` unavailable, `0x1100` write timeout, `0x1200` read timeout, `0x1300` read failure, `0x1400` function failure, `0x2000` syntax, `0x2100` unauthorized, `0x2200` invalid, `0x2300` config, `0x2400` already exists, `0x2500` unprepared.

Setup (both dialects): keyspace `e` (NetworkTopologyStrategy datacenter1: 1; tablets on in ScyllaDB), `e.t (pk int, ck int, v int, s int static, txt text, l list<int>, st set<int>, m map<int, int>, fl frozen<list<int>>, fm frozen<map<int, int>>, tu tuple<int, text>, PRIMARY KEY (pk, ck))`, `e.t2 (a, b, c, d, v int, PRIMARY KEY ((a, b), c, d))`, `e.c (pk int PRIMARY KEY, cnt counter)`, `e.u` (one column of every native type), `e.ix` with index `ix_v` on `v`, `e.uses (pk int PRIMARY KEY, t frozen<ty>, nt ty)` with `TYPE e.ty (x int, y text)`, `e.vec (pk int PRIMARY KEY, emb vector<float, 3>)`, `e.blobk (pk blob PRIMARY KEY)`, `e.textk (pk text, ck text, …)`, `e.big`/`e.tomb (pk int, ck int, v, PRIMARY KEY (pk, ck))`; one row `(1, 1, v=1, txt='a', l=[1, 2])` in `e.t`.

## SELECT restrictions and selectors

| id | statement | ScyllaDB 2026.3.3 | Cassandra 5.0.9 |
|---|---|---|---|
| unknown_keyspace | `SELECT * FROM nope.t` | `0x2200` Keyspace nope does not exist | `0x2200` keyspace nope does not exist |
| unknown_table | `SELECT * FROM e.nope` | `0x2200` unconfigured table nope | `0x2200` table nope does not exist |
| unknown_column_select | `SELECT nope FROM e.t` | `0x2200` Unrecognized name nope | `0x2200` Undefined column name nope in table e.t |
| unknown_column_where | `SELECT * FROM e.t WHERE nope = 1` | `0x2200` Unrecognized name nope | `0x2200` Undefined column name nope in table e.t |
| no_keyspace | `SELECT * FROM t` | `0x2200` No keyspace has been specified. USE a keyspace, or explicitly specify keyspace.tablename | (same) |
| filtering_regular | `SELECT * FROM e.t WHERE v = 1` | `0x2200` Cannot execute this query as it might involve data filtering and thus may have unpredictable performance. If you want to execute this query despite the performance unpredictability, use ALLOW FILTERING | (same) |
| filtering_clustering_only | `SELECT * FROM e.t WHERE ck = 1` | OK Rows; warnings: "This query should use ALLOW FILTERING and will be rejected in future versions." | `0x2200` Cannot execute this query as it might involve data filtering and thus may have unpredictable performance. If you want to execute this query despite the performance unpredictability, use ALLOW FILTERING |
| filtering_clustering_slice_only | `SELECT * FROM e.t WHERE ck > 1` | OK Rows; warnings: "This query should use ALLOW FILTERING and will be rejected in future versions." | `0x2200` Cannot execute this query as it might involve data filtering and thus may have unpredictable performance. If you want to execute this query despite the performance unpredictability, use ALLOW FILTERING |
| filtering_with_pk_regular | `SELECT * FROM e.t WHERE pk = 1 AND v = 1` | `0x2200` Cannot execute this query as it might involve data filtering and thus may have unpredictable performance. If you want to execute this query despite the performance unpredictability, use ALLOW FILTERING | (same) |
| filtering_static | `SELECT * FROM e.t WHERE s = 1` | `0x2200` Cannot execute this query as it might involve data filtering and thus may have unpredictable performance. If you want to execute this query despite the performance unpredictability, use ALLOW FILTERING | (same) |
| filtering_ok | `SELECT * FROM e.t WHERE v = 1 ALLOW FILTERING` | OK Rows | (same) |
| partial_partition_key | `SELECT * FROM e.t2 WHERE a = 1` | `0x2200` Cannot execute this query as it might involve data filtering and thus may have unpredictable performance. If you want to execute this query despite the performance unpredictability, use ALLOW FILTERING | (same) |
| partial_partition_key_filtering | `SELECT * FROM e.t2 WHERE a = 1 ALLOW FILTERING` | OK Rows | (same) |
| clustering_skip | `SELECT * FROM e.t2 WHERE a = 1 AND b = 1 AND d = 1` | `0x2200` PRIMARY KEY column "d" cannot be restricted as preceding column "c" is not restricted | (same) |
| clustering_after_slice | `SELECT * FROM e.t2 WHERE a = 1 AND b = 1 AND c > 1 AND d = 1` | `0x2200` Clustering column "d" cannot be restricted (preceding column "c" is restricted by a non-EQ relation) | (same) |
| clustering_in_then_slice | `SELECT * FROM e.t2 WHERE a = 1 AND b = 1 AND c IN (1, 2) AND d > 1` | OK Rows | (same) |
| partition_key_slice | `SELECT * FROM e.t WHERE pk > 1` | `0x2200` Only EQ and IN relation are supported on the partition key (unless you use the token() function or ALLOW FILTERING) | `0x2200` Cannot execute this query as it might involve data filtering and thus may have unpredictable performance. If you want to execute this query despite the performance unpredictability, use ALLOW FILTERING |
| partition_key_slice_filtering | `SELECT * FROM e.t WHERE pk > 1 ALLOW FILTERING` | OK Rows | (same) |
| partition_key_neq | `SELECT * FROM e.t WHERE pk != 1` | `0x2200` Unsupported "!=" relation: pk != 1 | (same) |
| neq_regular | `SELECT * FROM e.t WHERE pk = 1 AND v != 1 ALLOW FILTERING` | `0x2200` Unsupported "!=" relation: v != 1 | (same) |
| token_and_eq | `SELECT * FROM e.t WHERE token(pk) > 0 AND pk = 1` | `0x2200` Columns "ColumnDefinition{name=pk, type=org.apache.cassandra.db.marshal.Int32Type, kind=PARTITION_KEY, componentIndex=0, droppedAt=-9223372036854775808}" cannot be restricted by both a normal relation and a token relation | OK Rows |
| token_on_clustering | `SELECT * FROM e.t WHERE token(ck) > 0` | `0x2200` The token function arguments must be in the partition key order: pk | `0x2200` The token() function must be applied to all partition key components or none of them |
| token_partial | `SELECT * FROM e.t2 WHERE token(a) > 0` | `0x2200` Invalid number of arguments in call to function system.token: 2 required but 1 provided | `0x2200` The token() function must be applied to all partition key components or none of them |
| token_wrong_order | `SELECT * FROM e.t2 WHERE token(b, a) > 0` | `0x2200` The token function arguments must be in the partition key order: a, b | (same) |
| token_eq_literal_type | `SELECT * FROM e.t WHERE token(pk) = 'a'` | `0x2200` Invalid STRING constant (a) for "partition key token" of type bigint | (same) |
| multiple_eq_same_column | `SELECT * FROM e.t WHERE pk = 1 AND ck = 1 AND ck = 2` | OK Rows | `0x2200` ck cannot be restricted by more than one relation if it includes an Equal |
| eq_and_in_same_column | `SELECT * FROM e.t WHERE pk = 1 AND pk IN (1, 2)` | OK Rows | `0x2200` pk cannot be restricted by more than one relation if it includes an Equal |
| two_lower_bounds | `SELECT * FROM e.t WHERE pk = 1 AND ck > 1 AND ck >= 2` | OK Rows | `0x2200` More than one restriction was found for the start bound on ck |
| multi_column_non_clustering | `SELECT * FROM e.t WHERE pk = 1 AND (ck, v) > (1, 1)` | `0x2200` Multi-column relations can only be applied to clustering columns but was applied to: v | (same) |
| multi_column_arity | `SELECT * FROM e.t2 WHERE a = 1 AND b = 1 AND (c, d) = (1)` | `0x2200` Expected 2 elements in value tuple, but got 1: (1) | (same) |
| multi_column_out_of_order | `SELECT * FROM e.t2 WHERE a = 1 AND b = 1 AND (d, c) = (1, 1)` | `0x2200` Clustering columns may not be skipped in multi-column relations. They should appear in the PRIMARY KEY order | `0x2200` Clustering columns must appear in the PRIMARY KEY order in multi-column relations: (d, c) = (1, 1) |
| multi_and_single_mix | `SELECT * FROM e.t2 WHERE a = 1 AND b = 1 AND (c, d) > (1, 1) AND c < 5` | `0x2200` Mixing single column relations and multi column relations on clustering columns is not allowed | OK Rows |
| contains_no_index | `SELECT * FROM e.t WHERE pk = 1 AND l CONTAINS 1` | `0x2200` Cannot execute this query as it might involve data filtering and thus may have unpredictable performance. If you want to execute this query despite the performance unpredictability, use ALLOW FILTERING | (same) |
| contains_non_collection | `SELECT * FROM e.t WHERE pk = 1 AND v CONTAINS 1 ALLOW FILTERING` | `0x2200` Cannot use CONTAINS on non-collection column "v" | `0x2200` Cannot use CONTAINS on non-collection column v |
| contains_key_on_list | `SELECT * FROM e.t WHERE pk = 1 AND l CONTAINS KEY 1 ALLOW FILTERING` | `0x2200` Cannot use CONTAINS KEY on non-map column l | (same) |
| contains_frozen_filtering | `SELECT * FROM e.t WHERE pk = 1 AND fl CONTAINS 1 ALLOW FILTERING` | OK Rows | (same) |
| map_entry_no_filtering | `SELECT * FROM e.t WHERE pk = 1 AND m[1] = 2` | `0x2200` Cannot execute this query as it might involve data filtering and thus may have unpredictable performance. If you want to execute this query despite the performance unpredictability, use ALLOW FILTERING | (same) |
| map_entry_filtering | `SELECT * FROM e.t WHERE pk = 1 AND m[1] = 2 ALLOW FILTERING` | OK Rows | (same) |
| like_no_index | `SELECT * FROM e.t WHERE pk = 1 AND txt LIKE 'a%'` | `0x2200` Cannot execute this query as it might involve data filtering and thus may have unpredictable performance. If you want to execute this query despite the performance unpredictability, use ALLOW FILTERING | `0x2200` LIKE restriction is only supported on properly indexed columns. txt LIKE 'a%' is not valid. |
| like_filtering | `SELECT * FROM e.t WHERE pk = 1 AND txt LIKE 'a%' ALLOW FILTERING` | OK Rows | `0x2200` LIKE restriction is only supported on properly indexed columns. txt LIKE 'a%' is not valid. |
| is_not_null_select | `SELECT * FROM e.t WHERE v IS NOT NULL ALLOW FILTERING` | `0x2200` restriction 'v IS NOT null' is only supported in materialized view creation | `0x2200` Unsupported restriction: v IS NOT NULL |
| is_null_select | `SELECT * FROM e.t WHERE v IS NULL ALLOW FILTERING` | `0x2000` line 1:29 : Missing K_NOT | `0x2000` line 1:29 mismatched input 'NULL' expecting K_NOT (...e.t WHERE v IS [NULL]...) |
| in_on_regular | `SELECT * FROM e.t WHERE pk = 1 AND v IN (1, 2) ALLOW FILTERING` | OK Rows | (same) |
| in_empty | `SELECT * FROM e.t WHERE pk IN ()` | OK Rows | (same) |
| in_null_element | `SELECT * FROM e.t WHERE pk IN (1, null)` | OK Rows | `0x2200` Invalid null value in condition for column pk |
| in_collection_column | `SELECT * FROM e.t WHERE pk = 1 AND l IN ([1]) ALLOW FILTERING` | `0x2200` Collection column 'l' (list<int>) cannot be restricted by a 'IN' relation | (same) |
| order_by_no_pk | `SELECT * FROM e.t ORDER BY ck DESC` | `0x2200` ORDER BY is only supported when the partition key is restricted by an EQ or an IN. | (same) |
| order_by_regular | `SELECT * FROM e.t WHERE pk = 1 ORDER BY v` | `0x2200` Order by is currently only supported on the clustered columns of the PRIMARY KEY, got v | (same) |
| order_by_pk | `SELECT * FROM e.t WHERE pk = 1 ORDER BY pk` | `0x2200` Order by is currently only supported on the clustered columns of the PRIMARY KEY, got pk | (same) |
| order_by_skip | `SELECT * FROM e.t2 WHERE a = 1 AND b = 1 ORDER BY d` | `0x2200` Unsupported order by relation - column c doesn't have an ordering or EQ relation. | `0x2200` Order by currently only supports the ordering of columns following their declared order in the PRIMARY KEY |
| order_by_mixed | `SELECT * FROM e.t2 WHERE a = 1 AND b = 1 ORDER BY c ASC, d DESC` | `0x2200` Unsupported order by relation - only reversing all columns is supported, but column d has opposite ordering | `0x2200` Unsupported order by relation |
| order_by_unknown | `SELECT * FROM e.t WHERE pk = 1 ORDER BY nope` | `0x2200` Order by on unknown column nope | `0x2200` Undefined column name nope in table e.t |
| order_by_twice | `SELECT * FROM e.t WHERE pk = 1 ORDER BY ck, ck` | `0x2200` Order by currently only supports the ordering of columns following their declared order in the PRIMARY KEY | OK Rows |
| order_by_secondary_index | `SELECT * FROM e.ix WHERE v = 1 ORDER BY ck DESC` | `0x2200` ORDER BY with 2ndary indexes is not supported. | `0x2200` ORDER BY with 2ndary indexes is not supported, except for ANN queries. |
| in_order_by_paged | `SELECT * FROM e.t WHERE pk IN (1, 2) ORDER BY ck DESC` | `0x2200` Cannot page queries with both ORDER BY and a IN restriction on the partition key; you must either remove the ORDER BY or the IN and sort client side, or disable paging for this query | (same) |
| in_order_by_unpaged | `SELECT * FROM e.t WHERE pk IN (1, 2) ORDER BY ck DESC` | OK Rows | (same) |
| limit_zero | `SELECT * FROM e.t LIMIT 0` | `0x2200` LIMIT must be strictly positive | (same) |
| limit_negative | `SELECT * FROM e.t LIMIT -1` | `0x2200` LIMIT must be strictly positive | (same) |
| per_partition_limit_zero | `SELECT * FROM e.t PER PARTITION LIMIT 0` | `0x2200` PER PARTITION LIMIT must be strictly positive | `0x2200` LIMIT must be strictly positive |
| per_partition_limit_distinct | `SELECT DISTINCT pk FROM e.t PER PARTITION LIMIT 1` | `0x2200` PER PARTITION LIMIT is not allowed with SELECT DISTINCT queries | (same) |
| distinct_regular | `SELECT DISTINCT v FROM e.t` | `0x2200` SELECT DISTINCT queries must only request partition key columns and/or static columns (not v) | (same) |
| distinct_clustering | `SELECT DISTINCT pk, ck FROM e.t` | `0x2200` SELECT DISTINCT queries must only request partition key columns and/or static columns (not ck) | (same) |
| distinct_partial_pk | `SELECT DISTINCT a FROM e.t2` | `0x2200` SELECT DISTINCT queries must request all the partition key columns (missing b) | (same) |
| distinct_with_clustering_restriction | `SELECT DISTINCT pk FROM e.t WHERE ck = 1 ALLOW FILTERING` | `0x2200` SELECT DISTINCT with WHERE clause only supports restriction by partition key. | `0x2200` SELECT DISTINCT with WHERE clause only supports restriction by partition key and/or static columns. |
| group_by_regular | `SELECT * FROM e.t WHERE pk = 1 GROUP BY v` | `0x2200` Group by non-primary-key column v | `0x2200` Group by is currently only supported on the columns of the PRIMARY KEY, got v |
| group_by_order | `SELECT * FROM e.t2 WHERE a = 1 AND b = 1 GROUP BY a, b, d` | `0x2200` Group by column d is out of order | `0x2200` Group by currently only support groups of columns following their declared order in the PRIMARY KEY |
| group_by_partial_pk | `SELECT a, count(*) FROM e.t2 GROUP BY a` | `0x2200` GROUP BY must include the entire partition key | `0x2200` Group by is not supported on only a part of the partition key |
| group_by_unknown | `SELECT * FROM e.t GROUP BY nope` | `0x2200` Group by unknown column nope | `0x2200` Undefined column name nope in table e.t |
| unknown_function | `SELECT nofunc(v) FROM e.t` | `0x2200` Unknown function nofunc called | `0x2200` Unknown function 'nofunc' |
| aggregate_wrong_type | `SELECT sum(txt) FROM e.t WHERE pk = 1` | `0x2200` Invalid call to function sum, none of its type signatures match (known type signatures: system.sum, system.sum, system.sum, system.sum, system.sum, system.sum, system.sum, system.sum) | `0x2200` Invalid call to function sum, none of its type signatures match (known type signatures: system.sum : (float) -> float, system.sum : (varint) -> varint, system.sum : (bigint) -> bigint, system.sum : (decimal) -> decimal, system.sum : (int) -> int, system.sum : (smallint) -> smallint, system.sum : (double) -> double, system.sum : (tinyint) -> tinyint, system.sum : (counter) -> counter) |
| function_wrong_arity | `SELECT blobAsInt(v, v) FROM e.t WHERE pk = 1` | `0x2200` Invalid number of arguments in call to function system.blobasint: 1 required but 2 provided | (same) |
| function_wrong_arg_type | `SELECT blobAsInt(v) FROM e.t WHERE pk = 1` | OK Rows | (same) |
| function_in_where | `SELECT * FROM e.t WHERE pk = 1 AND ck = blobAsInt(0x00000001)` | OK Rows | (same) |
| aggregate_in_where | `SELECT * FROM e.t WHERE pk = 1 AND ck = max(ck)` | `0x2000` line 1:46 no viable alternative at input ')' | `0x2000` line 1:46 no viable alternative at input ')' (...1 AND ck = max([ck])) |
| aggregate_mixed_with_column | `SELECT v, count(*) FROM e.t WHERE pk = 1` | OK Rows | (same) |
| nested_aggregate | `SELECT max(count(*)) FROM e.t` | `0x2200` SELECT clause contains aggeregation of an aggregation | `0x2200` aggregate functions cannot be used as arguments of aggregate functions |
| ttl_on_pk | `SELECT ttl(pk) FROM e.t` | `0x2200` TTL is not legal on partition key component pk | `0x2200` Cannot use selection function ttl on PRIMARY KEY part pk |
| writetime_on_ck | `SELECT writetime(ck) FROM e.t` | `0x2200` WRITETIME is not legal on partition key component ck | `0x2200` Cannot use selection function writetime on PRIMARY KEY part ck |
| writetime_on_collection | `SELECT writetime(l) FROM e.t WHERE pk = 1` | `0x2200` WRITETIME expects an atomic column, but l is a non-frozen collection | OK Rows |
| writetime_on_frozen | `SELECT writetime(fl) FROM e.t WHERE pk = 1` | OK Rows | (same) |
| maxwritetime_on_collection | `SELECT maxwritetime(l) FROM e.t WHERE pk = 1` | `0x2200` Unknown function maxwritetime called | OK Rows |
| ttl_on_counter | `SELECT ttl(cnt) FROM e.c` | OK Rows | (same) |
| writetime_on_function | `SELECT writetime(blobAsInt(0x00000001)) FROM e.t` | `0x2000` line 1:26 : Missing ')' | `0x2000` line 1:26 no viable alternative at input '(' (SELECT writetime(blobAsInt[(]...) |
| cast_invalid | `SELECT CAST(l AS text) FROM e.t WHERE pk = 1` | `0x2200` org.apache.cassandra.db.marshal.ListType(org.apache.cassandra.db.marshal.Int32Type) cannot be cast to org.apache.cassandra.db.marshal.UTF8Type | `0x2200` Invalid call to function system.cast_as_text, none of its type signatures match (known type signatures: system.cast_as_text : (boolean) -> text, system.cast_as_text : (uuid) -> text, system.cast_as_text : (int) -> text, system.cast_as_text : (counter) -> text, system.cast_as_text : (float) -> text, system.cast_as_text : (date) -> text, system.cast_as_text : (smallint) -> text, system.cast_as_text … (781 chars) |
| cast_unknown_type | `SELECT CAST(v AS nope) FROM e.t WHERE pk = 1` | `0x2000` line 1:17 no viable alternative at input 'nope' | `0x2000` line 1:17 no viable alternative at input 'nope' (SELECT CAST(v AS [nope]...) |
| cast_text_to_int | `SELECT CAST(txt AS int) FROM e.t WHERE pk = 1` | `0x2200` org.apache.cassandra.db.marshal.UTF8Type cannot be cast to org.apache.cassandra.db.marshal.Int32Type | `0x2200` Invalid call to function system.cast_as_int, none of its type signatures match (known type signatures: system.cast_as_int : (bigint) -> int, system.cast_as_int : (float) -> int, system.cast_as_int : (smallint) -> int, system.cast_as_int : (decimal) -> int, system.cast_as_int : (double) -> int, system.cast_as_int : (varint) -> int, system.cast_as_int : (tinyint) -> int, system.cast_as_int : (counte… (410 chars) |
| json_and_distinct | `SELECT JSON DISTINCT pk FROM e.t` | OK Rows | (same) |
| json_unknown | `SELECT JSON nope FROM e.t` | `0x2200` Unrecognized name nope | `0x2200` Undefined column name nope in table e.t |
| alias_in_where | `SELECT v AS x FROM e.t WHERE x = 1 ALLOW FILTERING` | `0x2200` Aliases aren't allowed in the WHERE clause (name: 'x') | `0x2200` Undefined column name x in table e.t |
| duplicate_alias | `SELECT v AS x, ck AS x FROM e.t WHERE pk = 1` | OK Rows | (same) |
| select_element_of_list | `SELECT l[0] FROM e.t WHERE pk = 1` | OK Rows | `0x2200` Element selection is only allowed on sets and maps, but l is a list |
| select_element_of_map_wrong_type | `SELECT m['a'] FROM e.t WHERE pk = 1` | `0x2200` Invalid STRING constant (a) for "key(m)" of type int | (same) |
| select_slice_of_map | `SELECT m[1..2] FROM e.t WHERE pk = 1` | `0x2000` line 1:11 : Missing ']' | OK Rows |
| select_udt_field_unknown | `SELECT t.z FROM e.uses` | `0x2200` t of type frozen<ty> has no field z | (same) |
| select_field_of_non_udt | `SELECT v.x FROM e.t` | `0x2200` Invalid field selection: v of type int is not a user type | (same) |
| arithmetic_type_mismatch | `SELECT v + txt FROM e.t WHERE pk = 1` | `0x2000` line 1:7 no viable alternative at input 'v' | `0x2200` the '+' operation is not supported between v and txt |
| arithmetic_division_by_zero | `SELECT v / 0 FROM e.t WHERE pk = 1` | `0x2000` line 1:10 no viable alternative at input '' | `0x1400` the operation 'int / int' failed: / by zero (extra bytes `000673797374656d00075f64697669646500020003696e740003696e74`) |
| select_bind_unprepared | `SELECT * FROM e.t WHERE pk = ?` | `0x2200` Invalid amount of bind variables: expected 1 received 0 | `0x2200` Invalid amount of bind variables |
| select_named_bind_unprepared | `SELECT * FROM e.t WHERE pk = :p` | `0x2200` Invalid amount of bind variables: expected 1 received 0 | `0x2200` Invalid amount of bind variables |
| ann_on_non_vector | `SELECT * FROM e.t ORDER BY v ANN OF [1, 2, 3] LIMIT 1` | `0x2200` ANN ordering is only supported on float vector indexes | `0x2200` Unexpected receiver type 'int'; only list and vector are expected |
| ann_without_index | `SELECT * FROM e.vec ORDER BY emb ANN OF [1, 2, 3] LIMIT 1` | `0x2200` ANN ordering by vector requires the column to be indexed using 'vector_index' | `0x2200` ANN ordering by vector requires the column to be indexed |
| ann_wrong_dimension | `SELECT * FROM e.vec ORDER BY emb ANN OF [1, 2] LIMIT 1` | `0x2200` Invalid vector literal for emb: type vector<float, 3> expects 3 elements but got 2 | `0x2200` Invalid vector literal for emb of type vector<float, 3>; expected 3 elements, but given 2 |
| select_count_star_alias | `SELECT count(*) AS n FROM e.t WHERE pk = 1` | OK Rows | (same) |
| select_bypass_cache | `SELECT * FROM e.t WHERE pk = 1 BYPASS CACHE` | OK Rows | `0x2000` line 1:31 no viable alternative at input 'BYPASS' (...t WHERE pk = 1 [BYPASS]...) |
| select_using_timeout | `SELECT * FROM e.t WHERE pk = 1 USING TIMEOUT 10s` | OK Rows | `0x2000` line 1:31 no viable alternative at input 'USING' (...t WHERE pk = 1 [USING]...) |
| select_using_timeout_tiny | `SELECT * FROM e.big USING TIMEOUT 1us` | `0x2200` Timeout values cannot have granularity finer than milliseconds | `0x2000` line 1:20 mismatched input 'USING' expecting EOF (SELECT * FROM e.big [USING]...) |
| select_where_true | `SELECT * FROM e.t WHERE pk = 1 AND true` | `0x2000` line 1:35 no viable alternative at input 'true' | `0x2000` line 1:35 no viable alternative at input 'true' (...WHERE pk = 1 AND [true]) |
| select_or | `SELECT * FROM e.t WHERE pk = 1 OR pk = 2` | `0x2000` line 1:31 : Syntax error | `0x2000` line 1:31 no viable alternative at input 'OR' (...t WHERE pk = 1 [OR]...) |
| select_paren_where | `SELECT * FROM e.t WHERE (pk = 1)` | OK Rows | (same) |
| select_empty_blob_key | `SELECT * FROM e.blobk WHERE pk = 0x` | OK Rows | `0x2200` Key may not be empty |
| select_empty_text_key | `SELECT * FROM e.textk WHERE pk = ''` | OK Rows | `0x2200` Key may not be empty |
| select_from_index_table | `SELECT * FROM e.ix_v_index` | OK Rows | `0x2200` table ix_v_index does not exist |
| select_from_index_scylla_name | `SELECT * FROM e.ix_v` | `0x2200` unconfigured table ix_v | `0x2200` table ix_v does not exist |
| select_index_eq_and_filter | `SELECT * FROM e.ix WHERE v = 1 AND ck = 1` | `0x2200` Cannot execute this query as it might involve data filtering and thus may have unpredictable performance. If you want to execute this query despite the performance unpredictability, use ALLOW FILTERING | (same) |
| select_index_range | `SELECT * FROM e.ix WHERE v > 1` | `0x2200` Cannot execute this query as it might involve data filtering and thus may have unpredictable performance. If you want to execute this query despite the performance unpredictability, use ALLOW FILTERING | (same) |
| select_writetime_alias_star | `SELECT *, writetime(v) FROM e.t` | `0x2000` line 1:8 : Syntax error | `0x2000` line 1:8 mismatched input ',' expecting K_FROM (SELECT *[,]...) |
| select_token_function | `SELECT token(pk), token(ck) FROM e.t` | OK Rows | (same) |

## INSERT / UPDATE / DELETE and literals

| id | statement | ScyllaDB 2026.3.3 | Cassandra 5.0.9 |
|---|---|---|---|
| unknown_column_insert | `INSERT INTO e.t (pk, ck, nope) VALUES (1, 1, 1)` | `0x2200` Unknown identifier nope | `0x2200` Undefined column name nope in table e.t |
| unknown_column_update | `UPDATE e.t SET nope = 1 WHERE pk = 1 AND ck = 1` | `0x2200` Unknown identifier nope | `0x2200` Undefined column name nope in table e.t |
| unknown_column_delete | `DELETE nope FROM e.t WHERE pk = 1 AND ck = 1` | `0x2200` Unknown identifier nope | `0x2200` Undefined column name nope in table e.t |
| type_mismatch_pk | `INSERT INTO e.t (pk, ck, v) VALUES ('a', 1, 1)` | `0x2200` Invalid STRING constant (a) for "pk" of type int | (same) |
| type_mismatch_where | `SELECT * FROM e.t WHERE pk = 'a'` | `0x2200` Invalid STRING constant (a) for "pk" of type int | (same) |
| int_overflow | `INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 2147483648)` | `0x2200` marshaling error: Value out of range for type org.apache.cassandra.db.marshal.Int32Type: '2147483648' | `0x2200` Unable to make int from '2147483648' |
| smallint_overflow | `INSERT INTO e.u (id, sm) VALUES (1, 40000)` | `0x2200` marshaling error: Value out of range for type org.apache.cassandra.db.marshal.ShortType: '40000' | `0x2200` Unable to make short from '40000' |
| tinyint_overflow | `INSERT INTO e.u (id, ti) VALUES (1, 300)` | `0x2200` marshaling error: Value out of range for type org.apache.cassandra.db.marshal.ByteType: '300' | `0x2200` Unable to make byte from '300' |
| bigint_overflow | `INSERT INTO e.u (id, bi) VALUES (1, 9223372036854775808)` | `0x2200` marshaling error: Invalid number format '9223372036854775808' | `0x2200` Unable to make long from '9223372036854775808' |
| float_into_int | `INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1.5)` | `0x2200` Invalid FLOAT constant (1.5) for "v" of type int | (same) |
| int_into_text | `INSERT INTO e.t (pk, ck, txt) VALUES (1, 1, 1)` | `0x2200` Invalid INTEGER constant (1) for "txt" of type text | (same) |
| text_into_ascii_nonascii | `INSERT INTO e.u (id, asc_) VALUES (1, 'é')` | `0x2200` marshaling error: Invalid ASCII character in string literal: 'é' | `0x2200` Invalid ASCII character in string literal: java.nio.charset.UnmappableCharacterException: Input length = 1 |
| bad_timestamp_literal | `INSERT INTO e.u (id, ts) VALUES (1, 'notadate')` | `0x2200` marshaling error: unable to parse date 'notadate': marshaling error: Unable to parse timestamp from 'notadate' | `0x2200` Unable to parse a date/time from 'notadate' |
| bad_date_literal | `INSERT INTO e.u (id, dt) VALUES (1, '2020-13-45')` | OK Void | `0x2200` Unable to coerce '2020-13-45' to a formatted date (long) |
| bad_time_literal | `INSERT INTO e.u (id, tm) VALUES (1, '25:00:00')` | `0x2200` marshaling error: Hour out of bounds (25). | `0x2200` (TimeType) Unable to coerce '25:00:00' to a formatted time (long) |
| bad_inet_literal | `INSERT INTO e.u (id, ine) VALUES (1, '999.1.1.1')` | `0x2200` marshaling error: Failed to parse inet_addr from '999.1.1.1' | `0x2200` Unable to make inet address from '999.1.1.1' |
| bad_boolean_literal | `INSERT INTO e.u (id, bo) VALUES (1, 'yes')` | `0x2200` Invalid STRING constant (yes) for "bo" of type boolean | (same) |
| bad_decimal_literal | `INSERT INTO e.u (id, de) VALUES (1, 'abc')` | `0x2200` Invalid STRING constant (abc) for "de" of type decimal | (same) |
| nan_into_decimal | `INSERT INTO e.u (id, de) VALUES (1, NaN)` | `0x2200` marshaling error: unable to make BigDecimal from 'NaN' | `0x2200` unable to make BigDecimal from 'NaN' |
| uuid_from_int | `INSERT INTO e.u (id, u) VALUES (1, 123)` | `0x2200` Invalid INTEGER constant (123) for "u" of type uuid | (same) |
| uuid_bad_string | `INSERT INTO e.u (id, u) VALUES (1, 'not-a-uuid')` | `0x2200` Invalid STRING constant (not-a-uuid) for "u" of type uuid | (same) |
| uuid_into_timeuuid | `INSERT INTO e.u (id, tu) VALUES (1, 6ba7b810-9dad-41d1-80b4-00c04fd430c8)` | `0x2200` marshaling error: Unsupported UUID version (4) | `0x2200` TimeUUID supports only version 1 UUIDs |
| blob_from_string | `INSERT INTO e.u (id, b) VALUES (1, 'text')` | `0x2200` Invalid STRING constant (text) for "b" of type blob | (same) |
| blob_odd_hex | `INSERT INTO e.u (id, b) VALUES (1, 0x123)` | `0x0000` A hex string representing bytes must have an even length | `0x2200` cannot parse '123' as hex bytes |
| bad_duration | `INSERT INTO e.u (id, du) VALUES (1, '1x')` | `0x2200` Invalid STRING constant (1x) for "du" of type duration | (same) |
| duration_in_pk | `CREATE TABLE e.dk (pk duration PRIMARY KEY)` | `0x2200` duration type is not supported for PRIMARY KEY part pk | `0x2200` duration type is not supported for PRIMARY KEY column 'pk' |
| duration_in_clustering | `CREATE TABLE e.dk2 (pk int, ck duration, PRIMARY KEY (pk, ck))` | `0x2200` duration type is not supported for PRIMARY KEY part ck | `0x2200` duration type is not supported for PRIMARY KEY column 'ck' |
| duration_in_set | `CREATE TABLE e.dk3 (pk int PRIMARY KEY, s set<duration>)` | `0x2200` Durations are not allowed inside sets: set<duration> | (same) |
| vector_wrong_dimension | `INSERT INTO e.vec (pk, emb) VALUES (1, [1.0, 2.0])` | `0x2200` Invalid vector literal for emb: type vector<float, 3> expects 3 elements but got 2 | `0x2200` Invalid vector literal for emb of type vector<float, 3>; expected 3 elements, but given 2 |
| vector_null_element | `INSERT INTO e.vec (pk, emb) VALUES (1, [1.0, null, 2.0])` | `0x2200` null is not supported inside vectors | (same) |
| missing_clustering_insert | `INSERT INTO e.t (pk, v) VALUES (1, 1)` | `0x2200` Missing mandatory PRIMARY KEY part ck | `0x2200` Some clustering keys are missing: ck |
| missing_pk_insert | `INSERT INTO e.t (ck, v) VALUES (1, 1)` | `0x2200` Missing mandatory PRIMARY KEY part pk | `0x2200` Some partition key parts are missing: pk |
| values_count_mismatch | `INSERT INTO e.t (pk, ck) VALUES (1, 2, 3)` | `0x2200` Unmatched column names/values | (same) |
| duplicate_insert_column | `INSERT INTO e.t (pk, ck, v, v) VALUES (1, 1, 1, 1)` | `0x2200` Multiple definitions found for column v | `0x2200` The column names contains duplicates |
| null_pk_literal | `INSERT INTO e.t (pk, ck, v) VALUES (null, 1, 1)` | `0x2200` Invalid null value in condition for column pk | (same) |
| null_ck_literal | `INSERT INTO e.t (pk, ck, v) VALUES (1, null, 1)` | `0x2200` Invalid null value in condition for column ck | (same) |
| empty_blob_pk | `INSERT INTO e.blobk (pk, v) VALUES (0x, 1)` | `0x2200` Key may not be empty | (same) |
| empty_text_pk | `INSERT INTO e.textk (pk, ck, v) VALUES ('', 'a', 1)` | `0x2200` Key may not be empty | (same) |
| empty_text_ck | `INSERT INTO e.textk (pk, ck, v) VALUES ('a', '', 1)` | OK Void | (same) |
| pk_too_large | `INSERT INTO e.textk (pk, ck, v) VALUES ('xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx… (70051 chars)` | `0x2200` Key length of 70002 is longer than maximum of 65535 | `0x2200` Key length of 70000 is longer than maximum of 65535 |
| insert_only_pk_static | `INSERT INTO e.t (pk, s) VALUES (1, 1)` | OK Void | (same) |
| update_pk_column | `UPDATE e.t SET pk = 1 WHERE pk = 1 AND ck = 1` | `0x2200` PRIMARY KEY part pk found in SET part | (same) |
| update_ck_column | `UPDATE e.t SET ck = 1 WHERE pk = 1 AND ck = 1` | `0x2200` PRIMARY KEY part ck found in SET part | (same) |
| update_missing_clustering | `UPDATE e.t SET v = 1 WHERE pk = 1` | `0x2200` Missing mandatory PRIMARY KEY part ck | `0x2200` Some clustering keys are missing: ck |
| update_static_with_ck_only | `UPDATE e.t SET s = 1 WHERE ck = 1` | `0x2200` Invalid restrictions on clustering columns since the UPDATE statement modifies only static columns | `0x2200` Some partition key parts are missing: pk |
| update_static_only_pk | `UPDATE e.t SET s = 1 WHERE pk = 1` | OK Void | (same) |
| update_in_clustering | `UPDATE e.t SET v = 1 WHERE pk = 1 AND ck IN (1, 2)` | OK Void | (same) |
| update_range_clustering | `UPDATE e.t SET v = 1 WHERE pk = 1 AND ck > 1` | `0x2200` Invalid operator in where clause (ck > 1) | `0x2200` Slice restrictions are not supported on the clustering columns in UPDATE statements |
| update_regular_where | `UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 AND v = 1` | `0x2200` Invalid where clause contains non PRIMARY KEY columns: v | `0x2200` Non PRIMARY KEY columns found in where clause: v  |
| update_same_column_twice | `UPDATE e.t SET v = 1, v = 2 WHERE pk = 1 AND ck = 1` | `0x2200` Multiple incompatible setting of column v | `0x2000` Multiple incompatible setting of column v |
| update_token_where | `UPDATE e.t SET v = 1 WHERE token(pk) = 1 AND ck = 1` | `0x2200` The token function cannot be used in WHERE clauses for UPDATE and DELETE statements: (token(pk) = 1) | `0x2200` The token function cannot be used in WHERE clauses for UPDATE statements |
| delete_range_with_column | `DELETE v FROM e.t WHERE pk = 1 AND ck > 1` | `0x2200` Range deletions are not supported for specific columns | (same) |
| delete_no_pk | `DELETE FROM e.t WHERE ck = 1` | `0x2200` Missing mandatory PRIMARY KEY part pk | `0x2200` Some partition key parts are missing: pk |
| delete_partial_pk | `DELETE FROM e.t2 WHERE a = 1` | `0x2200` Missing mandatory PRIMARY KEY part b | `0x2200` Some partition key parts are missing: b |
| delete_regular_where | `DELETE FROM e.t WHERE pk = 1 AND v = 1` | `0x2200` Invalid where clause contains non PRIMARY KEY columns: v | `0x2200` Non PRIMARY KEY columns found in where clause: v  |
| delete_range_ok | `DELETE FROM e.t WHERE pk = 1 AND ck > 1 AND ck < 5` | OK Void | (same) |
| delete_clustering_skip | `DELETE FROM e.t2 WHERE a = 1 AND b = 1 AND d = 1` | `0x2200` PRIMARY KEY column "d" cannot be restricted as preceding column "c" is not restricted | (same) |
| delete_pk_column | `DELETE pk FROM e.t WHERE pk = 1 AND ck = 1` | `0x2200` Invalid identifier pk for deletion (should not be a PRIMARY KEY part) | (same) |
| delete_static_with_ck | `DELETE s FROM e.t WHERE pk = 1 AND ck = 1` | `0x2200` Invalid restrictions on clustering columns since the DELETE statement modifies only static columns | (same) |
| delete_list_element_by_index_frozen | `DELETE fl[0] FROM e.t WHERE pk = 1 AND ck = 1` | `0x2200` Invalid deletion operation for frozen collection column fl | (same) |
| delete_set_element | `DELETE st[1] FROM e.t WHERE pk = 1 AND ck = 1` | OK Void | (same) |
| delete_in_pk | `DELETE FROM e.t WHERE pk IN (1, 2) AND ck = 1` | OK Void | (same) |
| delete_using_ttl | `DELETE FROM e.t USING TTL 10 WHERE pk = 1 AND ck = 1` | `0x2000` line 1:22 no viable alternative at input 'TTL' | `0x2000` line 1:22 mismatched input 'TTL' expecting K_TIMESTAMP (DELETE FROM e.t USING [TTL]...) |
| list_add_set_literal | `UPDATE e.t SET l = l + {1} WHERE pk = 1 AND ck = 1` | `0x2200` Invalid set literal for l of type list<int> | (same) |
| list_prepend_wrong_type | `UPDATE e.t SET l = ['a'] + l WHERE pk = 1 AND ck = 1` | `0x2200` Invalid list literal for l: value 'a' is not of type int | (same) |
| list_subtract_from_other | `UPDATE e.t SET l = st - l WHERE pk = 1 AND ck = 1` | `0x2000` line 1:26 no viable alternative at input 'WHERE' | `0x2000` line 1:26 no viable alternative at input 'WHERE' (...SET l = st - [l] WHERE...) |
| frozen_element_set | `UPDATE e.t SET fl[0] = 1 WHERE pk = 1 AND ck = 1` | `0x2200` Invalid operation (fl[0] = 1) for frozen collection column fl | (same) |
| frozen_append | `UPDATE e.t SET fl = fl + [1] WHERE pk = 1 AND ck = 1` | `0x2200` Invalid operation (fl = fl + [1]) for frozen collection column fl | (same) |
| set_index_set | `UPDATE e.t SET st[0] = 1 WHERE pk = 1 AND ck = 1` | `0x2200` Invalid operation (st[0] = 1) for set column st | (same) |
| list_index_out_of_bounds_null | `UPDATE e.t SET l[5] = 1 WHERE pk = 1 AND ck = 1` | `0x2200` Attempted to set an element on a list which is null | (same) |
| list_index_out_of_bounds_existing | `UPDATE e.t SET l[5] = 1 WHERE pk = 1 AND ck = 1` | `0x2200` Attempted to set an element on a list which is null | (same) |
| list_index_negative | `UPDATE e.t SET l[-1] = 1 WHERE pk = 1 AND ck = 1` | `0x2200` Attempted to set an element on a list which is null | (same) |
| map_set_wrong_value_type | `UPDATE e.t SET m[1] = 'a' WHERE pk = 1 AND ck = 1` | `0x2200` Invalid STRING constant (a) for "value(m)" of type int | (same) |
| map_add_list | `UPDATE e.t SET m = m + [1] WHERE pk = 1 AND ck = 1` | `0x2200` Invalid list literal for m of type map<int, int> | `0x2200` Value for a map addition has to be a map, but was: '[1]' |
| null_in_list_literal | `INSERT INTO e.t (pk, ck, l) VALUES (1, 1, [1, null])` | `0x2200` Invalid NULL element in list | `0x2200` null is not supported inside collections |
| null_in_set_literal | `INSERT INTO e.t (pk, ck, st) VALUES (1, 1, {1, null})` | `0x2200` null is not supported inside collections | (same) |
| null_map_value | `INSERT INTO e.t (pk, ck, m) VALUES (1, 1, {1: null})` | `0x2200` null is not supported inside collections | (same) |
| null_map_key | `INSERT INTO e.t (pk, ck, m) VALUES (1, 1, {null: 1})` | `0x2200` null is not supported inside collections | (same) |
| map_wrong_key_type | `INSERT INTO e.t (pk, ck, m) VALUES (1, 1, {'a': 1})` | `0x2200` Invalid map literal for m: key 'a' is not of type int | (same) |
| set_literal_for_map_empty | `INSERT INTO e.t (pk, ck, m) VALUES (1, 1, {})` | OK Void | (same) |
| list_literal_for_set | `INSERT INTO e.t (pk, ck, st) VALUES (1, 1, [1])` | `0x2200` Invalid list literal for st of type set<int> | `0x2200` Unexpected receiver type 'set<int>'; only list and vector are expected |
| tuple_too_many | `INSERT INTO e.t (pk, ck, tu) VALUES (1, 1, (1, 'a', 3))` | `0x2200` Invalid tuple literal for tu: too many elements. Type frozen<tuple<int, text>> expects 2 but got 3 | (same) |
| tuple_wrong_type | `INSERT INTO e.t (pk, ck, tu) VALUES (1, 1, ('a', 'a'))` | `0x2200` Invalid tuple literal for tu: component 0 is not of type int | (same) |
| udt_unknown_field | `INSERT INTO e.uses (pk, t) VALUES (1, {x: 1, z: 'a'})` | `0x2200` Unknown field 'z' in value of user defined type ty | (same) |
| udt_wrong_field_type | `INSERT INTO e.uses (pk, t) VALUES (1, {x: 'a'})` | `0x2200` Invalid user type literal for t: field x is not of type int | (same) |
| udt_set_field_frozen | `UPDATE e.uses SET t.x = 1 WHERE pk = 1` | `0x2200` Invalid operation (t.x = 1) for frozen UDT column t | (same) |
| udt_set_field_nonfrozen_ok | `UPDATE e.uses SET nt.x = 1 WHERE pk = 1` | OK Void | (same) |
| udt_set_unknown_field | `UPDATE e.uses SET nt.z = 1 WHERE pk = 1` | `0x2200` UDT column nt does not have a field named z | (same) |
| ttl_negative | `INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) USING TTL -1` | `0x2200` A TTL must be greater or equal to 0 | `0x2200` A TTL must be greater or equal to 0, but was -1 |
| ttl_too_large | `INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) USING TTL 999999999` | `0x2200` ttl is too large. requested (999999999) maximum (630720000) | (same) |
| ttl_string | `INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) USING TTL 'a'` | `0x2000` line 1:55 no viable alternative at input 'a' | `0x2000` line 1:55 no viable alternative at input 'a' (...1, 1) USING TTL [']...) |
| timestamp_overflow_literal | `INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) USING TIMESTAMP 9223372036854775808` | `0x2200` marshaling error: Invalid number format '9223372036854775808' | `0x2200` Unable to make long from '9223372036854775808' |
| timestamp_min_long | `INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) USING TIMESTAMP -9223372036854775808` | `0x2200` Out of bound timestamp, must be in [-9223372036854775807, 9223372036854775807] | (same) |
| using_ttl_twice | `INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) USING TTL 1 AND TTL 2` | OK Void | (same) |
| using_timeout_insert | `INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) USING TIMEOUT 5s` | OK Void | `0x2000` line 1:51 no viable alternative at input 'TIMEOUT' (..., 1, 1) USING [TIMEOUT]...) |
| insert_json_unknown_field | `INSERT INTO e.t JSON '{"pk": 1, "ck": 1, "nope": 3}'` | `0x2200` JSON values map contains unrecognized column: nope | (same) |
| insert_json_malformed | `INSERT INTO e.t JSON '{bad'` | `0x0000` Parsing JSON failed: Missing a name for object member. at 1 | `0x2200` Could not decode JSON string as a map: com.fasterxml.jackson.core.JsonParseException: Unexpected character ('b' (code 98)): was expecting double-quote to start field name\n at [Source: REDACTED (`StreamReadFeature.INCLUDE_SOURCE_IN_LOCATION` disabled); line: 1, column: 2]. (String was: {bad) |
| insert_json_wrong_type | `INSERT INTO e.t JSON '{"pk": "x", "ck": 1}'` | `0x0000` marshaling error: Invalid number format 'x' | `0x2200` Error decoding JSON value for pk: Unable to make int from 'x' |
| insert_json_missing_pk | `INSERT INTO e.t JSON '{"ck": 1}'` | `0x2200` Missing mandatory PRIMARY KEY part pk | `0x2200` Invalid null value in condition for column pk |
| insert_json_array | `INSERT INTO e.t JSON '[1]'` | `0x0000` JSON assert failed on condition 'IsObject()', at: 0x3259abf 0x18af373 0x48788ec 0x48798bf 0x1c16331 0x1c14a05 0x5a84819 0x18b2f04 0x18b59a3 0x32a99e8 0x32a92ef 0x32a8ca4 0x32a54f7 0x32ab73e /opt/scylladb/libreloc/libc.so.6+0x3680 /opt/scylladb/libreloc/libc.so.6+0x3797 0x38cc004 (BuildId: d0d29ed4522f3714b887a0841c08d59f92b273fc) | `0x2200` Could not decode JSON string as a map: com.fasterxml.jackson.databind.exc.MismatchedInputException: Cannot deserialize value of type `java.util.LinkedHashMap<java.lang.Object,java.lang.Object>` from Array value (token `JsonToken.START_ARRAY`)\n at [Source: REDACTED (`StreamReadFeature.INCLUDE_SOURCE_IN_LOCATION` disabled); line: 1, column: 1]. (String was: [1]) |
| insert_json_null_pk | `INSERT INTO e.t JSON '{"pk": null, "ck": 1}'` | `0x2200` Missing mandatory PRIMARY KEY part pk | `0x2200` Invalid null value in condition for column pk |
| insert_json_default_unset | `INSERT INTO e.t JSON '{"pk": 1, "ck": 1}' DEFAULT UNSET` | OK Void | (same) |
| insert_json_default_bogus | `INSERT INTO e.t JSON '{"pk": 1, "ck": 1}' DEFAULT FOO` | `0x2000` line 1:42 : Syntax error | `0x2000` line 1:50 no viable alternative at input 'FOO' (...e.t JSON '{"pk": 1, "ck": 1}' DEFAULT [FOO]) |
| fromjson_bad | `INSERT INTO e.t (pk, ck, v) VALUES (1, 1, fromJson('"x"'))` | `0x1400` execution of fromjson failed: Failed execution of function system.fromjson: marshal_exception (marshaling error: Invalid number format 'x') (extra bytes `000673797374656d000866726f6d6a736f6e000100286f72672e6170616368652e63617373616e64… (124 chars)`) | `0x1400` org.apache.cassandra.serializers.MarshalException: Unable to make int from 'x' (extra bytes `000673797374656d000866726f6d6a736f6e0001000474657874`) |
| insert_if_exists | `INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) IF EXISTS` | `0x2000` line 1:48 : Missing K_NOT | `0x2000` line 1:48 mismatched input 'EXISTS' expecting K_NOT (..., 1, 1) IF [EXISTS]) |

## Counters

| id | statement | ScyllaDB 2026.3.3 | Cassandra 5.0.9 |
|---|---|---|---|
| counter_set | `UPDATE e.c SET cnt = 1 WHERE pk = 1` | `0x2200` Cannot set the value of counter column cnt (counters can only be incremented/decremented, not set) | (same) |
| counter_insert | `INSERT INTO e.c (pk, cnt) VALUES (1, 1)` | `0x2200` INSERT statement are not allowed on counter tables, use UPDATE instead | `0x2200` INSERT statements are not allowed on counter tables, use UPDATE instead |
| counter_mixed_table | `CREATE TABLE e.cm (pk int PRIMARY KEY, c counter, v int)` | `0x2300` Cannot add a non counter column (v) in a counter column family | `0x2200` Cannot mix counter and non counter columns in the same table |
| counter_ttl | `UPDATE e.c USING TTL 10 SET cnt = cnt + 1 WHERE pk = 1` | `0x2200` Cannot provide custom TTL for counter updates | (same) |
| counter_timestamp | `UPDATE e.c USING TIMESTAMP 10 SET cnt = cnt + 1 WHERE pk = 1` | `0x2200` Cannot provide custom timestamp for counter updates | (same) |
| counter_in_pk | `CREATE TABLE e.cp (pk counter PRIMARY KEY, v int)` | `0x2200` counter type is not supported for PRIMARY KEY part pk | `0x2200` counter type is not supported for PRIMARY KEY column 'pk' |
| counter_in_collection | `CREATE TABLE e.cp2 (pk int PRIMARY KEY, v list<counter>)` | `0x2200` Counters are not allowed inside collections: list<counter> | (same) |
| counter_static | `CREATE TABLE e.cp3 (pk int, ck int, c counter static, d counter, PRIMARY KEY (pk, ck))` | OK SchemaChange `["CREATED", "TABLE", "e", "cp3"]` | (same) |
| counter_increment_by_text | `UPDATE e.c SET cnt = cnt + 'a' WHERE pk = 1` | `0x2200` Invalid STRING constant (a) for "cnt" of type counter | (same) |
| counter_increment_other_column | `UPDATE e.c SET cnt = pk + 1 WHERE pk = 1` | `0x2000` Only expressions of the form X = X +<value> are supported. | (same) |
| counter_minus_reversed | `UPDATE e.c SET cnt = 1 - cnt WHERE pk = 1` | `0x2000` line 1:23 : Syntax error | `0x2000` line 1:29 no viable alternative at input 'WHERE' (...SET cnt = 1 - [cnt] WHERE...) |
| counter_delete_column_ok | `DELETE cnt FROM e.c WHERE pk = 1` | OK Void | (same) |
| non_counter_increment | `UPDATE e.t SET v = v + 1 WHERE pk = 1 AND ck = 1` | `0x2200` SET with a column expression (e.g. col = col + 1) requires an LWT condition (e.g., IF col != NULL or IF EXISTS) to ensure atomic read-before-write | `0x2200` Invalid operation (v = v + 1) for non counter column v |
| counter_lwt | `UPDATE e.c SET cnt = cnt + 1 WHERE pk = 1 IF cnt = 1` | `0x2200` Conditional updates are not supported on counter tables | `0x2200` Conditions on counters are not supported |
| counter_default_ttl | `CREATE TABLE e.cttl (pk int PRIMARY KEY, c counter) WITH default_time_to_live = 10` | `0x2200` Cannot set default_time_to_live on a table with counters | (same) |
| counter_alter_add_regular | `ALTER TABLE e.c ADD v int` | `0x2300` Cannot add a non counter column (v) in a counter column family | `0x2300` e.c: Cannot have a non counter column ("v") in a counter table |
| counter_overflow_ok | `UPDATE e.c SET cnt = cnt + 9223372036854775807 WHERE pk = 5` | OK Void | (same) |
| counter_overflow_wrap | `UPDATE e.c SET cnt = cnt + 9223372036854775807 WHERE pk = 5` | OK Void | (same) |
| counter_index | `CREATE INDEX ON e.c (cnt)` | `0x2200` Secondary indexes are not supported on counter tables | `0x2200` Secondary indexes on counter tables aren't supported |
| counter_consistency_any | `UPDATE e.c SET cnt = cnt + 1 WHERE pk = 1` | `0x2200` Consistency level ANY is not yet supported for counter table c | (same) |

## Lightweight transactions

| id | statement | ScyllaDB 2026.3.3 | Cassandra 5.0.9 |
|---|---|---|---|
| lwt_custom_timestamp | `INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) IF NOT EXISTS USING TIMESTAMP 1` | `0x2200` Cannot provide custom timestamp for conditional updates | (same) |
| lwt_in | `UPDATE e.t SET v = 1 WHERE pk IN (1, 2) AND ck = 1 IF v = 1` | `0x2200` IN on the partition key is not supported with conditional updates | (same) |
| lwt_in_clustering | `UPDATE e.t SET v = 1 WHERE pk = 1 AND ck IN (1, 2) IF v = 1` | `0x2200` IN on the clustering key columns is not supported with conditional updates | (same) |
| lwt_range_delete | `DELETE FROM e.t WHERE pk = 1 AND ck > 1 IF EXISTS` | `0x2200` DELETE statements must restrict all PRIMARY KEY columns with equality relations in order to delete non static columns | (same) |
| lwt_condition_on_pk | `UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF pk = 1` | `0x2200` PRIMARY KEY column 'pk' cannot have IF conditions | (same) |
| lwt_condition_on_ck | `UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF ck = 1` | `0x2200` PRIMARY KEY column 'ck' cannot have IF conditions | (same) |
| lwt_condition_unknown | `UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF nope = 1` | `0x2200` Unrecognized name nope | `0x2200` Undefined column name nope in table e.t |
| lwt_condition_type | `UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF v = 'a'` | `0x2200` Invalid STRING constant (a) for "v" of type int | (same) |
| lwt_if_exists_and_condition | `UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF EXISTS AND v = 1` | `0x2000` line 1:48 no viable alternative at input 'EXISTS' | `0x2000` line 1:55 no viable alternative at input 'AND' (...AND ck = 1 IF [EXISTS] AND...) |
| lwt_insert_if_condition | `INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) IF v = 1` | `0x2000` line 1:48 : Syntax error | `0x2000` line 1:48 mismatched input 'v' expecting K_NOT (..., 1, 1) IF [v]...) |
| lwt_static_only_condition | `UPDATE e.t SET s = 1 WHERE pk = 1 IF s = 1` | OK Rows | (same) |
| lwt_list_element_condition | `UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF l[0] = 1` | OK Rows | (same) |
| lwt_map_element_condition_wrong_type | `UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF m['a'] = 1` | `0x2200` Invalid STRING constant (a) for "key(m)" of type int | (same) |
| lwt_in_condition | `UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF v IN (1, 2)` | OK Rows | (same) |
| lwt_contains_condition | `UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF l CONTAINS 1` | `0x2000` line 1:50 no viable alternative at input 'CONTAINS' | OK Rows |
| lwt_like_condition | `UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF txt LIKE 'a%'` | OK Rows | `0x2000` line 1:52 no viable alternative at input 'LIKE' (...ck = 1 IF txt [LIKE]...) |
| lwt_serial_consistency_one | `UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF v = 1` | `0x2200` Invalid consistency for conditional update. Must be one of SERIAL or LOCAL_SERIAL | (same) |
| lwt_serial_read_ok | `SELECT * FROM e.t WHERE pk = 1 AND ck = 1` | OK Rows | (same) |
| lwt_serial_read_range | `SELECT * FROM e.t` | `0x2200` SERIAL/LOCAL_SERIAL consistency may only be requested for one partition at a time | OK Rows |
| lwt_ok_not_applied | `UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF v = 5` | OK Rows | (same) |
| lwt_ok_insert | `INSERT INTO e.t (pk, ck, v) VALUES (9, 9, 9) IF NOT EXISTS` | OK Rows | (same) |
| lwt_ok_insert_again | `INSERT INTO e.t (pk, ck, v) VALUES (9, 9, 9) IF NOT EXISTS` | OK Rows | (same) |
| lwt_with_ttl_ok | `UPDATE e.t USING TTL 100 SET v = 1 WHERE pk = 1 AND ck = 1 IF EXISTS` | OK Rows | (same) |

## Batches

| id | statement | ScyllaDB 2026.3.3 | Cassandra 5.0.9 |
|---|---|---|---|
| batch_lwt_two_partitions | `BEGIN BATCH UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF v = 1; UPDATE e.t SET v = 1 WHERE pk = 2 AND ck = 1 IF v = 1; APPLY BATCH` | `0x2200` BATCH with conditions cannot span multiple partitions | `0x2200` Batch with conditions cannot span multiple partitions |
| batch_lwt_two_tables | `BEGIN BATCH UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF v = 1; UPDATE e.ix SET v = 1 WHERE pk = 1 AND ck = 1 IF v = 1; APPLY BATCH` | `0x2200` BATCH with conditions cannot span multiple tables | `0x2200` Batch with conditions cannot span multiple tables |
| batch_lwt_with_timestamp | `BEGIN BATCH USING TIMESTAMP 5 UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF v = 1; APPLY BATCH` | `0x2200` Cannot provide custom timestamp for conditional BATCH | (same) |
| batch_lwt_ok_mixed | `BEGIN BATCH UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF v = 1; INSERT INTO e.t (pk, ck, v) VALUES (1, 2, 2); APPLY BATCH` | OK Rows | (same) |
| batch_counter_in_logged | `BEGIN BATCH UPDATE e.c SET cnt = cnt + 1 WHERE pk = 1; APPLY BATCH` | `0x2200` Cannot include a counter statement in a logged batch | (same) |
| batch_counter_in_unlogged | `BEGIN UNLOGGED BATCH UPDATE e.c SET cnt = cnt + 1 WHERE pk = 1; APPLY BATCH` | OK Void | (same) |
| batch_regular_in_counter | `BEGIN COUNTER BATCH INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1); APPLY BATCH` | `0x2200` Cannot include non-counter statement in a counter batch | (same) |
| batch_counter_ok | `BEGIN COUNTER BATCH UPDATE e.c SET cnt = cnt + 1 WHERE pk = 1; APPLY BATCH` | OK Void | (same) |
| batch_mixed_counter_regular | `BEGIN UNLOGGED BATCH UPDATE e.c SET cnt = cnt + 1 WHERE pk = 1; INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1); APPLY BATCH` | `0x2200` Counter and non-counter mutations cannot exist in the same batch | (same) |
| batch_select | `BEGIN BATCH SELECT * FROM e.t; APPLY BATCH` | `0x2000` line 1:12 : Syntax error | `0x2000` line 1:12 mismatched input 'SELECT' expecting K_APPLY (BEGIN BATCH [SELECT]...) |
| batch_ddl | `BEGIN BATCH CREATE TABLE e.zz (a int PRIMARY KEY); APPLY BATCH` | `0x2000` line 1:12 : Syntax error | `0x2000` line 1:12 mismatched input 'CREATE' expecting K_APPLY (BEGIN BATCH [CREATE]...) |
| batch_truncate | `BEGIN BATCH TRUNCATE e.t; APPLY BATCH` | `0x2000` line 1:12 : Syntax error | `0x2000` line 1:12 mismatched input 'TRUNCATE' expecting K_APPLY (BEGIN BATCH [TRUNCATE]...) |
| batch_nested | `BEGIN BATCH BEGIN BATCH INSERT INTO e.t (pk, ck) VALUES (1, 1); APPLY BATCH; APPLY BATCH` | `0x2000` line 1:12 : Syntax error | `0x2000` line 1:12 mismatched input 'BEGIN' expecting K_APPLY (BEGIN BATCH [BEGIN]...) |
| batch_double_timestamp | `BEGIN BATCH USING TIMESTAMP 1 INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) USING TIMESTAMP 2; APPLY BATCH` | `0x2200` Timestamp must be set either on BATCH or individual statements | (same) |
| batch_ttl_on_batch | `BEGIN BATCH USING TTL 10 INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1); APPLY BATCH` | `0x2200` Global TTL on the BATCH statement is not supported. | (same) |
| batch_empty | `BEGIN BATCH APPLY BATCH` | OK Void | (same) |
| batch_no_semicolons | `BEGIN BATCH INSERT INTO e.t (pk, ck) VALUES (1, 1) INSERT INTO e.t (pk, ck) VALUES (1, 2) APPLY BATCH` | OK Void | (same) |
| batch_unknown_table | `BEGIN BATCH INSERT INTO e.nope (pk) VALUES (1); APPLY BATCH` | `0x2200` unconfigured table nope | `0x2200` table nope does not exist |
| batch_missing_apply | `BEGIN BATCH INSERT INTO e.t (pk, ck) VALUES (1, 1);` | `0x2000` line 1:0 : Syntax error | `0x2000` line 0:-1 mismatched input '<EOF>' expecting K_APPLY |
| batch_consistency_serial | `BEGIN BATCH INSERT INTO e.t (pk, ck) VALUES (1, 1); APPLY BATCH` | `0x2200` You must use conditional updates for serializable writes | OK Void |

## DDL

| id | statement | ScyllaDB 2026.3.3 | Cassandra 5.0.9 |
|---|---|---|---|
| keyspace_exists | `CREATE KEYSPACE e WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}` | `0x2400` Cannot add existing keyspace "e" (extra: `{"keyspace": "e", "table": ""}`) | (same) |
| keyspace_exists_if_not_exists | `CREATE KEYSPACE IF NOT EXISTS e WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}` | OK SchemaChange `["CREATED", "KEYSPACE", "e"]`; warnings: "Using Replication Factor datacenter1=1 lower than the minimum_replication_factor_warn_threshold=3 is not recommended." | OK Void |
| table_exists | `CREATE TABLE e.t (pk int PRIMARY KEY)` | `0x2400` Cannot add already existing table "t" to keyspace "e" (extra: `{"keyspace": "e", "table": "t"}`) | (same) |
| table_exists_if_not_exists | `CREATE TABLE IF NOT EXISTS e.t (pk int PRIMARY KEY)` | OK SchemaChange `["CREATED", "TABLE", "e", "t"]` | OK Void |
| index_exists_same_name | `CREATE INDEX ix_v ON e.ix (v)` | `0x2200` Index already exists | `0x2200` Index 'ix_v' already exists |
| index_exists_other_name | `CREATE INDEX ix_v2 ON e.ix (v)` | `0x2200` Index ix_v2 is a duplicate of existing index ix_v | (same) |
| index_exists_if_not_exists | `CREATE INDEX IF NOT EXISTS ix_v ON e.ix (v)` | OK Void; warnings: "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | OK Void |
| index_name_clashes_table | `CREATE INDEX t ON e.ix (ck)` | OK SchemaChange `["UPDATED", "TABLE", "e", "ix"]`; warnings: "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | OK SchemaChange `["UPDATED", "TABLE", "e", "ix"]` |
| type_exists | `CREATE TYPE e.ty (x int)` | `0x2200` A user type of name e.ty already exists | `0x2200` A user type with name 'ty' already exists |
| ks_unknown_dc | `CREATE KEYSPACE x1 WITH replication = {'class': 'NetworkTopologyStrategy', 'nodc': 1}` | `0x2300` Unrecognized strategy option {nodc} passed to NetworkTopologyStrategy | `0x2300` Unrecognized strategy option {nodc} passed to NetworkTopologyStrategy for keyspace x1 |
| ks_simple_missing_rf | `CREATE KEYSPACE x2 WITH replication = {'class': 'SimpleStrategy'}` | `0x2300` SimpleStrategy requires a replication_factor strategy option. | OK SchemaChange `["CREATED", "KEYSPACE", "x2"]` |
| ks_unknown_class | `CREATE KEYSPACE x3 WITH replication = {'class': 'FooStrategy'}` | `0x2300` unable to find class 'FooStrategy' | `0x2300` Unable to find replication strategy class 'org.apache.cassandra.locator.FooStrategy' |
| ks_local_strategy | `CREATE KEYSPACE x3b WITH replication = {'class': 'LocalStrategy'}` | `0x2300` LocalStrategy doesn't support tablet replication | `0x2200` Unable to use given strategy class: LocalStrategy is reserved for internal use. |
| ks_everywhere_strategy | `CREATE KEYSPACE x3c WITH replication = {'class': 'EverywhereStrategy'}` | `0x2300` EverywhereStrategy doesn't support tablet replication | `0x2300` Unable to find replication strategy class 'org.apache.cassandra.locator.EverywhereStrategy' |
| ks_bad_rf | `CREATE KEYSPACE x4 WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 'abc'}` | `0x2300` Replication factor must be numeric and non-negative, found 'abc' | `0x2300` For input string: "abc" |
| ks_negative_rf | `CREATE KEYSPACE x4b WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': -1}` | `0x2300` Replication factor must be numeric and non-negative, found '-1' | `0x2300` Replication factor must be non-negative, found -1 |
| ks_zero_rf | `CREATE KEYSPACE x4c WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 0}` | OK SchemaChange `["CREATED", "KEYSPACE", "x4c"]` | `0x2300` Configuration for at least one datacenter must be present |
| ks_transient_rf | `CREATE KEYSPACE x4d WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': '3/1'}` | `0x2300` Replication factor must be numeric and non-negative, found '3/1' | `0x2300` Transient replication is not enabled on this node |
| ks_no_replication | `CREATE KEYSPACE x5` | OK SchemaChange `["CREATED", "KEYSPACE", "x5"]` | `0x2000` line 0:-1 mismatched input '<EOF>' expecting K_WITH |
| ks_no_class | `CREATE KEYSPACE x6 WITH replication = {'replication_factor': 1}` | OK SchemaChange `["CREATED", "KEYSPACE", "x6"]`; warnings: "Using Replication Factor replication_factor=1 lower than the minimum_replication_factor_warn_threshold=3 is not recommended." | `0x2300` Missing replication strategy class |
| ks_unknown_option | `CREATE KEYSPACE x7 WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1} AND nope = 1` | `0x2000` Unknown property 'nope' | (same) |
| ks_rf_too_high | `CREATE KEYSPACE x8 WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 3}` | OK SchemaChange `["CREATED", "KEYSPACE", "x8"]`; warnings: "Keyspace 'x8' is not RF-rack-valid: the replication factor doesn't match the rack count in at least one datacenter. A rack failure may reduce availability. For more context, see: https://docs.scylladb.com/manual/stable/reference/glossary.html#term-RF-rack-valid-keyspace." | OK SchemaChange `["CREATED", "KEYSPACE", "x8"]`; warnings: "Your replication factor 3 for keyspace x8 is higher than the number of nodes 1 for datacenter datacenter1" |
| ks_tablets_option | `CREATE KEYSPACE x9 WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1} AND tablets = {'enabled': false}` | OK SchemaChange `["CREATED", "KEYSPACE", "x9"]`; warnings: "Using Replication Factor datacenter1=1 lower than the minimum_replication_factor_warn_threshold=3 is not recommended." | `0x2000` Unknown property 'tablets' |
| ks_tablets_initial | `CREATE KEYSPACE x9b WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1} AND tablets = {'initial': 8}` | OK SchemaChange `["CREATED", "KEYSPACE", "x9b"]`; warnings: "Keyspace `initial` tablets option is deprecated.  Use per-table tablet options instead." / "Using Replication Factor datacenter1=1 lower than the minimum_replication_factor_warn_threshold=3 is not recommended." | `0x2000` Unknown property 'tablets' |
| ks_nts_rf_option | `CREATE KEYSPACE x10 WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}` | OK SchemaChange `["CREATED", "KEYSPACE", "x10"]`; warnings: "Using Replication Factor replication_factor=1 lower than the minimum_replication_factor_warn_threshold=3 is not recommended." | OK SchemaChange `["CREATED", "KEYSPACE", "x10"]` |
| ks_durable_writes_string | `CREATE KEYSPACE x11 WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1} AND durable_writes = 'maybe'` | OK SchemaChange `["CREATED", "KEYSPACE", "x11"]`; warnings: "Using Replication Factor datacenter1=1 lower than the minimum_replication_factor_warn_threshold=3 is not recommended." | `0x2000` Invalid boolean value maybe for 'durable_writes'. Positive values can be '1', 'true' or 'yes'. Negative values can be '0', 'false' or 'no'. |
| ks_name_too_long | `CREATE KEYSPACE kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}` | OK SchemaChange `["CREATED", "KEYSPACE", "kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk"]`; warnings: "Using Replication Factor datacenter1=1 lower than the minimum_replication_factor_warn_threshold=3 is not recommended." | `0x2200` Keyspace name must not be more than 48 characters long (got 60 characters for "kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk") |
| ks_name_invalid | `CREATE KEYSPACE "bad-name" WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}` | `0x2200` "bad-name" is not a valid keyspace name | `0x2200` Keyspace name must not be empty and must contain alphanumeric or underscore characters only (got "bad-name") |
| ks_replication_not_map | `CREATE KEYSPACE x12 WITH replication = 'abc'` | `0x2000` Invalid value for property 'replication'. It should be a map. | (same) |
| ks_full_class_name | `CREATE KEYSPACE x13 WITH replication = {'class': 'org.apache.cassandra.locator.NetworkTopologyStrategy', 'datacenter1': 1}` | OK SchemaChange `["CREATED", "KEYSPACE", "x13"]`; warnings: "Using Replication Factor datacenter1=1 lower than the minimum_replication_factor_warn_threshold=3 is not recommended." | OK SchemaChange `["CREATED", "KEYSPACE", "x13"]` |
| ks_alter_class_change | `ALTER KEYSPACE e WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1}` | `0x2200` Cannot alter replication strategy vnode/tablets flavor | OK SchemaChange `["UPDATED", "KEYSPACE", "e"]` |
| ks_alter_rf_jump | `ALTER KEYSPACE e WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 3}` | `0x2200` Only one DC's RF can be changed at a time and not by more than 1 | OK SchemaChange `["UPDATED", "KEYSPACE", "e"]`; warnings: "Your replication factor 3 for keyspace e is higher than the number of nodes 1 for datacenter datacenter1" / "When increasing replication factor you need to run a full (-full) repair to distribute the data." |
| ks_alter_tablets | `ALTER KEYSPACE e WITH tablets = {'enabled': false}` | `0x2200` Cannot alter replication strategy vnode/tablets flavor | `0x2000` Unknown property 'tablets' |
| table_unknown_property | `CREATE TABLE e.x1 (pk int PRIMARY KEY) WITH nope = 1` | `0x2000` Unknown property 'nope' | (same) |
| table_bad_compaction | `CREATE TABLE e.x2 (pk int PRIMARY KEY) WITH compaction = {'class': 'Nope'}` | `0x2300` Unable to find compaction strategy class 'Nope' | `0x2300` Unable to find compaction strategy class 'org.apache.cassandra.db.compaction.Nope' |
| table_compaction_no_class | `CREATE TABLE e.x2b (pk int PRIMARY KEY) WITH compaction = {'min_threshold': '2'}` | `0x2300` Missing sub-option 'class' for the 'compaction' option. | `0x2300` Missing sub-option 'class' for the 'compaction' option |
| table_compaction_bad_option | `CREATE TABLE e.x2c (pk int PRIMARY KEY) WITH compaction = {'class': 'SizeTieredCompactionStrategy', 'nope': '1'}` | `0x2300` Invalid compaction strategy options {"nope": "1"} for chosen strategy type | `0x2300` Properties specified [nope] are not understood by SizeTieredCompactionStrategy |
| table_compaction_bad_threshold | `CREATE TABLE e.x2d (pk int PRIMARY KEY) WITH compaction = {'class': 'SizeTieredCompactionStrategy', 'min_threshold': '1'}` | `0x2300` min_threshold value (1) must be bigger or equal to 2 | `0x2300` Min compaction threshold cannot be less than 2 (got 1) |
| table_twcs_bad_unit | `CREATE TABLE e.x2e (pk int PRIMARY KEY) WITH compaction = {'class': 'TimeWindowCompactionStrategy', 'compaction_window_unit': 'WEEKS'}` | `0x2300` Invalid window unit WEEKS for compaction_window_unit | `0x2300` WEEKS is not valid for compaction_window_unit |
| table_negative_gc_grace | `CREATE TABLE e.x3 (pk int PRIMARY KEY) WITH gc_grace_seconds = -1` | OK SchemaChange `["CREATED", "TABLE", "e", "x3"]` | `0x2300` gc_grace_seconds must be greater than or equal to 0 (got -1) |
| table_negative_ttl | `CREATE TABLE e.x3b (pk int PRIMARY KEY) WITH default_time_to_live = -1` | `0x2300` default_time_to_live cannot be smaller than 0, (default 0) | `0x2300` default_time_to_live must be greater than or equal to 0 (got -1) |
| table_ttl_too_large | `CREATE TABLE e.x3c (pk int PRIMARY KEY) WITH default_time_to_live = 999999999` | OK SchemaChange `["CREATED", "TABLE", "e", "x3c"]` | `0x2300` default_time_to_live must be less than or equal to 630720000 (got 999999999) |
| table_compression_class | `CREATE TABLE e.x4 (pk int PRIMARY KEY) WITH compression = {'class': 'LZ4Compressor'}` | `0x2300` Missing sub-option 'sstable_compression' for the 'compression' option. | OK SchemaChange `["CREATED", "TABLE", "e", "x4"]` |
| table_compression_sstable_compression | `CREATE TABLE e.x4b (pk int PRIMARY KEY) WITH compression = {'sstable_compression': 'LZ4Compressor'}` | OK SchemaChange `["CREATED", "TABLE", "e", "x4b"]` | `0x2300` Missing sub-option 'class' for the 'compression' option. |
| table_compression_unknown | `CREATE TABLE e.x4c (pk int PRIMARY KEY) WITH compression = {'class': 'NopeCompressor'}` | `0x2300` Missing sub-option 'sstable_compression' for the 'compression' option. | `0x2300` Could not create Compression for type org.apache.cassandra.io.compress.NopeCompressor |
| table_compression_bad_chunk | `CREATE TABLE e.x4d (pk int PRIMARY KEY) WITH compression = {'class': 'LZ4Compressor', 'chunk_length_in_kb': '3'}` | `0x2300` Missing sub-option 'sstable_compression' for the 'compression' option. | `0x2300` chunk_length_in_kb must be a power of 2 |
| table_compression_unknown_option | `CREATE TABLE e.x4e (pk int PRIMARY KEY) WITH compression = {'class': 'LZ4Compressor', 'nope': '1'}` | `0x2300` Missing sub-option 'sstable_compression' for the 'compression' option. | `0x2300` Unknown compression options nope |
| table_speculative_retry_95p | `CREATE TABLE e.x5 (pk int PRIMARY KEY) WITH speculative_retry = '95p'` | `0x0000` sstring out of range | OK SchemaChange `["CREATED", "TABLE", "e", "x5"]` |
| table_speculative_retry_bad | `CREATE TABLE e.x5b (pk int PRIMARY KEY) WITH speculative_retry = 'sometimes'` | `0x0000` sstring out of range | `0x2300` Invalid value sometimes for option 'speculative_retry' |
| table_speculative_retry_ms | `CREATE TABLE e.x5c (pk int PRIMARY KEY) WITH speculative_retry = '50ms'` | OK SchemaChange `["CREATED", "TABLE", "e", "x5c"]` | (same) |
| table_bad_caching | `CREATE TABLE e.x6 (pk int PRIMARY KEY) WITH caching = {'keys': 'SOME'}` | `0x2300` Invalid key value: SOME | `0x2300` Invalid value 'SOME' for caching sub-option 'keys': only 'ALL' and 'NONE' are allowed |
| table_caching_rows_bad | `CREATE TABLE e.x6b (pk int PRIMARY KEY) WITH caching = {'keys': 'ALL', 'rows_per_partition': 'MANY'}` | `0x2300` Invalid key value: MANY | `0x2300` Invalid value 'MANY' for caching sub-option 'rows_per_partition': only 'ALL', 'NONE', and integer values are allowed |
| table_bloom_out_of_range | `CREATE TABLE e.x7 (pk int PRIMARY KEY) WITH bloom_filter_fp_chance = 2` | `0x2300` bloom_filter_fp_chance must be larger than 6.71e-05 and less than or equal to 1.0 (got 2) | `0x2300` bloom_filter_fp_chance must be larger than 6.71E-5 and less than or equal to 1.0 (got 2.0) |
| table_bloom_zero | `CREATE TABLE e.x7b (pk int PRIMARY KEY) WITH bloom_filter_fp_chance = 0` | `0x2300` bloom_filter_fp_chance must be larger than 6.71e-05 and less than or equal to 1.0 (got 0) | `0x2300` bloom_filter_fp_chance must be larger than 6.71E-5 and less than or equal to 1.0 (got 0.0) |
| table_min_index_interval_bad | `CREATE TABLE e.x7c (pk int PRIMARY KEY) WITH min_index_interval = 0` | `0x2300` min_index_interval must be greater than 0 | `0x2300` min_index_interval must be greater than or equal to 1 (got 0) |
| table_crc_check_chance_bad | `CREATE TABLE e.x7d (pk int PRIMARY KEY) WITH crc_check_chance = 2` | OK SchemaChange `["CREATED", "TABLE", "e", "x7d"]` | `0x2300` crc_check_chance must be larger than or equal to 0 and smaller than or equal to 1.0 (got 2.0) |
| table_comment_number | `CREATE TABLE e.x7e (pk int PRIMARY KEY) WITH comment = 5` | OK SchemaChange `["CREATED", "TABLE", "e", "x7e"]` | (same) |
| table_property_twice | `CREATE TABLE e.x7f (pk int PRIMARY KEY) WITH comment = 'a' AND comment = 'b'` | `0x2000` Multiple definition for property 'comment' | `0x2000` Multiple definitions for property 'comment' |
| table_no_pk | `CREATE TABLE e.x8 (pk int, v int)` | `0x2200` No PRIMARY KEY specified (exactly one required) | `0x2200` No PRIMARY KEY specifed for table 'e.x8' (exactly one required) |
| table_two_pk_decls | `CREATE TABLE e.x9 (pk int PRIMARY KEY, v int, PRIMARY KEY (v))` | `0x2200` Multiple PRIMARY KEYs specified (exactly one required) | `0x2200` Multiple PRIMARY KEY specified for table 'e.x9' (exactly one required) |
| table_pk_unknown_column | `CREATE TABLE e.x9b (pk int, PRIMARY KEY (nope))` | `0x2200` Unknown definition nope referenced in PRIMARY KEY | `0x2200` Unknown column 'nope' referenced in PRIMARY KEY for table 'x9b' |
| table_pk_repeated | `CREATE TABLE e.x9c (pk int, ck int, PRIMARY KEY (pk, pk))` | `0x2200` Unknown definition pk referenced in PRIMARY KEY | `0x2200` Duplicate column 'pk' in PRIMARY KEY clause for table 'x9c' |
| table_duplicate_column | `CREATE TABLE e.x10 (pk int PRIMARY KEY, pk text)` | `0x2200` Multiple definition of identifier pk | `0x2200` Duplicate column 'pk' declaration for table 'e.x10' |
| table_static_no_clustering | `CREATE TABLE e.x11 (pk int PRIMARY KEY, v int static)` | `0x2200` Static columns are only useful (and thus allowed) if the table has at least one clustering column | (same) |
| table_static_pk | `CREATE TABLE e.x11b (pk int static, ck int, PRIMARY KEY (pk, ck))` | `0x2200` Static column pk cannot be part of the PRIMARY KEY | `0x2200` Static column 'pk' cannot be part of the PRIMARY KEY |
| table_nonfrozen_collection_pk | `CREATE TABLE e.x12 (pk list<int> PRIMARY KEY)` | `0x2200` Invalid non-frozen collection type for PRIMARY KEY component pk | `0x2200` Invalid non-frozen collection type list<int> for PRIMARY KEY column 'pk' |
| table_nonfrozen_collection_ck | `CREATE TABLE e.x12b (pk int, ck set<int>, PRIMARY KEY (pk, ck))` | `0x2200` Invalid non-frozen collection type for PRIMARY KEY component ck | `0x2200` Invalid non-frozen collection type set<int> for PRIMARY KEY column 'ck' |
| table_nonfrozen_udt_pk | `CREATE TABLE e.x12c (pk ty PRIMARY KEY)` | `0x2200` Invalid non-frozen user-defined type for PRIMARY KEY component pk | `0x2200` Invalid non-frozen user-defined type "e.ty" for PRIMARY KEY column 'pk' |
| table_nonfrozen_nested_collection | `CREATE TABLE e.x12d (pk int PRIMARY KEY, v list<list<int>>)` | `0x2200` Non-frozen user types or collections are not allowed inside collections: list<list<int>> | `0x2200` Non-frozen collections are not allowed inside collections: list<list<int>> |
| table_order_by_pk | `CREATE TABLE e.x13 (pk int PRIMARY KEY) WITH CLUSTERING ORDER BY (pk DESC)` | `0x2200` Only clustering key columns can be defined in CLUSTERING ORDER directive | `0x2200` Only clustering key columns can be defined in CLUSTERING ORDER directive: [pk] are not clustering columns |
| table_order_by_wrong_order | `CREATE TABLE e.x13b (pk int, a int, b int, PRIMARY KEY (pk, a, b)) WITH CLUSTERING ORDER BY (b DESC, a ASC)` | `0x2200` The order of columns in the CLUSTERING ORDER directive must be the one of the clustering key (a must appear before b) | `0x2200` The order of columns in the CLUSTERING ORDER directive must match that of the clustering columns (a must appear before b) |
| table_order_by_missing_prefix | `CREATE TABLE e.x13c (pk int, a int, b int, PRIMARY KEY (pk, a, b)) WITH CLUSTERING ORDER BY (b DESC)` | `0x2200` Missing CLUSTERING ORDER for column a | (same) |
| table_unknown_type | `CREATE TABLE e.x14 (pk int PRIMARY KEY, v nosuchtype)` | `0x2200` Unknown type e.nosuchtype | (same) |
| table_udt_other_keyspace | `CREATE TABLE e.x14b (pk int PRIMARY KEY, v system.ty)` | `0x2200` Statement on keyspace e cannot refer to a user type in keyspace system; user types can only be used in the keyspace they are defined in | (same) |
| table_compact_storage | `CREATE TABLE e.x15 (pk int PRIMARY KEY, v int) WITH COMPACT STORAGE` | `0x2200` Support for the deprecated feature of 'CREATE TABLE WITH COMPACT STORAGE' is disabled and will eventually be removed in a future version.  To enable, set the 'enable_create_table_with_compact_storage' config option to 'true'. | OK SchemaChange `["CREATED", "TABLE", "e", "x15"]` |
| table_cdc_option_map | `CREATE TABLE e.x16 (pk int PRIMARY KEY, v int) WITH cdc = {'enabled': true}` | OK SchemaChange `["CREATED", "TABLE", "e", "x16"]` | `0x2000` Invalid value for property 'cdc'. It should be a string |
| table_cdc_option_bool | `CREATE TABLE e.x16b (pk int PRIMARY KEY, v int) WITH cdc = true` | `0x0000` Cannot create cdc info from string | OK SchemaChange `["CREATED", "TABLE", "e", "x16b"]` |
| table_unknown_keyspace | `CREATE TABLE nope.x (pk int PRIMARY KEY)` | `0x2200` Can't find a keyspace nope | `0x2200` Keyspace 'nope' doesn't exist |
| table_name_too_long | `CREATE TABLE e.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa… (336 chars)` | `0x2200` Table names shouldn't be more than 192 characters long (got "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa") | `0x2300` e.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa: Table name must not be more than 222 characters long (got 300 characters for "aaaaaaaaaaaaaaaaaa… (684 chars) |
| table_name_48 | `CREATE TABLE e.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa (pk int PRIMARY KEY)` | OK SchemaChange `["CREATED", "TABLE", "e", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]` | (same) |
| table_vector_bad_dim | `CREATE TABLE e.x17 (pk int PRIMARY KEY, v vector<float, 0>)` | `0x2200` Vectors must have a dimension greater than 0 | `0x2200` vectors may only have positive dimensions; given 0 |
| table_vector_non_frozen | `CREATE TABLE e.x17b (pk int PRIMARY KEY, v vector<list<int>, 2>)` | OK SchemaChange `["CREATED", "TABLE", "e", "x17b"]` | (same) |
| table_counter_and_ttl | `CREATE TABLE e.x18 (pk int PRIMARY KEY, c counter) WITH default_time_to_live = 5` | `0x2200` Cannot set default_time_to_live on a table with counters | (same) |
| table_empty_columns | `CREATE TABLE e.x19 ()` | `0x2000` line 1:20 no viable alternative at input ')' | `0x2000` line 1:20 no viable alternative at input ')' (CREATE TABLE e.x19 ([)]) |
| table_with_id_dup | `CREATE TABLE e.x20 (pk int PRIMARY KEY) WITH ID = 5a1c395e-b41f-11e5-9f22-ba0be0483c18` | OK SchemaChange `["CREATED", "TABLE", "e", "x20"]` | (same) |
| table_in_memory_scylla | `CREATE TABLE e.x21 (pk int PRIMARY KEY) WITH in_memory = true` | `0x2000` Unknown property 'in_memory' | (same) |
| table_per_partition_rate_limit | `CREATE TABLE e.x22 (pk int PRIMARY KEY, v int) WITH per_partition_rate_limit = {'max_writes_per_second': 1}` | OK SchemaChange `["CREATED", "TABLE", "e", "x22"]` | `0x2000` Unknown property 'per_partition_rate_limit' |
| table_synchronous_updates | `CREATE TABLE e.x23 (pk int PRIMARY KEY) WITH synchronous_updates = true` | `0x2200` The synchronous_updates option is only applicable to materialized views, not to base tables | `0x2000` Unknown property 'synchronous_updates' |
| alter_add_existing | `ALTER TABLE e.t ADD v int` | `0x2200` Invalid column name v because it conflicts with an existing column | `0x2200` Column with name 'v' already exists |
| alter_add_if_not_exists | `ALTER TABLE e.t ADD IF NOT EXISTS v int` | `0x2000` line 1:20 no viable alternative at input 'IF' | OK Void |
| alter_add_static_no_ck | `ALTER TABLE e.c ADD s int static` | `0x2200` Static columns are only useful (and thus allowed) if the table has at least one clustering column | (same) |
| alter_add_counter_to_regular | `ALTER TABLE e.t ADD cc counter` | `0x2300` Cannot add a counter column (cc) in a non counter column family | `0x2300` e.t: Cannot have a counter column ("cc") in a non counter table |
| alter_add_multiple | `ALTER TABLE e.t ADD (n1 int, n2 text)` | OK SchemaChange `["UPDATED", "TABLE", "e", "t"]` | (same) |
| alter_drop_pk | `ALTER TABLE e.t DROP pk` | `0x2200` Cannot drop PRIMARY KEY part pk | `0x2200` Cannot drop PRIMARY KEY column pk |
| alter_drop_ck | `ALTER TABLE e.t DROP ck` | `0x2200` Cannot drop PRIMARY KEY part ck | `0x2200` Cannot drop PRIMARY KEY column ck |
| alter_drop_indexed | `ALTER TABLE e.ix DROP v` | `0x2200` Cannot drop column v from base table e.ix: materialized view ix_v_index needs this column | `0x2200` Cannot drop column v because it has dependent secondary indexes (ix_v) |
| alter_drop_if_exists | `ALTER TABLE e.t DROP IF EXISTS nope` | `0x2000` line 1:21 no viable alternative at input 'IF' | OK Void |
| alter_drop_with_timestamp | `ALTER TABLE e.t DROP n1 USING TIMESTAMP 1` | OK SchemaChange `["UPDATED", "TABLE", "e", "t"]` | (same) |
| alter_type_change | `ALTER TABLE e.t ALTER v TYPE text` | `0x2300` Cannot change v from type int to type text: types are incompatible. | `0x2200` Altering column types is no longer supported |
| alter_type_compatible | `ALTER TABLE e.t ALTER txt TYPE blob` | OK SchemaChange `["UPDATED", "TABLE", "e", "t"]` | `0x2200` Altering column types is no longer supported |
| alter_rename_regular | `ALTER TABLE e.t RENAME v TO w` | `0x2200` Cannot rename non PRIMARY KEY part v | `0x2200` Cannot rename non PRIMARY KEY column v |
| alter_rename_to_existing | `ALTER TABLE e.t RENAME ck TO v` | `0x2200` Cannot rename column ck to v in table t; another column of that name already exist | `0x2200` Cannot rename column ck to v in table 'e.t'; another column with that name already exists |
| alter_rename_pk_ok | `ALTER TABLE e.t2r RENAME a TO a2` | OK SchemaChange `["UPDATED", "TABLE", "e", "t2r"]` | (same) |
| alter_unknown_table | `ALTER TABLE e.nope ADD x int` | `0x2200` unconfigured table nope | `0x2200` Table 'e.nope' doesn't exist |
| alter_drop_unknown_column | `ALTER TABLE e.t DROP nope` | `0x2200` Column nope was not found in table t | `0x2200` Column nope was not found in table 'e.t' |
| alter_compact_storage_drop | `ALTER TABLE e.t DROP COMPACT STORAGE` | `0x2000` line 1:29 : Unexpected 'STORAGE' | `0x2200` DROP COMPACT STORAGE is disabled. Enable in cassandra.yaml to use. |
| alter_unknown_option | `ALTER TABLE e.t WITH nope = 1` | `0x2000` Unknown property 'nope' | (same) |
| alter_set_id | `ALTER TABLE e.t WITH ID = 5a1c395e-b41f-11e5-9f22-ba0be0483c18` | `0x2300` Cannot alter table id. | (same) |
| alter_counter_ttl | `ALTER TABLE e.c WITH default_time_to_live = 10` | `0x2200` Cannot set default_time_to_live on a table with counters | (same) |
| alter_unknown_keyspace | `ALTER KEYSPACE nope WITH durable_writes = true` | `0x2200` Can't find a keyspace nope | `0x2200` Keyspace 'nope' doesn't exist |
| alter_type_add_existing | `ALTER TYPE e.ty ADD x int` | `0x2200` Cannot add new field x to type e.ty: a field of the same name already exists | `0x2200` Cannot add field x to type e.ty: a field with name x already exists |
| alter_type_rename_unknown | `ALTER TYPE e.ty RENAME nope TO z` | `0x2200` Unknown field nope in type e.ty | `0x2200` Unkown field nope in user type e.ty |
| alter_type_unknown | `ALTER TYPE e.nope ADD z int` | `0x2200` No user type named e.nope exists. | `0x2200` Type e.nope doesn't exist |
| alter_type_change_field | `ALTER TYPE e.ty ALTER x TYPE text` | `0x2200` Type text in incompatible with previous type int of field x in user type e.ty | `0x2200` Altering field types is no longer supported |
| alter_mv_unknown | `ALTER MATERIALIZED VIEW e.nope WITH comment = 'x'` | `0x2200` unconfigured table nope | `0x2200` Materialized view 'e.nope' doesn't exist |
| drop_unknown_table | `DROP TABLE e.nope` | `0x2200` Cannot drop non existing table 'nope' in keyspace 'e'. | `0x2200` Table 'e.nope' doesn't exist |
| drop_unknown_table_if_exists | `DROP TABLE IF EXISTS e.nope` | OK Void | (same) |
| drop_unknown_keyspace | `DROP KEYSPACE nope` | `0x2300` Cannot drop non existing keyspace 'nope'. | `0x2200` Keyspace 'nope' doesn't exist |
| drop_unknown_index | `DROP INDEX e.nope` | `0x2200` Index 'nope' could not be found in any of the tables of keyspace 'e' | `0x2200` Index 'e.nope' doesn't exist' |
| drop_unknown_type | `DROP TYPE e.nope` | `0x2200` No user type named e.nope exists. | `0x2200` Type 'e.nope' doesn't exist |
| drop_unknown_function | `DROP FUNCTION e.nope` | `0x2200` User defined functions are disabled. Set enable_user_defined_functions and experimental_features:udf to enable them | `0x2200` Function 'e.nope' doesn't exist |
| drop_unknown_aggregate | `DROP AGGREGATE e.nope` | `0x2200` User defined functions are disabled. Set enable_user_defined_functions and experimental_features:udf to enable them | `0x2200` Aggregate 'e.nope' doesn't exist |
| drop_unknown_view | `DROP MATERIALIZED VIEW e.nope` | `0x2300` Cannot drop non existing materialized view 'nope' in keyspace 'e'. | `0x2200` Materialized view 'e.nope' doesn't exist |
| drop_unknown_trigger | `DROP TRIGGER nope ON e.t` | `0x2000` line 1:0 no viable alternative at input 'DROP' | `0x2200` Trigger 'nope' on 'e.t' doesn't exist |
| drop_type_in_use | `DROP TYPE e.ty` | `0x2200` Cannot drop user type e.ty as it is still used by table e.uses | `0x2200` Cannot drop user type 'e.ty' as it is still used by tables uses |
| drop_table_as_view | `DROP TABLE e.ix_v_index` | `0x2200` Cannot use DROP TABLE on Materialized View. (Did you mean DROP MATERIALIZED VIEW)? | `0x2200` Table 'e.ix_v_index' doesn't exist |
| use_unknown_keyspace | `USE nope` | `0x2200` Keyspace 'nope' does not exist | (same) |
| use_ok | `USE e` | OK SetKeyspace | (same) |
| truncate_unknown | `TRUNCATE e.nope` | `0x2200` unconfigured table nope | `0x2200` table nope does not exist |
| truncate_view | `TRUNCATE e.ix_v_index` | `0x2200` Cannot TRUNCATE materialized view directly; must truncate base table instead | `0x2200` table ix_v_index does not exist |
| trigger_create | `CREATE TRIGGER tr ON e.t USING 'org.example.Trigger'` | `0x2000` line 1:0 no viable alternative at input 'CREATE' | `0x2200` Trigger class 'org.example.Trigger' couldn't be loaded |
| udf_disabled | `CREATE FUNCTION e.f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE java AS 'return x;'` | `0x2200` User defined functions are disabled. Set enable_user_defined_functions and experimental_features:udf to enable them | `0x2200` User-defined functions are disabled in cassandra.yaml - set user_defined_functions_enabled=true to enable |
| udf_lua_disabled | `CREATE FUNCTION e.f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x'` | `0x2200` User defined functions are disabled. Set enable_user_defined_functions and experimental_features:udf to enable them | `0x2200` User-defined functions are disabled in cassandra.yaml - set user_defined_functions_enabled=true to enable |
| udf_unknown_language | `CREATE FUNCTION e.f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE cobol AS 'x'` | `0x2200` User defined functions are disabled. Set enable_user_defined_functions and experimental_features:udf to enable them | `0x2200` User-defined functions are disabled in cassandra.yaml - set user_defined_functions_enabled=true to enable |
| uda_unknown_sfunc | `CREATE AGGREGATE e.a(int) SFUNC nope STYPE int` | `0x2200` User defined functions are disabled. Set enable_user_defined_functions and experimental_features:udf to enable them | `0x2200` State function e.nope(int, int) doesn't exist |

## Materialized views, secondary indexes, SAI and vector indexes

| id | statement | ScyllaDB 2026.3.3 | Cassandra 5.0.9 |
|---|---|---|---|
| mv_missing_is_not_null | `CREATE MATERIALIZED VIEW e.mv1 AS SELECT * FROM e.ix WHERE v IS NOT NULL PRIMARY KEY (v, pk, ck)` | `0x2200` Primary key column 'ck' is required to be filtered by 'IS NOT NULL' | `0x2200` Materialized views are disabled. Enable in cassandra.yaml to use. |
| mv_on_counter | `CREATE MATERIALIZED VIEW e.mv2 AS SELECT * FROM e.c WHERE pk IS NOT NULL AND cnt IS NOT NULL PRIMARY KEY (cnt, pk)` | `0x2200` Materialized views are not supported on counter tables | `0x2200` Materialized views are disabled. Enable in cassandra.yaml to use. |
| mv_missing_base_pk | `CREATE MATERIALIZED VIEW e.mv3 AS SELECT pk, ck, v FROM e.ix WHERE v IS NOT NULL AND pk IS NOT NULL PRIMARY KEY (v, pk)` | `0x2200` Cannot create Materialized View mv3 without primary key columns from base ix (ck) | `0x2200` Materialized views are disabled. Enable in cassandra.yaml to use. |
| mv_static | `CREATE MATERIALIZED VIEW e.mv4 AS SELECT pk, ck, s FROM e.t WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)` | `0x2200` Unable to include static column 's' which would be included by Materialized View SELECT * statement | `0x2200` Materialized views are disabled. Enable in cassandra.yaml to use. |
| mv_select_star_static | `CREATE MATERIALIZED VIEW e.mv4b AS SELECT * FROM e.t WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)` | `0x2200` Unable to include static column 's' which would be included by Materialized View SELECT * statement | `0x2200` Materialized views are disabled. Enable in cassandra.yaml to use. |
| mv_two_regular_in_pk | `CREATE MATERIALIZED VIEW e.mv5 AS SELECT * FROM e.t2 WHERE a IS NOT NULL AND b IS NOT NULL AND c IS NOT NULL AND d IS NOT NULL AND v IS NOT NULL PRIMARY KEY ((v, d), a, b, c)` | OK SchemaChange `["CREATED", "TABLE", "e", "mv5"]`; warnings: "Creating a materialized view in a keyspaces that uses tablets requires the keyspace to remain RF-rack-valid while the materialized view exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | `0x2200` Materialized views are disabled. Enable in cassandra.yaml to use. |
| mv_collection_in_pk | `CREATE MATERIALIZED VIEW e.mv6 AS SELECT pk, ck, fl FROM e.t WHERE pk IS NOT NULL AND ck IS NOT NULL AND fl IS NOT NULL PRIMARY KEY (fl, pk, ck)` | OK SchemaChange `["CREATED", "TABLE", "e", "mv6"]`; warnings: "Creating a materialized view in a keyspaces that uses tablets requires the keyspace to remain RF-rack-valid while the materialized view exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | `0x2200` Materialized views are disabled. Enable in cassandra.yaml to use. |
| mv_unknown_base | `CREATE MATERIALIZED VIEW e.mv7 AS SELECT * FROM e.nope WHERE pk IS NOT NULL PRIMARY KEY (pk)` | `0x2200` unconfigured table nope | `0x2200` Materialized views are disabled. Enable in cassandra.yaml to use. |
| mv_ttl_option | `CREATE MATERIALIZED VIEW e.mv9 AS SELECT * FROM e.ix WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk) WITH default_time_to_live = 5` | `0x2200` Cannot set or alter default_time_to_live for a materialized view. Data in a materialized view always expire at the same time than the corresponding data in the parent table. | `0x2200` Materialized views are disabled. Enable in cassandra.yaml to use. |
| mv_filter_regular | `CREATE MATERIALIZED VIEW e.mv10 AS SELECT * FROM e.ix WHERE pk IS NOT NULL AND ck IS NOT NULL AND v = 1 PRIMARY KEY (ck, pk)` | `0x2200` Non-primary key columns cannot be restricted in the SELECT statement used for materialized view mv10 creation (got restrictions on: v) | `0x2200` Materialized views are disabled. Enable in cassandra.yaml to use. |
| mv_ok | `CREATE MATERIALIZED VIEW e.mvok AS SELECT * FROM e.ix WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)` | OK SchemaChange `["CREATED", "TABLE", "e", "mvok"]`; warnings: "Creating a materialized view in a keyspaces that uses tablets requires the keyspace to remain RF-rack-valid while the materialized view exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | `0x2200` Materialized views are disabled. Enable in cassandra.yaml to use. |
| mv_write_to_view | `INSERT INTO e.mvok (ck, pk, v) VALUES (1, 1, 1)` | `0x2200` Cannot directly modify a materialized view | `0x2200` table mvok does not exist |
| mv_drop_base_with_view | `DROP TABLE e.ix` | `0x2200` Cannot drop table when materialized views still depend on it (e.{mvok}) | OK SchemaChange `["DROPPED", "TABLE", "e", "ix"]` |
| mv_drop_view_as_table | `DROP TABLE e.mvok` | `0x2200` Cannot use DROP TABLE on Materialized View. (Did you mean DROP MATERIALIZED VIEW)? | `0x2200` Table 'e.mvok' doesn't exist |
| mv_alter_view_add | `ALTER TABLE e.mvok ADD z int` | `0x2200` Cannot use ALTER TABLE on Materialized View. (Did you mean ALTER MATERIALIZED VIEW)? | `0x2200` Table 'e.mvok' doesn't exist |
| mv_index_on_view | `CREATE INDEX ON e.mvok (v)` | `0x2200` Secondary indexes are not supported on materialized views | `0x2200` Table 'mvok' doesn't exist |
| mv_drop_base_column | `ALTER TABLE e.ix DROP v` | `0x2200` Cannot drop column v from base table e.ix: materialized view ix_v_index needs this column | `0x2200` Table 'e.ix' doesn't exist |
| index_on_only_pk | `CREATE INDEX ON e.vec (pk)` | `0x2200` Cannot create secondary index on partition key column pk | `0x2200` Cannot create secondary index on the only partition key column pk |
| index_on_counter | `CREATE INDEX ON e.c (cnt)` | `0x2200` Secondary indexes are not supported on counter tables | `0x2200` Secondary indexes on counter tables aren't supported |
| index_keys_on_list | `CREATE INDEX ON e.t (KEYS(l))` | `0x2200` Cannot create secondary index on keys of column l with non-map type | `0x2200` Cannot create index on keys of column l with non-map type |
| index_entries_on_set | `CREATE INDEX ON e.t (ENTRIES(st))` | `0x2200` Cannot create secondary index on entries of column st with non-map type | `0x2200` Cannot create index on entries of column st with non-map type |
| index_full_on_nonfrozen | `CREATE INDEX ON e.t (FULL(l))` | `0x2200` full() indexes can only be created on frozen collections | (same) |
| index_values_on_frozen | `CREATE INDEX ON e.t (VALUES(fl))` | `0x2200` Cannot create index on values() of frozen collection column fl | `0x2200` Cannot create values() index on frozen column fl. Frozen collections are immutable and must be fully indexed by using the 'full(fl)' modifier |
| index_on_duration | `CREATE INDEX ON e.u (du)` | `0x2200` Secondary indexes are not supported on duration columns | (same) |
| index_unknown_column | `CREATE INDEX ON e.t (nope)` | `0x2200` No column definition found for column nope | `0x2200` Undefined column name nope in table e.t |
| index_on_static | `CREATE INDEX ON e.t (s)` | OK SchemaChange `["UPDATED", "TABLE", "e", "t"]`; warnings: "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | OK SchemaChange `["UPDATED", "TABLE", "e", "t"]` |
| index_on_whole_pk_component | `CREATE INDEX ON e.t2 (a)` | OK SchemaChange `["UPDATED", "TABLE", "e", "t2"]`; warnings: "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | OK SchemaChange `["UPDATED", "TABLE", "e", "t2"]` |
| index_custom_unknown_class | `CREATE CUSTOM INDEX ON e.t (txt) USING 'com.example.Nope'` | `0x2200` Non-supported custom class 'com.example.Nope' provided | `0x2300` Unable to find custom indexer class 'com.example.Nope' |
| index_custom_no_using | `CREATE CUSTOM INDEX ON e.t (txt)` | `0x2200` CUSTOM index requires specifying the index class | `0x2300` Unable to find custom indexer class 'legacy_local_table' |
| index_options_without_custom | `CREATE INDEX ON e.t (txt) WITH OPTIONS = {'a': 'b'}` | `0x2200` Cannot specify options for a non-CUSTOM index | (same) |
| sai_on_regular | `CREATE INDEX ON e.t2 (v) USING 'sai'` | `0x2200` StorageAttachedIndex (SAI) is only supported on vector columns; use a secondary index for non-vector columns | OK SchemaChange `["UPDATED", "TABLE", "e", "t2"]` |
| sai_on_collection | `CREATE INDEX ON e.t (l) USING 'sai'` | `0x2200` StorageAttachedIndex (SAI) is only supported on vector columns; use a secondary index for non-vector columns | OK SchemaChange `["UPDATED", "TABLE", "e", "t"]` |
| sai_unknown_option | `CREATE INDEX ON e.t2 (v) USING 'sai' WITH OPTIONS = {'nope': '1'}` | `0x2200` StorageAttachedIndex (SAI) is only supported on vector columns; use a secondary index for non-vector columns | `0x2200` Cannot create more than one storage-attached index on the same column: v |
| sai_on_vector_ok | `CREATE INDEX vec_sai ON e.vec (emb) USING 'sai'` | OK SchemaChange `["UPDATED", "TABLE", "e", "vec"]` | OK SchemaChange `["UPDATED", "TABLE", "e", "vec"]`; warnings: "SAI ANN indexes on vector columns are experimental and are not recommended for production use.\nThey don't yet support SELECT queries with:\n * Consistency level higher than ONE/LOCAL_ONE.\n * Paging.\n * No LIMIT clauses.\n * PER PARTITION LIMIT clauses.\n * GROUP BY clauses.\n * Aggregation functions.\n * Filters on columns without a SAI index." |
| sai_on_vector_bad_similarity | `CREATE INDEX vec_sai2 ON e.vec (emb) USING 'sai' WITH OPTIONS = {'similarity_function': 'NOPE'}` | `0x2200` Invalid value in option 'similarity_function' for vector index: 'NOPE'. Supported are case-insensitive: cosine, euclidean, dot_product | `0x2200` Cannot create more than one storage-attached index on the same column: emb |
| vector_index_scylla_ok | `CREATE CUSTOM INDEX vec_vs ON e.vec (emb) USING 'vector_index'` | OK SchemaChange `["UPDATED", "TABLE", "e", "vec"]` | `0x2300` Unable to find custom indexer class 'vector_index' |
| vector_index_scylla_bad_option | `CREATE CUSTOM INDEX vec_vs2 ON e.vec (emb) USING 'vector_index' WITH OPTIONS = {'similarity_function': 'NOPE'}` | `0x2200` Invalid value in option 'similarity_function' for vector index: 'NOPE'. Supported are case-insensitive: cosine, euclidean, dot_product | `0x2300` Unable to find custom indexer class 'vector_index' |
| ann_with_index | `SELECT * FROM e.vec ORDER BY emb ANN OF [1, 2, 3] LIMIT 1` | `0x2200` Vector Store is disabled | OK Rows |
| ann_with_index_no_limit | `SELECT * FROM e.vec ORDER BY emb ANN OF [1, 2, 3]` | `0x2200` Vector ANN queries must have a limit specified | `0x2200` Top-K queries must have a limit specified and the limit must be less than the query page size |
| ann_with_filter | `SELECT * FROM e.vec WHERE pk = 1 ORDER BY emb ANN OF [1, 2, 3] LIMIT 1` | `0x2200` Vector Store is disabled | OK Rows |
| sasi_disabled | `CREATE CUSTOM INDEX ON e.t (txt) USING 'org.apache.cassandra.index.sasi.SASIIndex'` | `0x2200` Non-supported custom class 'org.apache.cassandra.index.sasi.SASIIndex' provided | `0x2200` SASI indexes are disabled. Enable in cassandra.yaml to use. |
| drop_index_on_view_name | `DROP INDEX e.mvok` | `0x2200` Index 'mvok' could not be found in any of the tables of keyspace 'e' | `0x2200` Index 'e.mvok' doesn't exist' |

## Syntax errors

| id | statement | ScyllaDB 2026.3.3 | Cassandra 5.0.9 |
|---|---|---|---|
| typo_keyword | `SELEC * FROM e.t` | `0x2000` line 1:0 no viable alternative at input 'SELEC' | `0x2000` line 1:0 no viable alternative at input 'SELEC' ([SELEC]...) |
| where_eof | `SELECT * FROM e.t WHERE` | `0x2000` line 1:0 no viable alternative at input '<EOF>' | `0x2000` line 0:-1 no viable alternative at input '<EOF>' |
| eq_eof | `SELECT * FROM e.t WHERE pk =` | `0x2000` line 1:0 no viable alternative at input '<EOF>' | `0x2000` line 0:-1 no viable alternative at input '<EOF>' |
| limit_string | `SELECT * FROM e.t LIMIT 'a'` | `0x2000` line 1:24 no viable alternative at input 'a' | `0x2000` line 1:24 no viable alternative at input 'a' (...* FROM e.t LIMIT [']...) |
| trailing_comma_ddl | `CREATE TABLE e.x30 (pk int PRIMARY KEY,)` | OK SchemaChange `["CREATED", "TABLE", "e", "x30"]` | (same) |
| allow_eof | `SELECT * FROM e.t WHERE pk = 1 ALLOW` | `0x2000` line 1:0 : Missing K_FILTERING | `0x2000` line 0:-1 mismatched input '<EOF>' expecting K_FILTERING |
| double_semicolon | `SELECT * FROM e.t;;` | OK Rows | (same) |
| semicolon_only | `;` | `0x2000` line 1:0 no viable alternative at input ';' | `0x2000` line 1:0 no viable alternative at input ';' ([;]) |
| unterminated_string | `SELECT * FROM e.t WHERE txt = 'abc` | `0x2000` line 1:34 : Syntax error | `0x2000` line 1:34 mismatched character '<EOF>' expecting ''' |
| unterminated_quoted_ident | `SELECT * FROM e."t` | `0x2000` line 1:18 : Syntax error | `0x2000` line 1:18 mismatched character '<EOF>' expecting '"' |
| unterminated_dollar_string | `SELECT * FROM e.t WHERE txt = $$abc` | `0x2000` line 1:35 : Syntax error | `0x2000` line 1:34 mismatched character 'c' expecting '$' |
| and_and | `SELECT * FROM e.t WHERE pk = 1 AND AND ck = 1` | `0x2000` line 1:35 no viable alternative at input 'AND' | `0x2000` line 1:35 no viable alternative at input 'AND' (...WHERE pk = 1 AND [AND]...) |
| quoted_unknown_table | `SELECT * FROM e."T"` | `0x2200` unconfigured table T | `0x2200` table T does not exist |
| empty | `` | `0x2000` line 1:0 no viable alternative at input '<EOF>' | `0x2000` line 0:-1 no viable alternative at input '<EOF>' |
| whitespace_only | `   ` | `0x2000` line 1:0 no viable alternative at input '<EOF>' | `0x2000` line 0:-1 no viable alternative at input '<EOF>' |
| comment_only | `-- just a comment` | `0x2000` line 1:17 : Unexpected '' | `0x2000` line 1:17 mismatched character '<EOF>' expecting set null |
| bad_hex | `SELECT * FROM e.t WHERE pk = 0x` | `0x2200` Invalid HEX constant (0x) for "pk" of type int | (same) |
| bad_float | `SELECT * FROM e.t WHERE pk = 1e` | `0x2000` line 1:31 no viable alternative at input '' | `0x2000` line 1:31 no viable alternative at character '<EOF>' |
| reserved_table_name | `CREATE TABLE e.select (a int PRIMARY KEY)` | `0x2000` line 1:15 no viable alternative at input 'select' | `0x2000` line 1:15 no viable alternative at input 'select' (CREATE TABLE e.[select]...) |
| reserved_column_name | `CREATE TABLE e.x31 (from int PRIMARY KEY)` | `0x2000` line 1:20 no viable alternative at input 'from' | `0x2000` line 1:20 no viable alternative at input 'from' (CREATE TABLE e.x31 ([from]...) |
| unreserved_keyword_ok | `CREATE TABLE e.x32 (key int PRIMARY KEY, type text, ttl int)` | OK SchemaChange `["CREATED", "TABLE", "e", "x32"]` | (same) |
| unknown_statement | `FROBNICATE e.t` | `0x2000` line 1:0 no viable alternative at input 'FROBNICATE' | `0x2000` line 1:0 no viable alternative at input 'FROBNICATE' ([FROBNICATE]...) |
| missing_from | `SELECT * e.t` | `0x2000` line 1:9 : Missing K_FROM | `0x2000` line 1:9 mismatched input 'e' expecting K_FROM (SELECT * [e]...) |
| garbage_after | `SELECT * FROM e.t garbage` | `0x2000` line 1:18 : Unexpected 'garbage' | `0x2000` line 1:18 mismatched input 'garbage' expecting EOF (SELECT * FROM e.t [garbage]) |
| bad_collection_literal | `INSERT INTO e.t (pk, ck, l) VALUES (1, 1, [1, 2)` | `0x2000` line 1:47 : Missing ']' | `0x2000` line 1:47 mismatched input ')' expecting ']' |
| nested_unterminated_comment | `SELECT * FROM e.t /* unterminated` | `0x2000` line 1:33 : Syntax error | `0x2000` line 1:33 mismatched character '<EOF>' expecting '*' |
| non_ascii_identifier | `SELECT * FROM e.tä` | `0x2000` line 1:17 no viable alternative at input '' | `0x2000` line 1:17 no viable alternative at character 'ä' |
| select_star_and_column | `SELECT *, v FROM e.t` | `0x2000` line 1:8 : Syntax error | `0x2000` line 1:8 mismatched input ',' expecting K_FROM (SELECT *[,]...) |
| insert_no_values | `INSERT INTO e.t (pk, ck)` | `0x2000` line 1:0 : Syntax error | `0x2000` line 0:-1 mismatched input '<EOF>' expecting K_VALUES |
| update_no_set | `UPDATE e.t WHERE pk = 1` | `0x2000` line 1:11 : Syntax error | `0x2000` line 1:11 mismatched input 'WHERE' expecting K_SET (UPDATE e.t [WHERE]...) |
| delete_no_where | `DELETE FROM e.t` | `0x2000` line 1:0 : Syntax error | `0x2000` line 0:-1 mismatched input '<EOF>' expecting K_WHERE |
| update_no_where | `UPDATE e.t SET v = 1` | `0x2000` line 1:0 : Syntax error | `0x2000` line 0:-1 mismatched input '<EOF>' expecting K_WHERE |
| create_table_no_columns_parens | `CREATE TABLE e.x33` | `0x2000` line 1:0 : Syntax error | `0x2000` line 0:-1 mismatched input '<EOF>' expecting '(' |
| create_keyspace_with_eof | `CREATE KEYSPACE x WITH` | `0x2000` line 1:0 no viable alternative at input '<EOF>' | `0x2000` line 0:-1 no viable alternative at input '<EOF>' |
| in_without_parens | `SELECT * FROM e.t WHERE pk IN 1` | `0x2000` line 1:27 no viable alternative at input 'IN' | `0x2000` line 1:30 no viable alternative at input '1' (...* FROM e.t WHERE [pk] IN...) |
| double_where | `SELECT * FROM e.t WHERE pk = 1 WHERE ck = 1` | `0x2000` line 1:31 : Syntax error | `0x2000` line 1:31 mismatched input 'WHERE' expecting EOF (...t WHERE pk = 1 [WHERE] ck...) |
| limit_before_where | `SELECT * FROM e.t LIMIT 1 WHERE pk = 1` | `0x2000` line 1:26 : Syntax error | `0x2000` line 1:26 mismatched input 'WHERE' expecting EOF (...FROM e.t LIMIT 1 [WHERE]...) |
| order_without_by | `SELECT * FROM e.t WHERE pk = 1 ORDER ck` | `0x2000` line 1:37 : Missing K_BY | `0x2000` line 1:37 mismatched input 'ck' expecting K_BY (...WHERE pk = 1 ORDER [ck]) |
| integer_too_large_literal | `SELECT * FROM e.t WHERE pk = 99999999999999999999999999` | `0x2200` marshaling error: Invalid number format '99999999999999999999999999' | `0x2200` Unable to make int from '99999999999999999999999999' |
| negative_hex | `SELECT * FROM e.t WHERE pk = -0x01` | `0x2000` line 1:31 : Unexpected 'x01' | `0x2000` line 1:31 no viable alternative at input 'x01' (....t WHERE pk = -0[x01]) |
| uuid_malformed_literal | `SELECT * FROM e.u WHERE id = 1 AND u = 123e4567-e89b-12d3-a456` | `0x2000` line 1:62 : Syntax error | `0x2000` line 1:62 mismatched character '<EOF>' expecting '-' |
| duration_literal_bad | `INSERT INTO e.u (id, du) VALUES (1, 1hx)` | `0x2000` line 1:38 : Unexpected 'x' | `0x2000` line 1:38 no viable alternative at input 'x' (...du) VALUES (1, 1h[x]...) |
| lowercase_keywords | `select * from e.t where pk = 1` | OK Rows | (same) |
| mixed_case_table | `SELECT * FROM E.T` | OK Rows | (same) |
| colon_named_and_positional | `SELECT * FROM e.t WHERE pk = ? AND ck = :c` | `0x2200` Invalid amount of bind variables: expected 2 received 0 | `0x2200` Invalid amount of bind variables |
| long_identifier | `CREATE TABLE e.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa… (336 chars)` | `0x2200` Table names shouldn't be more than 192 characters long (got "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa") | `0x2300` e.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa: Table name must not be more than 222 characters long (got 300 characters for "aaaaaaaaaaaaaaaaaa… (684 chars) |
| very_long_query | `SELECT * FROM e.t WHERE pk IN (1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1… (10031 chars)` | OK Rows | (same) |
| deep_nesting | `SELECT * FROM e.t WHERE pk = (((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((((… (430 chars)` | `0x2000` expression nested too deeply | OK Rows |
| grant_bad_permission | `GRANT NOPE ON KEYSPACE e TO r` | `0x2000` line 1:11 : Syntax error | `0x2000` line 1:11 mismatched input 'ON' expecting K_TO (GRANT NOPE [ON]...) |
| use_quoted | `USE "E"` | `0x2200` Keyspace 'E' does not exist | (same) |

## Warnings and guardrails

| id | statement | ScyllaDB 2026.3.3 | Cassandra 5.0.9 |
|---|---|---|---|
| agg_no_pk | `SELECT count(*) FROM e.t` | OK Rows | OK Rows; warnings: "Aggregation query used without partition key" |
| agg_with_pk | `SELECT count(*) FROM e.t WHERE pk = 1` | OK Rows | (same) |
| agg_with_in_pk | `SELECT count(*) FROM e.t WHERE pk IN (1, 2)` | OK Rows | OK Rows; warnings: "Aggregation query used on multiple partition keys (IN restriction)" |
| agg_group_by_pk | `SELECT pk, count(*) FROM e.t GROUP BY pk` | OK Rows | OK Rows; warnings: "Aggregation query used without partition key" |
| unlogged_multi_partition_11 | `BEGIN UNLOGGED BATCH INSERT INTO e.t (pk, ck, v) VALUES (0, 1, 1); INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1); INSERT INTO e.t (pk, ck, v) VALUES (2, 1, 1); INSERT INTO e.t (pk, ck, v) VALUES (3, 1,… (539 chars)` | OK Void | OK Void; warnings: "Unlogged batch covering 11 partitions detected against table [e.t]. You should use a logged batch for atomicity, or asynchronous writes for performance." |
| unlogged_multi_partition_10 | `BEGIN UNLOGGED BATCH INSERT INTO e.t (pk, ck, v) VALUES (0, 1, 1); INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1); INSERT INTO e.t (pk, ck, v) VALUES (2, 1, 1); INSERT INTO e.t (pk, ck, v) VALUES (3, 1,… (492 chars)` | OK Void | (same) |
| logged_multi_partition_12 | `BEGIN BATCH INSERT INTO e.t (pk, ck, v) VALUES (0, 1, 1); INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1); INSERT INTO e.t (pk, ck, v) VALUES (2, 1, 1); INSERT INTO e.t (pk, ck, v) VALUES (3, 1, 1); INSE… (577 chars)` | OK Void | (same) |
| batch_size_4kb | `logged batch of 4 inserts of a 1 KiB text value into one partition` | OK Void | (same) |
| batch_size_6kb | `logged batch of 6 inserts of a 1 KiB text value into one partition` | OK Void | (same) |
| batch_size_49kb | `logged batch of 49 inserts of a 1 KiB text value into one partition` | OK Void | (same) |
| batch_size_60kb | `logged batch of 60 inserts of a 1 KiB text value into one partition` | OK Void | (same) |
| batch_size_129kb | `logged batch of 129 inserts of a 1 KiB text value into one partition` | OK Void | (same) |
| batch_size_1100kb | `logged batch of 1100 inserts of a 1 KiB text value into one partition` | OK Void | (same) |
| batch_6_partitions_6kb | `logged batch, 6 x 1 KiB, 6 partitions` | OK Void | OK Void; warnings: "Batch for [e.big] is of size 6588, exceeding specified threshold of 5120 by 1468." |
| batch_40_partitions_40kb | `logged batch, 40 x 1 KiB, 40 partitions` | OK Void | OK Void; warnings: "Batch for [e.big] is of size 43920, exceeding specified threshold of 5120 by 38800." |
| unlogged_batch_size_60kb | `unlogged batch, 60 x 1 KiB, one partition` | OK Void | (same) |
| unlogged_batch_size_60kb_multi | `unlogged batch, 60 x 1 KiB, 60 partitions` | OK Void | `0x2200` Batch too large |
| tombstones_1100 | `SELECT * FROM e.tomb WHERE pk = 1` | OK Rows | OK Rows; warnings: "Read 0 live rows and 1100 tombstone cells for query SELECT * FROM e.tomb WHERE pk = 1 LIMIT 5000 ALLOW FILTERING; token -4069959284402364209 (see tombstone_warn_threshold)" |
| tombstones_100100 | `SELECT * FROM e.tomb WHERE pk = 2` | OK Rows | `0x1300` Operation failed - received 0 responses and 1 failures: READ_TOO_MANY_TOMBSTONES from <hostname>/<ip>:7000 (extra bytes `000100000000000000010000000100`) |
| tombstones_100100_unpaged | `SELECT * FROM e.tomb WHERE pk = 2` | OK Rows | `0x1300` Operation failed - received 0 responses and 1 failures: READ_TOO_MANY_TOMBSTONES from <hostname>/<ip>:7000 (extra bytes `000100000000000000010000000100`) |
| select_unpaged_filtering | `SELECT * FROM e.t WHERE v = 1 ALLOW FILTERING` | OK Rows | (same) |
| simple_strategy_ks | `CREATE KEYSPACE w1 WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1}` | `0x2300` SimpleStrategy doesn't support tablet replication | OK SchemaChange `["CREATED", "KEYSPACE", "w1"]` |
| simple_strategy_ks_vnodes | `CREATE KEYSPACE w2 WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1} AND tablets = {'enabled': false}` | OK SchemaChange `["CREATED", "KEYSPACE", "w2"]`; warnings: "SimpleStrategy replication class is not recommended, but was used for keyspace w2. You may suppress this warning by delisting SimpleStrategy from replication_strategy_warn_list configuration option, or make it into an error by listing this replication strategy on replication_strategy_fail_list." / "Using Replication Factor replication_factor=1 lower than the minimum_replication_factor_warn_threshold=3 is not recommended." | `0x2000` Unknown property 'tablets' |
| mv_create | `CREATE MATERIALIZED VIEW e.mvw AS SELECT * FROM e.t2 WHERE v IS NOT NULL AND a IS NOT NULL AND b IS NOT NULL AND c IS NOT NULL AND d IS NOT NULL PRIMARY KEY (v, a, b, c, d)` | OK SchemaChange `["CREATED", "TABLE", "e", "mvw"]`; warnings: "Creating a materialized view in a keyspaces that uses tablets requires the keyspace to remain RF-rack-valid while the materialized view exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | `0x2200` Materialized views are disabled. Enable in cassandra.yaml to use. |
| index_create | `CREATE INDEX ix_w ON e.t2 (v)` | OK SchemaChange `["UPDATED", "TABLE", "e", "t2"]`; warnings: "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack." | OK SchemaChange `["UPDATED", "TABLE", "e", "t2"]` |
| in_many_200 | `SELECT * FROM e.t WHERE pk IN (0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59… (721 chars)` | `0x0000` size of partition-key IN list or partition-key cartesian product of IN list 200 is greater than maximum 100. Use --max-partition-key-restrictions-per-query configuration option to change default setting. | OK Rows |
| in_many_100 | `SELECT * FROM e.t WHERE pk IN (0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59… (321 chars)` | OK Rows | (same) |
| in_cartesian | `SELECT * FROM e.t2 WHERE a IN (0,1,2,3,4,5,6,7,8,9,10) AND b IN (0,1,2,3,4,5,6,7,8,9,10)` | `0x0000` size of partition-key IN list or partition-key cartesian product of IN list 121 is greater than maximum 100. Use --max-partition-key-restrictions-per-query configuration option to change default setting. | OK Rows |
| collection_large | `INSERT INTO e.t (pk, ck, l) VALUES (3, 3, [1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1… (140044 chars)` | OK Void | (same) |
| large_text_cell | `INSERT INTO e.big (pk, ck, v) VALUES (7, 7, 'yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy… (1100047 chars)` | OK Void | (same) |
| drop_ks_w1 | `DROP KEYSPACE IF EXISTS w1` | OK Void | OK SchemaChange `["DROPPED", "KEYSPACE", "w1"]` |
| drop_ks_w2 | `DROP KEYSPACE IF EXISTS w2` | OK SchemaChange `["DROPPED", "KEYSPACE", "w2"]` | OK Void |
| ks_rf3_default | `CREATE KEYSPACE e3t WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 3}` | OK SchemaChange `["CREATED", "KEYSPACE", "e3t"]`; warnings: "Keyspace 'e3t' is not RF-rack-valid: the replication factor doesn't match the rack count in at least one datacenter. A rack failure may reduce availability. For more context, see: https://docs.scylladb.com/manual/stable/reference/glossary.html#term-RF-rack-valid-keyspace." | OK SchemaChange `["CREATED", "KEYSPACE", "e3t"]`; warnings: "Your replication factor 3 for keyspace e3t is higher than the number of nodes 1 for datacenter datacenter1" |

## Consistency levels and replication

| id | statement | ScyllaDB 2026.3.3 | Cassandra 5.0.9 |
|---|---|---|---|
| ks_rf3_default_table | `CREATE TABLE e3t.t (pk int PRIMARY KEY, v int)` | `0x2300` Datacenter datacenter1 doesn't have enough token-owning nodes for replication_factor=3 | OK SchemaChange `["CREATED", "TABLE", "e3t", "t"]` |
| rf3_write_ONE | `INSERT INTO e3.t (pk, v) VALUES (1, 1)` | OK Void | (same) |
| rf3_write_TWO | `INSERT INTO e3.t (pk, v) VALUES (1, 1)` | `0x1000` Cannot achieve consistency level for cl TWO. Requires 2, alive 1 (extra: `{"consistency": 2, "required": 2, "alive": 1}`) | `0x1000` Cannot achieve consistency level TWO (extra: `{"consistency": 2, "required": 2, "alive": 1}`) |
| rf3_write_QUORUM | `INSERT INTO e3.t (pk, v) VALUES (1, 1)` | `0x1000` Cannot achieve consistency level for cl QUORUM. Requires 2, alive 1 (extra: `{"consistency": 4, "required": 2, "alive": 1}`) | `0x1000` Cannot achieve consistency level QUORUM (extra: `{"consistency": 4, "required": 2, "alive": 1}`) |
| rf3_write_ALL | `INSERT INTO e3.t (pk, v) VALUES (1, 1)` | `0x1000` Cannot achieve consistency level for cl ALL. Requires 3, alive 1 (extra: `{"consistency": 5, "required": 3, "alive": 1}`) | `0x1000` Cannot achieve consistency level ALL (extra: `{"consistency": 5, "required": 3, "alive": 1}`) |
| rf3_write_LOCAL_QUORUM | `INSERT INTO e3.t (pk, v) VALUES (1, 1)` | `0x1000` Cannot achieve consistency level for cl LOCAL_QUORUM. Requires 2, alive 1 (extra: `{"consistency": 6, "required": 2, "alive": 1}`) | `0x1000` Cannot achieve consistency level LOCAL_QUORUM (extra: `{"consistency": 6, "required": 2, "alive": 1}`) |
| rf3_write_EACH_QUORUM | `INSERT INTO e3.t (pk, v) VALUES (1, 1)` | `0x1000` Cannot achieve consistency level for cl EACH_QUORUM. Requires 2, alive 1 (extra: `{"consistency": 7, "required": 2, "alive": 1}`) | `0x1000` Cannot achieve consistency level EACH_QUORUM in DC datacenter1 (extra: `{"consistency": 7, "required": 2, "alive": 0}`) |
| rf3_write_LOCAL_ONE | `INSERT INTO e3.t (pk, v) VALUES (1, 1)` | OK Void | (same) |
| rf3_write_ANY | `INSERT INTO e3.t (pk, v) VALUES (1, 1)` | OK Void | (same) |
| rf3_read_ONE | `SELECT * FROM e3.t WHERE pk = 1` | OK Rows | (same) |
| rf3_read_QUORUM | `SELECT * FROM e3.t WHERE pk = 1` | `0x1000` Cannot achieve consistency level for cl QUORUM. Requires 2, alive 1 (extra: `{"consistency": 4, "required": 2, "alive": 1}`) | `0x1000` Cannot achieve consistency level QUORUM (extra: `{"consistency": 4, "required": 2, "alive": 1}`) |
| rf3_read_ALL | `SELECT * FROM e3.t WHERE pk = 1` | `0x1000` Cannot achieve consistency level for cl ALL. Requires 3, alive 1 (extra: `{"consistency": 5, "required": 3, "alive": 1}`) | `0x1000` Cannot achieve consistency level ALL (extra: `{"consistency": 5, "required": 3, "alive": 1}`) |
| rf3_read_LOCAL_QUORUM | `SELECT * FROM e3.t WHERE pk = 1` | `0x1000` Cannot achieve consistency level for cl LOCAL_QUORUM. Requires 2, alive 1 (extra: `{"consistency": 6, "required": 2, "alive": 1}`) | `0x1000` Cannot achieve consistency level LOCAL_QUORUM (extra: `{"consistency": 6, "required": 2, "alive": 1}`) |
| rf3_read_EACH_QUORUM | `SELECT * FROM e3.t WHERE pk = 1` | `0x2200` EACH_QUORUM ConsistencyLevel is only supported for writes | `0x1000` Cannot achieve consistency level EACH_QUORUM in DC datacenter1 (extra: `{"consistency": 7, "required": 2, "alive": 0}`) |
| rf3_read_SERIAL | `SELECT * FROM e3.t WHERE pk = 1` | OK Rows | `0x1000` Cannot achieve consistency level QUORUM (extra: `{"consistency": 4, "required": 2, "alive": 1}`) |
| rf3_lwt | `INSERT INTO e3.t (pk, v) VALUES (2, 1) IF NOT EXISTS` | OK Rows | `0x1000` Cannot achieve consistency level QUORUM (extra: `{"consistency": 4, "required": 2, "alive": 1}`) |
| rf3_lwt_local_serial | `INSERT INTO e3.t (pk, v) VALUES (2, 1) IF NOT EXISTS` | OK Rows | `0x1000` Cannot achieve consistency level LOCAL_QUORUM (extra: `{"consistency": 6, "required": 2, "alive": 1}`) |
| rf1_each_quorum_read | `SELECT * FROM e.t WHERE pk = 1` | `0x2200` EACH_QUORUM ConsistencyLevel is only supported for writes | `0x1000` Cannot achieve consistency level EACH_QUORUM in DC datacenter1 (extra: `{"consistency": 7, "required": 2, "alive": 0}`) |
| rf1_any_read | `SELECT * FROM e.t WHERE pk = 1` | `0x2200` ANY ConsistencyLevel is only supported for writes | (same) |
| rf1_counter_any | `UPDATE e.c SET cnt = cnt + 1 WHERE pk = 1` | `0x2200` Consistency level ANY is not yet supported for counter table c | (same) |
| truncate_rf3_all | `TRUNCATE e3.t` | OK Void | (same) |

## Functions

| id | statement | ScyllaDB 2026.3.3 | Cassandra 5.0.9 |
|---|---|---|---|
| blobasint_wrong_length | `SELECT blobAsInt(0x0102) FROM e.t WHERE pk = 1` | `0x2200` In call to function blobasint, value 0x0102 is not a valid binary representation for type int | `0x2200` In call to function system.blobasint, value 0x0102 is not a valid binary representation for type int |
| blobasbigint_empty | `SELECT blobAsBigint(0x) FROM e.t WHERE pk = 1` | OK Rows | (same) |
| blobastext_invalid_utf8 | `SELECT blobAsText(0xff) FROM e.t WHERE pk = 1` | `0x2200` In call to function blobastext, value 0xff is not a valid binary representation for type text | `0x2200` In call to function system.blobastext, value 0xff is not a valid binary representation for type text |
| blobasascii_invalid | `SELECT blobAsAscii(0xc3a9) FROM e.t WHERE pk = 1` | `0x2200` In call to function blobasascii, value 0xc3a9 is not a valid binary representation for type ascii | `0x2200` In call to function system.blobasascii, value 0xc3a9 is not a valid binary representation for type ascii |
| fromjson_select_bad | `SELECT fromJson('x') FROM e.t WHERE pk = 1` | `0x2200` fromJson() can only be called if receiver type is known | `0x2200` fromjson() cannot be used in the selection clause of a SELECT statement |
| tojson_two_args | `SELECT toJson(v, v) FROM e.t WHERE pk = 1` | `0x2200` toJson() accepts 1 argument only | `0x2200` Invalid number of arguments for function system.tojson(any) |
| mintimeuuid_text | `SELECT minTimeuuid('nope') FROM e.t WHERE pk = 1` | `0x2200` marshaling error: unable to parse date 'nope': marshaling error: Unable to parse timestamp from 'nope' | `0x2200` Unable to parse a date/time from 'nope' |
| totimestamp_text | `SELECT toTimestamp('x') FROM e.t WHERE pk = 1` | `0x2200` marshaling error: Unable to make unsigned int (for date) from x | `0x2200` Unable to coerce 'x' to a formatted date (long) |
| token_select_wrong | `SELECT token(v) FROM e.t WHERE pk = 1` | OK Rows | (same) |
| ttl_two_args | `SELECT ttl(v, v) FROM e.t WHERE pk = 1` | `0x2000` line 1:12 : Missing ')' | `0x2000` line 1:12 mismatched input ',' expecting ')' (SELECT ttl(v[,]...) |
| now_with_arg | `SELECT now(1) FROM e.t WHERE pk = 1` | `0x2200` Invalid number of arguments in call to function system.now: 0 required but 1 provided | (same) |
| uuid_in_insert_ok | `INSERT INTO e.u (id, u) VALUES (2, uuid())` | OK Void | (same) |
| now_into_uuid_ok | `INSERT INTO e.u (id, u) VALUES (3, now())` | OK Void | (same) |
| now_into_text | `INSERT INTO e.u (id, tx) VALUES (4, now())` | `0x2200` Type error: cannot assign result of function system.now (type timeuuid) to tx (type text) | (same) |
| currenttimestamp_into_date | `INSERT INTO e.u (id, dt) VALUES (5, currentTimestamp())` | `0x2200` Type error: cannot assign result of function system.currenttimestamp (type timestamp) to dt (type date) | (same) |
| abs_text | `SELECT abs(txt) FROM e.t WHERE pk = 1` | `0x2200` Unknown function abs called | `0x2200` Invalid call to function abs, none of its type signatures match (known type signatures: system.abs : (smallint) -> smallint, system.abs : (int) -> int, system.abs : (float) -> float, system.abs : (varint) -> varint, system.abs : (tinyint) -> tinyint, system.abs : (decimal) -> decimal, system.abs : (double) -> double, system.abs : (bigint) -> bigint, system.abs : (counter) -> counter) |
| abs_ok | `SELECT abs(v) FROM e.t WHERE pk = 1` | `0x2200` Unknown function abs called | OK Rows |
| map_keys | `SELECT map_keys(m) FROM e.t WHERE pk = 1` | `0x2200` Unknown function map_keys called | OK Rows |
| collection_count | `SELECT collection_count(l) FROM e.t WHERE pk = 1` | `0x2200` Unknown function collection_count called | OK Rows |
| mask_default | `SELECT mask_default(v) FROM e.t WHERE pk = 1` | `0x2200` Unknown function mask_default called | OK Rows |
| similarity_cosine_zero | `SELECT similarity_cosine([0.0, 0.0, 0.0], [1.0, 2.0, 3.0]) FROM e.t WHERE pk = 1` | OK Rows | `0x2200` Function system.similarity_cosine doesn't support all-zero vectors. |
| similarity_dim_mismatch | `SELECT similarity_cosine(emb, [1.0, 2.0]) FROM e.vec` | `0x2200` All arguments must have the same vector dimensions, but found vector<float, 3> and vector<float, 2> | `0x2200` All arguments must have the same vector dimensions |
| avg_overflow_int | `SELECT avg(v) FROM e.t WHERE pk = 1` | OK Rows | (same) |
| sum_overflow_setup | `INSERT INTO e.t (pk, ck, v) VALUES (20, 1, 2147483647)` | OK Void | (same) |
| sum_overflow_setup2 | `INSERT INTO e.t (pk, ck, v) VALUES (20, 2, 2147483647)` | OK Void | (same) |
| sum_overflow | `SELECT sum(v) FROM e.t WHERE pk = 20` | `0x2200` Sum overflow. Values should be casted to a wider type. | OK Rows |
| cast_overflow | `SELECT CAST(v AS tinyint) FROM e.t WHERE pk = 20` | OK Rows | (same) |
| arith_overflow | `SELECT v + 1 FROM e.t WHERE pk = 20` | `0x2000` line 1:7 no viable alternative at input 'v' | OK Rows |

## Rate limits and USING TIMEOUT

| id | statement | ScyllaDB 2026.3.3 | Cassandra 5.0.9 |
|---|---|---|---|
| rate_limit_writes | `40 x INSERT INTO e.rl (pk, v) VALUES (1, i) with per_partition_rate_limit max_writes_per_second 1` | `0x2300` Per-partition rate limit reached for write in table e.rl, rejected by coordinator | `0x2200` table rl does not exist |
| rate_limit_reads | `40 x SELECT * FROM e.rl WHERE pk = 1` | `0x2300` Per-partition rate limit reached for read in table e.rl, rejected by coordinator | `0x2200` table rl does not exist |
| timeout_select_1us | `SELECT * FROM e.tomb WHERE pk = 2 USING TIMEOUT 1us` | `0x2200` Timeout values cannot have granularity finer than milliseconds | `0x2000` line 1:34 no viable alternative at input 'USING' (...tomb WHERE pk = 2 [USING]...) |
| timeout_insert_1us | `INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) USING TIMEOUT 1us` | `0x2200` Timeout values cannot have granularity finer than milliseconds | `0x2000` line 1:51 no viable alternative at input 'TIMEOUT' (..., 1, 1) USING [TIMEOUT]...) |
| timeout_negative | `SELECT * FROM e.t WHERE pk = 1 USING TIMEOUT -1s` | `0x2200` Timeout values must be non-negative | `0x2000` line 1:31 no viable alternative at input 'USING' (...t WHERE pk = 1 [USING]...) |
| timeout_zero | `SELECT * FROM e.t WHERE pk = 1 USING TIMEOUT 0s` | OK Rows | `0x2000` line 1:31 no viable alternative at input 'USING' (...t WHERE pk = 1 [USING]...) |
| timeout_and_ttl | `INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) USING TIMEOUT 10s AND TTL 10` | OK Void | `0x2000` line 1:51 no viable alternative at input 'TIMEOUT' (..., 1, 1) USING [TIMEOUT]...) |

## Prepared statements, bound nulls and UNSET

| id | statement | ScyllaDB 2026.3.3 | Cassandra 5.0.9 |
|---|---|---|---|
| null_pk_bound | `INSERT INTO e.t (pk, ck, v) VALUES (?, ?, ?) with [None, 1, 1]` | `0x2200` Invalid null value in condition for column pk | (same) |
| null_ck_bound | `INSERT INTO e.t (pk, ck, v) VALUES (?, ?, ?) with [1, None, 1]` | `0x2200` Invalid null value in condition for column ck | (same) |
| null_pk_bound_select | `SELECT * FROM e.t WHERE pk = ? with [None]` | OK | `0x2200` Invalid null value in condition for column pk |
| null_in_list_bound | `SELECT * FROM e.t WHERE pk IN ? with [[1, None]]` | OK | `0x2200` Invalid null value in condition for column pk |
| null_limit_bound | `SELECT * FROM e.t LIMIT ? with [None]` | `0x2200` Invalid null value of limit | (same) |
| null_ttl_bound | `INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) USING TTL ? with [None]` | `0x2200` Invalid null value of TTL | OK |
| null_timestamp_bound | `INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) USING TIMESTAMP ? with [None]` | `0x2200` Invalid null value of timestamp | (same) |
| null_counter_increment | `UPDATE e.c SET cnt = cnt + ? WHERE pk = 1 with [None]` | `0x2200` Invalid null value for counter increment | (same) |
| null_list_index | `UPDATE e.t SET l[?] = 1 WHERE pk = 1 AND ck = 1 with [None]` | `0x2200` Invalid null value for list index | (same) |
| null_map_key_bound | `UPDATE e.t SET m[?] = 1 WHERE pk = 1 AND ck = 1 with [None]` | `0x2200` Invalid null map key | (same) |
| empty_blob_pk_bound | `INSERT INTO e.blobk (pk, v) VALUES (?, 1) with [b'']` | `0x2200` Key may not be empty | (same) |
| negative_limit_bound | `SELECT * FROM e.t LIMIT ? with [-1]` | `0x2200` LIMIT must be strictly positive | (same) |
| null_in_set_bound | `INSERT INTO e.t (pk, ck, st) VALUES (1, 1, ?) with [{None, 1}]` | `0x2200` null is not supported inside collections | `0x2200` Null value read when not allowed |
| unset_pk | `INSERT INTO e.t (pk, ck, v) VALUES (?, ?, ?) with ['UNSET', 1, 1]` | `None` Cannot bind UNSET_VALUE as a part of the routing key 'pk' | (same) |
| unset_regular_ok | `INSERT INTO e.t (pk, ck, v) VALUES (?, ?, ?) with [1, 1, 'UNSET']` | OK | (same) |
| unset_where_select | `SELECT * FROM e.t WHERE pk = ? with ['UNSET']` | `None` Cannot bind UNSET_VALUE as a part of the routing key 'pk' | (same) |
| unset_limit | `SELECT * FROM e.t LIMIT ? with ['UNSET']` | OK | (same) |
| unset_ttl | `INSERT INTO e.t (pk, ck, v) VALUES (1, 1, 1) USING TTL ? with ['UNSET']` | OK | (same) |
| unset_in_list | `SELECT * FROM e.t WHERE pk IN ? with ['UNSET']` | `0x2200` Unexpected unset value for bind variable 0 | `0x2200` Invalid unset value for column pk |
| unset_lwt_condition | `UPDATE e.t SET v = 1 WHERE pk = 1 AND ck = 1 IF v = ? with ['UNSET']` | `0x2200` Unexpected unset value for bind variable 0 | `0x1000` Cannot achieve consistency level QUORUM |
| prepare_duplicate_named_marker | `PREPARE SELECT * FROM e.t WHERE pk = :x AND ck = :x` | OK | (same) |
| prepare_marker_in_select | `PREPARE SELECT ? FROM e.t` | `0x2200` Could not infer type of ? | `0x2200` Cannot infer type for term ? in selection clause (try using a cast to force a type) |
| prepare_marker_in_table | `PREPARE SELECT * FROM ?` | `0x2000` Bind variables cannot be used for table names | (same) |
| prepare_marker_as_ttl_fn | `PREPARE SELECT ttl(?) FROM e.t` | `0x2000` line 1:11 no viable alternative at input '?' | `0x2000` line 1:11 no viable alternative at input '?' (SELECT ttl([?]...) |
| prepare_use | `PREPARE USE e` | OK | (same) |
| prepare_ddl | `PREPARE CREATE TABLE e.px (pk int PRIMARY KEY)` | OK | (same) |
| prepare_marker_untyped_fn | `PREPARE SELECT * FROM e.t WHERE pk = abs(?)` | `0x2200` Unknown function abs called | OK |
| prepare_marker_in_collection | `PREPARE INSERT INTO e.t (pk, ck, l) VALUES (1, 1, [?, ?])` | OK | `0x2200` Invalid list literal for l: bind variables are not supported inside collection literals |
| prepare_marker_in_map_key | `PREPARE INSERT INTO e.t (pk, ck, m) VALUES (1, 1, {?: 1})` | OK | `0x2200` Invalid map literal for m: bind variables are not supported inside collection literals |
| prepare_marker_vector | `PREPARE SELECT * FROM e.vec ORDER BY emb ANN OF ? LIMIT 1` | OK | (same) |
| prepare_too_many_markers | `PREPARE SELECT * FROM e.t WHERE pk IN (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?… (140039 chars)` | `0x2200` Too many markers(?). 70000 markers exceed the allowed maximum of 65535 | (same) |

## Auth statements with the default AllowAllAuthenticator

| id | statement | ScyllaDB 2026.3.3 | Cassandra 5.0.9 |
|---|---|---|---|
| grant_allowall | `GRANT SELECT ON e.t TO nope` | `0x2100` You have to be logged in and not anonymous to perform this request | (same) |
| revoke_allowall | `REVOKE SELECT ON e.t FROM nope` | `0x2100` You have to be logged in and not anonymous to perform this request | (same) |
| create_role_allowall | `CREATE ROLE r1 WITH PASSWORD = 'x' AND LOGIN = true` | `0x2100` You have to be logged in and not anonymous to perform this request | `0x2200` org.apache.cassandra.auth.CassandraRoleManager doesn't support PASSWORD |
| create_role_nopw_allowall | `CREATE ROLE r2` | `0x2100` You have to be logged in and not anonymous to perform this request | (same) |
| create_user_allowall | `CREATE USER u1 WITH PASSWORD 'x'` | `0x2100` You have to be logged in and not anonymous to perform this request | `0x2200` org.apache.cassandra.auth.CassandraRoleManager doesn't support PASSWORD |
| alter_role_allowall | `ALTER ROLE cassandra WITH PASSWORD = 'x'` | `0x2100` You have to be logged in and not anonymous to perform this request | `0x2200` org.apache.cassandra.auth.CassandraRoleManager doesn't support PASSWORD |
| drop_role_allowall | `DROP ROLE nope` | `0x2100` You have to be logged in and not anonymous to perform this request | (same) |
| list_roles_allowall | `LIST ROLES` | `0x2100` You have to be logged in and not anonymous to perform this request | (same) |
| list_permissions_allowall | `LIST ALL PERMISSIONS` | `0x2100` You are not authorized to view everyone's permissions | `0x2100` You have to be logged in and not anonymous to perform this request |
| list_users_allowall | `LIST USERS` | `0x2100` You have to be logged in and not anonymous to perform this request | (same) |
| grant_role_allowall | `GRANT r1 TO r2` | `0x2100` You have to be logged in and not anonymous to perform this request | (same) |
| service_level_create | `CREATE SERVICE LEVEL sl1 WITH timeout = 10s` | OK Void | `0x2000` line 1:7 no viable alternative at input 'SERVICE' ([CREATE] SERVICE...) |
| service_level_list | `LIST ALL SERVICE LEVELS` | OK Rows | `0x2000` line 1:9 mismatched input 'SERVICE' expecting EOF (LIST ALL [SERVICE]...) |
| attach_service_level | `ATTACH SERVICE LEVEL sl1 TO cassandra` | `0x2200` Role cassandra doesn't exist. | `0x2000` line 1:0 no viable alternative at input 'ATTACH' ([ATTACH]...) |
| describe_passwords_anon | `DESCRIBE SCHEMA WITH INTERNALS AND PASSWORDS` | `0x2100` DESCRIBE SCHEMA WITH INTERNALS AND PASSWORDS can only be issued by a superuser | `0x2000` line 1:31 mismatched input 'AND' expecting EOF (DESCRIBE SCHEMA WITH INTERNALS [AND]...) |

## Auth with PasswordAuthenticator

Captured on references started from the default image with only `PasswordAuthenticator` + `CassandraAuthorizer` added (superuser `cassandra`/`cassandra`). `refs_alice` is a non-superuser login role, `refs_admin` a second superuser, `refs_nologin` has `LOGIN = false`; `refs_ea.t` is a table. Login cases use raw SASL PLAIN; statements are sent on raw connections logged in as the role named.

| id | statement | ScyllaDB 2026.3.3 | Cassandra 5.0.9 |
|---|---|---|---|
| login_wrong_password | `SASL PLAIN refs_alice / wrong` | `0x0100` Username and/or password are incorrect | `0x0100` Provided username refs_alice and/or password are incorrect |
| login_unknown_user | `SASL PLAIN nobody / x` | `0x0100` Username and/or password are incorrect | `0x0100` Provided username nobody and/or password are incorrect |
| login_not_permitted | `SASL PLAIN refs_nologin / x (LOGIN = false)` | `0x0100` refs_nologin is not permitted to log in | (same) |
| login_empty_user | `SASL PLAIN empty user and password` | `0x0100` Authentication ID must not be null | `0x0100` Password must not be null |
| login_ok | `SASL PLAIN refs_alice / refs_alice` | OK | (same) |
| startup_authenticate | `STARTUP → AUTHENTICATE body` | OK | (same) |
| query_before_auth | `QUERY SELECT key FROM system.local before AUTH_RESPONSE` | `0x000A` Unexpected message 7, expecting SASL_RESPONSE | `0x000A` Unexpected message QUERY, expecting SASL_RESPONSE |
| auth_response_garbage | `AUTH_RESPONSE with token "abc"` | `0x0100` Authentication ID must not be null | `0x0100` Password must not be null |
| select_no_permission | `as refs_alice: SELECT * FROM refs_ea.t` | `0x2100` User refs_alice has no SELECT permission on <table refs_ea.t> or any of its parents | (same) |
| insert_no_permission | `as refs_alice: INSERT INTO refs_ea.t (pk, v) VALUES (1, 1)` | `0x2100` User refs_alice has no MODIFY permission on <table refs_ea.t> or any of its parents | (same) |
| create_ks_no_permission | `as refs_alice: CREATE KEYSPACE refs_ea2 WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}` | `0x2100` User refs_alice has no CREATE permission on <all keyspaces> or any of its parents | (same) |
| create_table_no_permission | `as refs_alice: CREATE TABLE refs_ea.t2 (pk int PRIMARY KEY)` | `0x2100` User refs_alice has no CREATE permission on <keyspace refs_ea> or any of its parents | `0x2100` User refs_alice has no CREATE permission on <all tables in refs_ea> or any of its parents |
| drop_table_no_permission | `as refs_alice: DROP TABLE refs_ea.t` | `0x2100` User refs_alice has no DROP permission on <table refs_ea.t> or any of its parents | (same) |
| alter_table_no_permission | `as refs_alice: ALTER TABLE refs_ea.t ADD x int` | `0x2100` User refs_alice has no ALTER permission on <table refs_ea.t> or any of its parents | (same) |
| truncate_no_permission | `as refs_alice: TRUNCATE refs_ea.t` | `0x2100` User refs_alice has no MODIFY permission on <table refs_ea.t> or any of its parents | (same) |
| create_index_no_permission | `as refs_alice: CREATE INDEX ON refs_ea.t (v)` | `0x2100` User refs_alice has no ALTER permission on <table refs_ea.t> or any of its parents | (same) |
| grant_no_permission | `as refs_alice: GRANT SELECT ON ALL KEYSPACES TO refs_alice` | `0x2100` User refs_alice has no AUTHORIZE permission on <all keyspaces> or any of its parents | (same) |
| create_role_no_permission | `as refs_alice: CREATE ROLE refs_bob` | `0x2100` User refs_alice has no CREATE permission on <all roles> or any of its parents | `0x2100` User refs_alice does not have sufficient privileges to perform the requested operation |
| drop_role_no_permission | `as refs_alice: DROP ROLE refs_admin` | `0x2100` User refs_alice has no DROP permission on <role refs_admin> or any of its parents | `0x2100` User refs_alice does not have sufficient privileges to perform the requested operation |
| alter_other_role | `as refs_alice: ALTER ROLE refs_admin WITH PASSWORD = 'x'` | `0x2100` User refs_alice has no ALTER permission on <role refs_admin> or any of its parents | `0x2100` User refs_alice does not have sufficient privileges to perform the requested operation |
| alter_own_superuser | `as refs_alice: ALTER ROLE refs_alice WITH SUPERUSER = true` | `0x2100` Only superusers are allowed to alter superuser status. | `0x2100` You aren't allowed to alter your own superuser status or that of a role granted to you |
| alter_own_password | `as refs_alice: ALTER ROLE refs_alice WITH PASSWORD = 'refs_alice'` | OK Void | `0x1001` Password for role refs_alice can only be changed every 5000ms. |
| list_roles_nonsuper | `as refs_alice: LIST ROLES` | OK Rows | (same) |
| list_all_permissions_nonsuper | `as refs_alice: LIST ALL PERMISSIONS` | `0x2100` You are not authorized to view everyone's permissions | (same) |
| list_own_permissions | `as refs_alice: LIST ALL PERMISSIONS OF refs_alice` | OK Rows | OK Void |
| list_other_permissions | `as refs_alice: LIST ALL PERMISSIONS OF refs_admin` | `0x2100` You are not authorized to view refs_admin's permissions | (same) |
| read_system_auth_roles | `as refs_alice: SELECT * FROM system_auth.roles` | `0x2200` unconfigured table roles | `0x2100` User refs_alice has no SELECT permission on <table system_auth.roles> or any of its parents |
| read_system_roles | `as refs_alice: SELECT * FROM system.roles` | `0x2100` User refs_alice has no SELECT permission on <table system.roles> or any of its parents | `0x2200` table roles does not exist |
| describe_passwords_nonsuper | `as refs_alice: DESCRIBE SCHEMA WITH INTERNALS AND PASSWORDS` | `0x2100` DESCRIBE SCHEMA WITH INTERNALS AND PASSWORDS can only be issued by a superuser | `0x2000` line 1:31 mismatched input 'AND' expecting EOF (DESCRIBE SCHEMA WITH INTERNALS [AND]...) |
| describe_schema_nonsuper | `as refs_alice: DESCRIBE SCHEMA` | OK Rows | (same) |
| select_system_local_nonsuper | `as refs_alice: SELECT cluster_name FROM system.local` | OK Rows | (same) |
| select_system_schema_nonsuper | `as refs_alice: SELECT keyspace_name FROM system_schema.keyspaces` | OK Rows | (same) |
| use_keyspace_nonsuper | `as refs_alice: USE refs_ea` | OK SetKeyspace | (same) |
| create_function_nonsuper | `as refs_alice: CREATE FUNCTION refs_ea.f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x'` | `0x2100` User refs_alice has no CREATE permission on <all functions in refs_ea> or any of its parents | (same) |
| service_level_nonsuper | `as refs_alice: CREATE SERVICE LEVEL refs_sl_alice` | `0x2100` User refs_alice has no CREATE permission on <all service levels> or any of its parents | `0x2000` line 1:7 no viable alternative at input 'SERVICE' ([CREATE] SERVICE...) |
| list_service_levels_nonsuper | `as refs_alice: LIST ALL SERVICE LEVELS` | `0x2100` User refs_alice has no DESCRIBE permission on <all service levels> or any of its parents | `0x2000` line 1:9 mismatched input 'SERVICE' expecting EOF (LIST ALL [SERVICE]...) |
| grant_select | `GRANT SELECT ON refs_ea.t TO refs_alice` | OK Void | (same) |
| grant_modify_keyspace | `GRANT MODIFY ON KEYSPACE refs_ea TO refs_alice` | OK Void | (same) |
| select_after_grant | `as refs_alice: SELECT * FROM refs_ea.t` | OK Rows | `0x2100` User refs_alice has no SELECT permission on <table refs_ea.t> or any of its parents |
| insert_after_grant | `as refs_alice: INSERT INTO refs_ea.t (pk, v) VALUES (1, 1)` | OK Void | `0x2100` User refs_alice has no MODIFY permission on <table refs_ea.t> or any of its parents |
| alter_after_grant_select_only | `as refs_alice: ALTER TABLE refs_ea.t ADD y int` | `0x2100` User refs_alice has no ALTER permission on <table refs_ea.t> or any of its parents | (same) |
| create_role_exists | `CREATE ROLE refs_alice` | `0x2200` Role refs_alice already exists. | `0x2200` refs_alice already exists |
| create_role_if_not_exists | `CREATE ROLE IF NOT EXISTS refs_alice` | OK Void | (same) |
| drop_role_unknown | `DROP ROLE nope` | `0x2200` Role nope doesn't exist. | `0x2200` nope doesn't exist |
| drop_role_if_exists_unknown | `DROP ROLE IF EXISTS nope` | OK Void | (same) |
| grant_unknown_table | `GRANT SELECT ON refs_ea.nope TO refs_alice` | `0x2200` <table refs_ea.nope> doesn't exist. | `0x2200` Resource <table refs_ea.nope> doesn't exist |
| grant_unknown_keyspace | `GRANT SELECT ON KEYSPACE nope TO refs_alice` | `0x2200` <keyspace nope> doesn't exist. | `0x2200` Resource <keyspace nope> doesn't exist |
| grant_unknown_role | `GRANT SELECT ON KEYSPACE refs_ea TO nope` | `0x2200` Role nope doesn't exist. | `0x2200` Role nope doesn't exist |
| grant_bad_permission | `GRANT NOPE ON KEYSPACE refs_ea TO refs_alice` | `0x2000` line 1:11 : Syntax error | `0x2000` line 1:11 mismatched input 'ON' expecting K_TO (GRANT NOPE [ON]...) |
| grant_describe_on_keyspace | `GRANT DESCRIBE ON KEYSPACE refs_ea TO refs_alice` | `0x2000` Resource <keyspace refs_ea> does not support any of the requested permissions. | `0x2000` Resource type DataResource does not support any of the requested permissions |
| grant_execute_on_table | `GRANT EXECUTE ON refs_ea.t TO refs_alice` | `0x2000` Resource <table refs_ea.t> does not support any of the requested permissions. | `0x2000` Resource type DataResource does not support any of the requested permissions |
| grant_authorize_on_role | `GRANT AUTHORIZE ON ROLE refs_alice TO refs_alice` | OK Void | (same) |
| grant_select_on_role | `GRANT SELECT ON ROLE refs_alice TO refs_alice` | `0x2000` Resource <role refs_alice> does not support any of the requested permissions. | `0x2000` Resource type RoleResource does not support any of the requested permissions |
| grant_all_functions | `GRANT EXECUTE ON ALL FUNCTIONS IN KEYSPACE refs_ea TO refs_alice` | OK Void | (same) |
| grant_unknown_function | `GRANT EXECUTE ON FUNCTION refs_ea.nope(int) TO refs_alice` | `0x2200` <function refs_ea.nope(int)> doesn't exist. | `0x0000` java.lang.AssertionError: Unable to find function object for resource <function refs_ea.nope(int)> |
| grant_mbean | `GRANT SELECT ON MBEAN 'org.apache.cassandra.db:type=Tables' TO refs_alice` | `0x2000` line 1:22 : Unexpected 'org.apache.cassandra.db:type=Tables' | `0x2200` Resource <mbean org.apache.cassandra.db:type=Tables> doesn't exist |
| grant_unmask | `GRANT UNMASK ON refs_ea.t TO refs_alice` | `0x2000` line 1:13 : Syntax error | OK Void |
| grant_select_masked | `GRANT SELECT_MASKED ON refs_ea.t TO refs_alice` | `0x2000` line 1:20 : Syntax error | OK Void |
| grant_vector_search | `GRANT VECTOR_SEARCH_INDEXING ON ALL KEYSPACES TO refs_alice` | OK Void | `0x2000` line 1:29 mismatched input 'ON' expecting K_TO (GRANT VECTOR_SEARCH_INDEXING [ON]...) |
| list_permissions_unknown_role | `LIST ALL PERMISSIONS OF nope` | `0x2200` Role nope doesn't exist. | `0x2200` <role nope> doesn't exist |
| alter_unknown_role | `ALTER ROLE nope WITH PASSWORD = 'x'` | `0x2200` Role nope doesn't exist. | `0x2200` Role nope doesn't exist |
| revoke_unknown_keyspace | `REVOKE SELECT ON KEYSPACE nope FROM refs_alice` | `0x2200` <keyspace nope> doesn't exist. | `0x2200` Resource <keyspace nope> doesn't exist |
| revoke_not_granted | `REVOKE ALTER ON KEYSPACE refs_ea FROM refs_alice` | OK Void | OK Void; warnings: "Role 'refs_alice' was not granted ALTER on <keyspace refs_ea>" |
| create_user_legacy | `CREATE USER refs_legacy WITH PASSWORD 'x' NOSUPERUSER` | OK Void | (same) |
| create_user_exists | `CREATE USER refs_alice WITH PASSWORD 'x'` | `0x2200` Role refs_alice already exists. | `0x2200` refs_alice already exists |
| alter_user_legacy | `ALTER USER refs_legacy WITH PASSWORD 'y'` | OK Void | `0x1001` Password for role refs_legacy can only be changed every 5000ms. |
| list_users_super | `LIST USERS` | OK Rows | (same) |
| role_bad_option | `CREATE ROLE refs_r9 WITH NOPE = 1` | `0x2000` line 1:25 no viable alternative at input 'NOPE' | `0x2000` line 1:25 no viable alternative at input 'NOPE' (CREATE ROLE refs_r9 WITH [NOPE]...) |
| role_options_map | `CREATE ROLE refs_r11 WITH OPTIONS = {'a': 'b'}` | `0x2200` The OPTIONS option is not supported. | `0x2200` org.apache.cassandra.auth.CassandraRoleManager doesn't support OPTIONS |
| role_hashed_password | `CREATE ROLE refs_r10 WITH HASHED PASSWORD = 'notahash' AND LOGIN = true` | OK Void | `0x2200` Invalid hashed password value. Please use jBcrypt. |
| role_password_and_hashed | `CREATE ROLE refs_r12 WITH PASSWORD = 'x' AND HASHED PASSWORD = 'y'` | `0x2000` Only one of the options: PASSWORD, HASHED PASSWORD can be provided | `0x2000` Options 'password' and 'hashed password' are mutually exclusive |
| role_generated_password | `CREATE ROLE refs_r13 WITH GENERATED PASSWORD` | `0x2000` line 1:26 no viable alternative at input 'GENERATED' | `0x2000` line 1:26 no viable alternative at input 'GENERATED' (CREATE ROLE refs_r13 WITH [GENERATED]...) |
| role_access_to_datacenters | `CREATE ROLE refs_r14 WITH ACCESS TO DATACENTERS {'datacenter1'}` | `0x2000` line 1:26 no viable alternative at input 'ACCESS' | OK Void |
| role_access_to_cidrs | `CREATE ROLE refs_r15 WITH ACCESS FROM CIDRS {'region1'}` | `0x2000` line 1:26 no viable alternative at input 'ACCESS' | `0x2200` Invalid CIDR group(s): [region1]. Available CIDR Groups are: [] |
| grant_role_to_self | `GRANT refs_alice TO refs_alice` | `0x2200` refs_alice already includes role refs_alice. | `0x2200` refs_alice is a member of refs_alice |
| grant_role_cycle_setup | `CREATE ROLE refs_ra` | OK Void | (same) |
| grant_role_cycle_1 | `GRANT refs_ra TO refs_alice` | OK Void | (same) |
| grant_role_cycle_2 | `GRANT refs_alice TO refs_ra` | `0x2200` refs_alice already includes role refs_ra. | `0x2200` refs_alice is a member of refs_ra |
| grant_role_twice | `GRANT refs_ra TO refs_alice` | `0x2200` refs_alice already includes role refs_ra. | `0x2200` refs_alice is a member of refs_ra |
| revoke_role_not_granted | `REVOKE refs_ra FROM refs_nologin` | `0x2200` refs_nologin was not granted role refs_ra, so it cannot be revoked. | `0x2200` refs_nologin is not a member of refs_ra |
| list_roles_super | `LIST ROLES` | OK Rows | (same) |
| list_roles_of | `LIST ROLES OF refs_alice` | OK Rows | (same) |
| list_perms_alice | `LIST ALL PERMISSIONS OF refs_alice` | OK Rows | (same) |
| list_perms_on_table | `LIST ALL PERMISSIONS ON refs_ea.t` | OK Rows | (same) |
| list_perms_norecursive | `LIST ALL PERMISSIONS OF refs_alice NORECURSIVE` | OK Rows | (same) |
| describe_passwords_super | `DESCRIBE SCHEMA WITH INTERNALS AND PASSWORDS` | OK Rows | `0x2000` line 1:31 mismatched input 'AND' expecting EOF (DESCRIBE SCHEMA WITH INTERNALS [AND]...) |
| create_service_level | `CREATE SERVICE LEVEL refs_sl1 WITH timeout = 10s AND workload_type = 'interactive'` | OK Void | `0x2000` line 1:7 no viable alternative at input 'SERVICE' ([CREATE] SERVICE...) |
| create_service_level_bad_option | `CREATE SERVICE LEVEL refs_sl2 WITH nope = 1` | `0x2000` Unknown property 'nope' | `0x2000` line 1:7 no viable alternative at input 'SERVICE' ([CREATE] SERVICE...) |
| attach_service_level | `ATTACH SERVICE LEVEL refs_sl1 TO refs_alice` | OK Void | `0x2000` line 1:0 no viable alternative at input 'ATTACH' ([ATTACH]...) |
| list_service_levels | `LIST ALL SERVICE LEVELS` | OK Rows | `0x2000` line 1:9 mismatched input 'SERVICE' expecting EOF (LIST ALL [SERVICE]...) |
| list_attached | `LIST ALL ATTACHED SERVICE LEVELS` | OK Rows | `0x2000` line 1:9 mismatched input 'ATTACHED' expecting EOF (LIST ALL [ATTACHED]...) |
| list_effective | `LIST EFFECTIVE SERVICE LEVEL OF refs_alice` | OK Rows | `0x2000` line 1:5 no viable alternative at input 'EFFECTIVE' ([LIST] EFFECTIVE...) |
| detach_service_level | `DETACH SERVICE LEVEL FROM refs_alice` | OK Void | `0x2000` line 1:0 no viable alternative at input 'DETACH' ([DETACH]...) |
| drop_service_level | `DROP SERVICE LEVEL refs_sl1` | OK Void | `0x2000` line 1:5 no viable alternative at input 'SERVICE' ([DROP] SERVICE...) |
| drop_service_level_unknown | `DROP SERVICE LEVEL nope` | `0x2200` Service Level nope doesn't exists. | `0x2000` line 1:5 no viable alternative at input 'SERVICE' ([DROP] SERVICE...) |
| drop_role_alice | `DROP ROLE refs_alice` | OK Void | (same) |
| drop_own_role | `as refs_admin (superuser): DROP ROLE refs_admin` | `0x2200` Cannot DROP primary role for current login. | `0x2200` Cannot DROP primary role for current login |
| query_after_role_dropped | `as refs_alice (role dropped while connected): SELECT * FROM refs_ea.t` | `0x2100` User refs_alice has no SELECT permission on <table refs_ea.t> or any of its parents | (same) |

## Writes to system keyspaces (observed once; not re-probed)

These statements were accepted by both references in an early capture run and damaged the node (drivers could no longer connect; ScyllaDB's Raft group 0 stopped). They are recorded here as observed and are no longer probed. Do not run them against shared references.

| id | statement | ScyllaDB 2026.3.3 | Cassandra 5.0.9 |
|---|---|---|---|
| ks_alter_system | `ALTER KEYSPACE system WITH durable_writes = false` | `0x2100` system keyspace is not user-modifiable. | (same) |
| ks_alter_system_auth | `ALTER KEYSPACE system_auth WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}` | OK SchemaChange `["UPDATED", "KEYSPACE", "system_auth"]`; warnings: "Using Replication Factor datacenter1=1 lower than the minimum_replication_factor_warn_threshold=3 is not recommended." | OK SchemaChange `["UPDATED", "KEYSPACE", "system_auth"]` |
| drop_system_keyspace | `DROP KEYSPACE system` | `0x2100` system keyspace is not user-modifiable. | (same) |
| drop_system_table | `DROP TABLE system.local` | `0x2100` system keyspace is not user-modifiable. | (same) |
| drop_system_schema_keyspace | `DROP KEYSPACE system_schema` | `0x2100` system_schema keyspace is not user-modifiable. | (same) |
| truncate_system_table | `TRUNCATE system.local` | OK Void | (same) |
| create_table_in_system | `CREATE TABLE system.x (pk int PRIMARY KEY)` | `0x2100` system keyspace is not user-modifiable. | (same) |
| insert_into_system_local | `INSERT INTO system.local (key) VALUES ('x')` | OK Void | (same) |
| delete_from_system_schema | `DELETE FROM system_schema.keyspaces WHERE keyspace_name = 'e'` | OK Void | (same) |
| create_function_system | `CREATE FUNCTION system.f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE java AS 'return x;'` | `0x2100` system keyspace is not user-modifiable. | `0x2200` System keyspace 'system' is not user-modifiable |
| mv_other_keyspace | `CREATE MATERIALIZED VIEW system.mv8 AS SELECT * FROM e.ix WHERE pk IS NOT NULL AND ck IS NOT NULL PRIMARY KEY (ck, pk)` | `0x2100` system keyspace is not user-modifiable. | `0x2200` Cannot create a materialized view on a table in a different keyspace |
