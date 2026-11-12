// The driver suite for Node's cassandra-driver. Same checks, names, environment and output as
// tests/drivers/python/suite.py: `PASS <name>`, `FAIL <name>: <why>`, `SKIP <name>: <why>`.
'use strict';
const cassandra = require('cassandra-driver');
const readline = require('readline');
const { types } = cassandra;

const env = (k, d) => (process.env[k] === undefined ? d : process.env[k]);
const HOST = env('HOST', '127.0.0.1');
const PORT = env('PORT', '9042');
const DIALECT = env('DIALECT', 'scylla');
const DC = env('DC', 'datacenter1');
const USER = env('USER_NAME', '');
const PASS = env('PASS', '');
const FEATURES = env('FEATURES', '') === '1';
const RESTART = env('RESTART', '') === '1';
const KS = env('KS', 'drv_node');
let failed = 0;

const out = (line) => process.stdout.write(line + '\n');
class Skip extends Error {}
const skip = (why) => { throw new Skip(why); };
const expect = (cond, msg) => { if (!cond) throw new Error(msg); };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function eventually(f, timeout = 15000) {
  const end = Date.now() + timeout;
  for (;;) {
    try {
      if (await f()) return;
    } catch (e) {
      if (Date.now() > end) throw e;
    }
    if (Date.now() > end) throw new Error('timed out');
    await sleep(200);
  }
}
async function check(name, f) {
  try {
    await f();
    out(`PASS ${name}`);
  } catch (e) {
    if (e instanceof Skip) {
      out(`SKIP ${name}: ${e.message}`);
      return;
    }
    failed++;
    out(`FAIL ${name}: ${(e.name + ': ' + e.message).replace(/\n/g, ' ').slice(0, 400)}`);
    if (env('VERBOSE', '')) console.error(e);
  }
}

function client(user, pass, extra = {}) {
  return new cassandra.Client({
    contactPoints: [`${HOST}:${PORT}`],
    localDataCenter: DC,
    authProvider: user ? new cassandra.auth.PlainTextAuthProvider(user, pass) : undefined,
    socketOptions: { readTimeout: 30000, connectTimeout: 10000 },
    policies: { reconnection: new cassandra.policies.reconnection.ConstantReconnectionPolicy(500) },
    refreshSchemaDelay: 200,
    ...extra,
  });
}

function same(a, b) {
  if (a instanceof Map && b instanceof Map) return a.size === b.size && [...a].every(([k, v]) => b.has(k) && same(v, b.get(k)));
  if (a instanceof Set && b instanceof Set) return a.size === b.size && [...a].every((v) => b.has(v));
  if (Buffer.isBuffer(a)) return Buffer.isBuffer(b) && a.equals(b);
  if (a instanceof Date) return b instanceof Date && a.getTime() === b.getTime();
  if (a && typeof a.equals === 'function') return a.equals(b);
  if (a instanceof types.Tuple) return same(a.elements, b.elements);
  if (a instanceof types.Vector) return same([...a.elements], [...b.elements]);
  if (Array.isArray(a)) return Array.isArray(b) && a.length === b.length && a.every((x, i) => same(x, b[i]));
  if (a && typeof a === 'object') return Object.keys(a).every((k) => same(a[k], b[k]));
  return a === b;
}

