-- What drivers read at connect: system.local (volatile columns masked), peers.
!mask host_id,schema_version,broadcast_address,listen_address,rpc_address,tokens,truncated_at,gossip_generation,data_center,rack,cluster_name,startup_time,scylla_cpu_sharding_algorithm,scylla_nr_shards,scylla_msb_ignore,supported_features,release_version,cql_version,native_protocol_version,bootstrapped SELECT key, cluster_name, partitioner FROM system.local;
SELECT * FROM system.peers;
!cassandra SELECT * FROM system.peers_v2;
SELECT keyspace_name, durable_writes FROM system_schema.keyspaces WHERE keyspace_name = 'system';
