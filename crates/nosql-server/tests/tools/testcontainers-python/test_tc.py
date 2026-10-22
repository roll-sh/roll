"""Testcontainers for Python's CassandraContainer and ScyllaContainer with ROLL_NOSQL_IMAGE.
Prints `PASS <check>` / `FAIL <check>: <why>` like the driver suites."""
import os
import sys
import traceback

from cassandra.cluster import Cluster
from cassandra.policies import DCAwareRoundRobinPolicy

IMAGE = os.environ["ROLL_NOSQL_IMAGE"]
failed = 0


def check(name, fn):
    global failed
    try:
        fn()
        print(f"PASS {name}", flush=True)
    except Exception as e:  # noqa: BLE001
        failed += 1
        print(f"FAIL {name}: {type(e).__name__}: {e}".replace("\n", " ")[:500], flush=True)
        traceback.print_exc()


def queries(session, release):
    assert session.execute("SELECT release_version FROM system.local").one().release_version == release
    session.execute("CREATE KEYSPACE app WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}")
    session.execute("CREATE TABLE app.t (k int PRIMARY KEY, v text)")
    session.execute("INSERT INTO app.t (k, v) VALUES (1, 'one')")
    assert session.execute("SELECT v FROM app.t WHERE k = 1").one().v == "one"


def cassandra_module():
    from testcontainers.cassandra import CassandraContainer  # waits for "Startup complete"

    with CassandraContainer(IMAGE) as c:
        assert c.get_local_datacenter() == "datacenter1"
        with Cluster(c.get_contact_points(), load_balancing_policy=DCAwareRoundRobinPolicy(c.get_local_datacenter())) as cluster:
            queries(cluster.connect(), "5.0.9")
            local = cluster.metadata.all_hosts()[0]
            assert local.datacenter == "datacenter1", local.datacenter


def scylla_module():
    from testcontainers.scylla import ScyllaContainer  # waits for "Starting listening for CQL clients"

    with ScyllaContainer(IMAGE) as c:
        with c.get_cluster() as cluster:
            queries(cluster.connect(), "3.0.8")


check("testcontainers-python.cassandra", cassandra_module)
check("testcontainers-python.scylla", scylla_module)
sys.exit(1 if failed else 0)
