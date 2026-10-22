// Testcontainers for Node's CassandraContainer (Docker health check running `cqlsh -u -p -e`)
// and ScyllaContainer with ROLL_NOSQL_IMAGE. Prints `PASS <check>` / `FAIL <check>: <why>`.
'use strict';
const { CassandraContainer } = require('@testcontainers/cassandra');
const { ScyllaContainer } = require('@testcontainers/scylladb');
const cassandra = require('cassandra-driver');

const IMAGE = process.env.ROLL_NOSQL_IMAGE;
let failed = 0;

async function check(name, f) {
  try {
    await f();
    console.log(`PASS ${name}`);
  } catch (e) {
    failed++;
    console.log(`FAIL ${name}: ${String(e && e.stack || e).replace(/\n/g, ' ').slice(0, 500)}`);
  }
}

async function queries(contactPoint, dc, release, auth) {
  const client = new cassandra.Client({ contactPoints: [contactPoint], localDataCenter: dc, authProvider: auth });
  await client.connect();
  try {
    const v = (await client.execute('SELECT release_version, data_center FROM system.local')).first();
    if (v.release_version !== release || v.data_center !== dc) throw new Error(`system.local ${v.release_version} ${v.data_center}`);
    await client.execute("CREATE KEYSPACE app WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}");
    await client.execute('CREATE TABLE app.t (k int PRIMARY KEY, v text)');
    await client.execute("INSERT INTO app.t (k, v) VALUES (1, 'one')");
    if ((await client.execute('SELECT v FROM app.t WHERE k = 1')).first().v !== 'one') throw new Error('row');
  } finally {
    await client.shutdown();
  }
}

(async () => {
  await check('testcontainers-node.cassandra', async () => {
    const c = await new CassandraContainer(IMAGE).withDatacenter('dc-node').withRack('rack-node').start();
    try {
      await queries(c.getContactPoint(), c.getDatacenter(), '5.0.9', new cassandra.auth.PlainTextAuthProvider(c.getUsername(), c.getPassword()));
    } finally {
      await c.stop();
    }
  });
  await check('testcontainers-node.scylladb', async () => {
    const c = await new ScyllaContainer(IMAGE).start();
    try {
      await queries(c.getContactPoint(), c.getDatacenter(), '3.0.8');
    } finally {
      await c.stop();
    }
  });
  process.exit(failed ? 1 : 0);
})();