async function main() {
  let c = client(USER, PASS);
  await check('connect', () => c.connect());

  await check('auth.bad_password', async () => {
    if (!USER) skip('no authenticator');
    const bad = client(USER, 'wrong-password');
    try {
      await bad.connect();
      throw new Error('connected with a wrong password');
    } catch (e) {
      const inner = Object.values(e.innerErrors || {})[0];
      expect(e instanceof cassandra.errors.NoHostAvailableError && inner instanceof cassandra.errors.AuthenticationError, `error ${e} / ${inner}`);
    } finally {
      await bad.shutdown();
    }
  });

  await check('system.local', async () => {
    const r = (await c.execute('SELECT release_version, data_center, rack FROM system.local')).first();
    expect(r.release_version === (DIALECT === 'scylla' ? '3.0.8' : '5.0.9'), `release_version ${r.release_version}`);
    expect(r.data_center === DC, `data_center ${r.data_center}`);
    const h = c.hosts.values()[0];
    expect(h.datacenter === DC && h.rack === r.rack, `host ${h.datacenter}/${h.rack}`);
  });

  const x = (q, p, o) => c.execute(q, p, { prepare: true, ...o });
  await c.execute(`DROP KEYSPACE IF EXISTS ${KS}`);
  await c.execute(`CREATE KEYSPACE ${KS} WITH replication = {'class': 'NetworkTopologyStrategy', '${DC}': 1}`);
  await c.shutdown();
  c = client(USER, PASS, { keyspace: KS });
  await c.connect();
  await c.execute('CREATE TYPE addr (street text, zip int, tags frozen<set<text>>)');
  await c.execute("CREATE TABLE kv (k int, c int, v text, PRIMARY KEY (k, c)) WITH CLUSTERING ORDER BY (c DESC) AND comment = 'drivers'");
  await c.execute('CREATE TABLE counters (k int PRIMARY KEY, n counter)');
  await c.execute('CREATE TABLE lwt (k int PRIMARY KEY, v text, n int)');
  await c.execute('CREATE INDEX kv_v ON kv (v)');

  const addr = { street: 'Main St', zip: 12345, tags: new Set(['home', 'work']) }; // with the Set encoding below
  const T = [
    ['ascii', 'ascii', 'plain ascii'],
    ['bigint', 'bigint', types.Long.fromString('-9223372036854775808')],
    ['blob', 'blob', Buffer.from([0, 1, 0xfe, 0xff])],
    ['boolean', 'boolean', true],
    ['date', 'date', types.LocalDate.fromString('2024-02-29')],
    ['decimal', 'decimal', types.BigDecimal.fromString('-1234567890.0987654321')],
    ['double', 'double', 1.5e300],
    ['duration', 'duration', new types.Duration(1, 2, types.Long.fromNumber(3000000004))],
    ['float', 'float', 0.5],
    ['inet4', 'inet', types.InetAddress.fromString('192.168.1.10')],
    ['inet6', 'inet', types.InetAddress.fromString('2001:db8::1')],
    ['int', 'int', -2147483648],
    ['smallint', 'smallint', -32768],
    ['text', 'text', 'texte unicode é中\u{1F600}'],
    ['time', 'time', types.LocalTime.fromString('23:59:59.999999999')],
    ['timestamp', 'timestamp', new Date('2026-10-20T08:00:01.234Z')],
    ['timeuuid', 'timeuuid', types.TimeUuid.fromString('d2177dd0-eaa2-11de-a572-001b779c76e3')],
    ['tinyint', 'tinyint', -128],
    ['uuid', 'uuid', types.Uuid.fromString('01234567-89ab-cdef-0123-456789abcdef')],
    ['varchar', 'varchar', 'varchar'],
    ['varint', 'varint', types.Integer.fromString('123456789012345678901234567890')],
    ['list', 'list<int>', [3, 1, 2]],
    ['set', 'set<text>', new Set(['a', 'b'])],
    ['map', 'map<text, int>', new Map([['x', 1], ['y', 2]])],
    ['nested', 'frozen<list<frozen<map<int, text>>>>', [new Map([[1, 'one']]), new Map([[2, 'two']])]],
    ['tuple', 'tuple<int, text, double>', new types.Tuple(1, 't', 2.5)],
    ['udt', 'frozen<addr>', addr],
    ['udt_nonfrozen', 'addr', addr],
    ['vector', 'vector<float, 3>', new types.Vector(new Float32Array([0.5, 1.0, -2.0]), 'float')],
  ];
  await c.execute(`CREATE TABLE types (k int PRIMARY KEY${T.map(([l, t]) => `, c_${l} ${t}`).join('')})`);

  await check('metadata.table', async () => {
    await eventually(() => c.metadata.keyspaces[KS] && c.metadata.keyspaces[KS].strategy, 5000).catch(() => {});
    const ks = c.metadata.keyspaces[KS];
    expect(ks && String(ks.strategy).endsWith('NetworkTopologyStrategy'), `strategy ${ks && JSON.stringify(ks).slice(0, 300)}`);
    const t = await c.metadata.getTable(KS, 'kv');
    expect(t.partitionKeys.map((x) => x.name).join() === 'k' && t.clusteringKeys.map((x) => x.name).join() === 'c', 'keys');
    expect(t.clusteringOrder[0] === 'DESC', `clustering order ${t.clusteringOrder}`);
    expect(t.comment === 'drivers', `comment ${t.comment}`);
    expect(types.getDataTypeNameByCode(t.columnsByName.v.type) === 'text' || t.columnsByName.v.type.code === types.dataTypes.varchar, 'v type');
    const tt = await c.metadata.getTable(KS, 'types');
    const codes = { varchar: 'text' }; // the schema stores varchar as text
    for (const [l, ty] of T) {
      const col = tt.columnsByName[`c_${l}`];
      const want = ty.startsWith('frozen<addr') || ty === 'addr' ? types.dataTypes.udt : ty.startsWith('frozen<list') ? types.dataTypes.list
        : ty.startsWith('vector') ? types.dataTypes.custom : types.dataTypes[(codes[ty] || ty).split('<')[0]];
      expect(col && col.type.code === want, `${l}: code ${col && col.type.code} != ${want}`);
    }
  });

  await check('metadata.udt', async () => {
    const u = await c.metadata.getUdt(KS, 'addr');
    expect(u.fields.map((f) => f.name).join() === 'street,zip,tags' && u.fields[2].type.code === types.dataTypes.set, `udt ${JSON.stringify(u.fields)}`);
  });

  await check('metadata.index', async () => {
    const t = await c.metadata.getTable(KS, 'kv');
    const i = t.indexes.find((x) => x.name === 'kv_v');
    expect(i && i.target === 'v' && i.isCompositesKind(), `index ${JSON.stringify(i)}`);
  });

  await check('metadata.view', async () => {
    if (!FEATURES && DIALECT === 'cassandra') skip('materialized views are disabled');
    await c.execute('CREATE MATERIALIZED VIEW kv_by_v AS SELECT k, c, v FROM kv WHERE v IS NOT NULL AND k IS NOT NULL AND c IS NOT NULL PRIMARY KEY (v, k, c)');
    const v = await c.metadata.getMaterializedView(KS, 'kv_by_v');
    expect(v && v.tableName === 'kv' && v.partitionKeys[0].name === 'v' && v.clusteringKeys.length === 2, `view ${v && v.tableName}`);
  });

  await check('metadata.functions', async () => {
    if (!FEATURES) skip('UDFs are disabled');
    const lua = DIALECT === 'scylla';
    await c.execute(lua ? "CREATE FUNCTION twice(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x * 2'"
      : "CREATE FUNCTION twice(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE java AS 'return x * 2;'");
    await c.execute(lua ? "CREATE FUNCTION plus(acc int, x int) CALLED ON NULL INPUT RETURNS int LANGUAGE lua AS 'return (acc or 0) + (x or 0)'"
      : "CREATE FUNCTION plus(acc int, x int) CALLED ON NULL INPUT RETURNS int LANGUAGE java AS 'return (acc == null ? 0 : acc) + (x == null ? 0 : x);'");
    await c.execute('CREATE AGGREGATE total(int) SFUNC plus STYPE int INITCOND 0');
    const f = await c.metadata.getFunction(KS, 'twice', ['int']);
    expect(f && f.argumentNames.join() === 'x' && f.returnType.code === types.dataTypes.int && f.language === (lua ? 'lua' : 'java'), `function ${f && f.language}`);
    const a = await c.metadata.getAggregate(KS, 'total', ['int']);
    expect(a && a.stateFunction === 'plus' && a.stateType.code === types.dataTypes.int && a.initCondition === '0', `aggregate ${a && a.stateFunction} ${a && a.initCondition}`);
  });

  await check('schema_agreement', async () => {
    expect(await c.metadata.checkSchemaAgreement(), 'no agreement');
    const r = await c.execute('CREATE TABLE agree (k int PRIMARY KEY)');
    expect(r.info.isSchemaInAgreement, 'no agreement after DDL');
  });

  await check('prepared', async () => {
    for (let i = 0; i < 3; i++) await x('INSERT INTO kv (k, c, v) VALUES (?, ?, ?)', [1, i, `v${i}`]);
    const r = await x('SELECT c, v FROM kv WHERE k = :k AND c >= :lo', { k: 1, lo: 1 });
    expect(r.rows.map((row) => `${row.c}=${row.v}`).join() === '2=v2,1=v1', `rows ${r.rows.map((row) => row.c)}`);
    expect(r.columns.map((col) => col.name).join() === 'c,v', 'result metadata');
  });

  await check('paging', async () => {
    for (let i = 0; i < 250; i++) await x("INSERT INTO kv (k, c, v) VALUES (2, ?, 'p')", [i]);
    const r = await x('SELECT c FROM kv WHERE k = 2', [], { fetchSize: 50 });
    expect(r.rows.length === 50 && r.pageState, `first page ${r.rows.length}`);
    const r2 = await x('SELECT c FROM kv WHERE k = 2', [], { fetchSize: 50, pageState: r.pageState });
    expect(r2.rows[0].c === 199, `resumed at ${r2.rows[0].c}`);
    const all = [];
    for await (const row of await x('SELECT c FROM kv WHERE k = 2', [], { fetchSize: 50 })) all.push(row.c);
    expect(all.length === 250 && all[0] === 249 && all[249] === 0, `${all.length} rows`);
  });

  await check('batches', async () => {
    await c.batch([{ query: 'INSERT INTO kv (k, c, v) VALUES (?, ?, ?)', params: [3, 1, 'b1'] }, "INSERT INTO kv (k, c, v) VALUES (4, 1, 'b2')"], { prepare: true, logged: true });
    await c.batch([{ query: 'INSERT INTO kv (k, c, v) VALUES (?, ?, ?)', params: [3, 2, 'u1'] }, { query: 'INSERT INTO kv (k, c, v) VALUES (?, ?, ?)', params: [3, 3, 'u2'] }], { prepare: true, logged: false });
    const up = 'UPDATE counters SET n = n + ? WHERE k = ?';
    await c.batch([{ query: up, params: [types.Long.fromNumber(5), 1] }, { query: up, params: [types.Long.fromNumber(2), 1] }], { prepare: true, counter: true });
    expect((await c.execute('SELECT * FROM kv WHERE k = 3')).rowLength === 3, 'logged+unlogged rows');
    expect((await c.execute('SELECT n FROM counters WHERE k = 1')).first().n.toNumber() === 7, 'counter');
  });

  await check('lwt', async () => {
    expect((await x('INSERT INTO lwt (k, v, n) VALUES (?, ?, ?) IF NOT EXISTS', [1, 'first', 1])).wasApplied(), 'first insert applied');
    const r = await x('INSERT INTO lwt (k, v, n) VALUES (?, ?, ?) IF NOT EXISTS', [1, 'second', 2]);
    expect(!r.wasApplied() && r.first().v === 'first', 'second insert');
    expect((await x('UPDATE lwt SET v = ? WHERE k = ? IF n = ?', ['third', 1, 1])).wasApplied(), 'conditional update');
    expect(!(await x('UPDATE lwt SET v = ? WHERE k = ? IF n = ?', ['fourth', 1, 5])).wasApplied(), 'failed condition');
  });

  // ES6 Map/Set encoding for the values (the driver's own schema parsing needs the default, so
  // this is a second client).
  const tc = client(USER, PASS, { keyspace: KS, encoding: { map: Map, set: Set } });
  await tc.connect();
  for (let i = 0; i < T.length; i++) {
    const [label, , value] = T[i];
    await check(`types.${label}`, async () => {
      await tc.execute(`INSERT INTO types (k, c_${label}) VALUES (?, ?)`, [i, value], { prepare: true });
      const got = (await tc.execute(`SELECT c_${label} FROM types WHERE k = ?`, [i], { prepare: true })).first()[`c_${label}`];
      expect(same(value, got), `${got} != ${value}`);
    });
  }
  await tc.shutdown();

  await check('token_aware', async () => {
    for (const k of [1, 42, -7, 123456]) {
      await x("INSERT INTO kv (k, c, v) VALUES (?, 0, 't')", [k]);
      const key = Buffer.alloc(4);
      key.writeInt32BE(k);
      const tok = c.metadata.newToken(key);
      const server = (await x('SELECT token(k) AS t FROM kv WHERE k = ?', [k])).first().t;
      expect(tok.getValue().toString() === server.toString(), `k=${k}: driver ${tok.getValue()} server ${server}`);
      expect(c.getReplicas(KS, key).length === 1, 'replicas');
    }
  });

  await check('shard_aware', () => skip('not a shard-aware driver'));

  await check('tracing', async () => {
    const r = await c.execute('SELECT * FROM kv WHERE k = 1', [], { traceQuery: true });
    const tr = await c.metadata.getTrace(r.info.traceId);
    expect(tr.coordinator && tr.duration !== undefined && tr.events.length > 0, `trace ${JSON.stringify(tr).slice(0, 200)}`);
  });

  await check('events.schema', async () => {
    const other = client(USER, PASS);
    await other.connect();
    try {
      // The driver caches table metadata and drops it when a schema change event arrives.
      expect((await c.metadata.getTable(KS, 'kv')).columnsByName.w === undefined, 'w already there');
      await other.execute(`ALTER TABLE ${KS}.kv ADD w int`);
      await eventually(async () => (await c.metadata.getTable(KS, 'kv')).columnsByName.w !== undefined);
      await other.execute(`CREATE TABLE ${KS}.evt (k int PRIMARY KEY)`);
      await eventually(() => c.metadata.keyspaces[KS] && c.metadata.getTable(KS, 'evt').then((t) => t !== null));
      await other.execute(`DROP TABLE ${KS}.evt`);
      await eventually(async () => (await c.metadata.getTable(KS, 'evt')) === null);
    } finally {
      await other.shutdown();
    }
  });

  await check('reconnect', async () => {
    if (!RESTART) skip("the harness doesn't restart this server");
    const q = 'SELECT v FROM kv WHERE k = ? AND c = ?';
    expect((await x(q, [1, 1])).first().v === 'v1', 'before restart');
    out('@@RESTART');
    await new Promise((resolve) => readline.createInterface({ input: process.stdin }).once('line', resolve));
    await eventually(async () => (await x(q, [1, 1])).first().v === 'v1', 60000);
  });

  try {
    await c.execute(`DROP KEYSPACE IF EXISTS ${KS}`);
  } catch (e) { /* best effort */ }
  await c.shutdown();
  out(`DONE ${failed} failed`);
  process.exit(failed ? 1 : 0);
}

main().catch((e) => {
  out(`FAIL suite: ${e.stack || e}`.replace(/\n/g, ' '));
  process.exit(1);
});
