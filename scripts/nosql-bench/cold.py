"""Cold start: time from Cluster.connect() to the first row of `query` (one sample per run)."""
import sys, time
from cassandra.cluster import Cluster
from cassandra.policies import WhiteListRoundRobinPolicy

host, port, query = sys.argv[1], int(sys.argv[2]), sys.argv[3]
# Seconds to wait for the connection and the query (short when polling a starting server).
timeout = float(sys.argv[4]) if len(sys.argv) > 4 else 120
t0 = time.perf_counter()
cluster = Cluster([host], port=port, load_balancing_policy=WhiteListRoundRobinPolicy([host]), protocol_version=4, connect_timeout=timeout, control_connection_timeout=timeout)
session = cluster.connect()
t1 = time.perf_counter()
row = session.execute(query, timeout=timeout).one()
t2 = time.perf_counter()
assert row is not None, "no row"
print(f"connect_ms={(t1 - t0) * 1000:.1f} first_row_ms={(t2 - t0) * 1000:.1f}")
cluster.shutdown()
