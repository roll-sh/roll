# Run and verify rolld

`rolld` runs the control API, PostgreSQL proxy, and background reconciler. It starts
official PostgreSQL containers on demand. The separate `pg-engine` binary is not
wired into this service path.

Start a local daemon with Docker running:

```sh
cargo run -p rolld -- --home .roll
```

In another shell, create a database and print its connection string:

```sh
cargo run -p roll-cli -- db create shop
cargo run -p roll-cli -- db url shop
```

Use the returned URI with your PostgreSQL driver. The endpoint option selects the
compute, and the first connection starts it. Keep the control API private. It
currently has no management authentication. The proxy does not terminate TLS.

Run the actual-daemon lifecycle check:

```sh
ROLL_E2E=1 cargo test -p rolld --test service -- --ignored --nocapture
```

This check needs `postgres:18-alpine` in Docker. It starts an isolated daemon,
creates an endpoint over HTTP, and uses a real PostgreSQL driver through the proxy.
It checks rollback, four concurrent writers, live CPU resizing without replacing
the session, and data after suspend and reconnect. It then kills the daemon,
restarts it with the same state, and checks adoption of the surviving compute,
new writes, and resumed WAL acknowledgement into the local spool.

The check removes its owned containers and keeps logs under
`target/tmp/rolld-service-*.log`. Successful runs remove their database state;
failed runs retain it. Set `ROLL_E2E_KEEP=1` to retain state after success too.
It does not establish recovery from lost compute data, object-store durability,
lower CPU, TLS compatibility, or multi-host failover.
`scripts/test-postgres.sh --force` includes this check before the control-plane
Docker suite. Filtered runs retain the existing control-plane test filter.
The [2026-11-02 validation record](tests/results/2026-11-02-service/README.md)
records a passing actual-daemon check and four failed recovery preflights.

For restores, computes must reach the daemon's authenticated WAL HTTP route.
`--internal-url` selects that route. On Linux, a loopback-only API listener is
normally insufficient for bridged containers. Binding the API to another address
also requires a permitted route from the compute network. A successful host-side
health request does not prove compute-to-daemon reachability.

The recovery tests first fetch a known WAL file from inside a compute. They check
wrong-token rejection and exact bytes with the correct token. A bounded timeout
reports the routing prerequisite before attempting recovery. On ai-server,
requests from the `roll` bridge to both Docker host gateway addresses time out;
recovery acceptance remains open. Do not change the shared server's firewall to
make the tests pass.

Local resizing and read replicas are different from distribution across hosts.
Multi-host placement, ownership fencing, automatic failover, and distributed SQL
still require design and fault verification. See the
[acceptance requirements](../pg-engine/ACCEPTANCE.md).
