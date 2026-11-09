// roll queues compatibility scenarios for Node amqplib (promise API).
'use strict';

const crypto = require('node:crypto');
const path = require('node:path');
const amqp = require('amqplib');

// amqplib has no public tx API; its method table is used with Channel#rpc for the tx scenario.
const defs = require(path.join(path.dirname(require.resolve('amqplib')), 'lib', 'defs.js'));

const URL = process.env.AMQP_URL || 'amqp://guest:guest@localhost:5672/%2F';
const P = `amqplib-${crypto.randomBytes(3).toString('hex')}`;
const SCENARIOS = [];
const scenario = (name, fn) => SCENARIOS.push([name, fn]);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const noop = () => {};
const open = new Set();

function check(cond, msg) {
  if (!cond) throw new Error(`assertion failed: ${msg}`);
}

async function connect(query = '') {
  const c = await amqp.connect(URL + query);
  c.on('error', (e) => console.error('connection error:', e.message));
  open.add(c);
  c.on('close', () => open.delete(c));
  return c;
}

async function channel(c, confirm = false) {
  const ch = confirm ? await c.createConfirmChannel() : await c.createChannel();
  ch.on('error', noop); // channel errors surface as rejected operations
  return ch;
}

const count = async (ch, q) => (await ch.checkQueue(q)).messageCount;

async function getWait(ch, q, timeout = 5000) {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    const m = await ch.get(q);
    if (m) return m;
    await sleep(50);
  }
  throw new Error(`no message on ${q} within ${timeout}ms`);
}

async function expectClose(promise, code) {
  try {
    await promise;
  } catch (e) {
    check(e.code === code, `expected ${code}, got ${e.code} ${e.message}`);
    return e.message;
  }
  throw new Error(`expected channel error ${code}`);
}

async function waitFor(cond, timeout, what) {
  const end = Date.now() + timeout;
  while (!cond()) {
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await sleep(20);
  }
}

scenario('connect', async () => {
  const c = await connect();
  const props = c.connection.serverProperties;
  const ch = await channel(c);
  await ch.close();
  await c.close();
  return `${props.product} ${props.version}`;
});

scenario('declare', async () => {
  const c = await connect();
  let ch = await channel(c);
  const x = `${P}.declare.x`;
  const q = `${P}.declare.q`;
  await ch.assertExchange(x, 'direct', { durable: true });
  const ok = await ch.assertQueue(q, { durable: true });
  check(ok.queue === q && ok.messageCount === 0, `declare-ok ${JSON.stringify(ok)}`);
  await ch.bindQueue(q, x, 'k');
  await ch.checkExchange(x);
  const srv = (await ch.assertQueue('', { exclusive: true })).queue;
  check(srv.startsWith('amq.gen-'), `server-named queue ${srv}`);
  const text = await expectClose(ch.assertQueue(q, { durable: false }), 406);
  check(text.includes("inequivalent arg 'durable'"), text);
  ch = await channel(c);
  await expectClose(ch.checkQueue(`${P}.missing`), 404);
  ch = await channel(c);
  await ch.unbindQueue(q, x, 'k');
  await ch.deleteQueue(q);
  await ch.deleteExchange(x);
  await c.close();
});

scenario('publish_confirms', async () => {
  const c = await connect();
  const ch = await channel(c, true);
  const q = `${P}.confirms`;
  await ch.assertQueue(q, { durable: true });
  const returned = [];
  ch.on('return', (m) => returned.push(m.fields.routingKey));
  for (let i = 0; i < 100; i++) ch.sendToQueue(q, Buffer.from(`m${i}`), { persistent: true, mandatory: true });
  await ch.waitForConfirms();
  check(returned.length === 0, `returned ${returned}`);
  check((await count(ch, q)) === 100, `count ${await count(ch, q)}`);
  ch.publish('', `${P}.nowhere`, Buffer.from('x'), { mandatory: true });
  await ch.waitForConfirms();
  check(returned.length === 1 && returned[0] === `${P}.nowhere`, `returns before ack: ${returned}`);
  await ch.deleteQueue(q);
  await c.close();
  return '100 acked, mandatory return ok';
});

scenario('consume_ack', async () => {
  const c = await connect();
  const ch = await channel(c, true);
  const q = `${P}.consume`;
  await ch.assertQueue(q);
  for (let i = 0; i < 10; i++) ch.sendToQueue(q, Buffer.from(`m${i}`));
  await ch.waitForConfirms();
  const got = [];
  const { consumerTag } = await ch.consume(q, (m) => {
    got.push(m.content.toString());
    ch.ack(m);
  });
  await waitFor(() => got.length >= 10, 5000, `10 deliveries (got ${got.length})`);
  await ch.cancel(consumerTag);
  check(got.join() === [...Array(10).keys()].map((i) => `m${i}`).join(), `order ${got}`);
  check((await count(ch, q)) === 0, 'queue not empty after acks');
  await ch.deleteQueue(q);
  await c.close();
});

scenario('nack_requeue', async () => {
  const c = await connect();
  const ch = await channel(c);
  const q = `${P}.nack`;
  await ch.assertQueue(q);
  ch.sendToQueue(q, Buffer.from('again'));
  const seen = [];
  const { consumerTag } = await ch.consume(q, (m) => {
    seen.push([m.fields.redelivered, m.content.toString()]);
    if (seen.length === 1) ch.nack(m, false, true);
    else ch.ack(m);
  });
  await waitFor(() => seen.length >= 2, 5000, 'redelivery');
  await ch.cancel(consumerTag);
  check(!seen[0][0], 'first delivery flagged redelivered');
  check(seen[1][0] && seen[1][1] === 'again', 'requeued delivery not redelivered');
  await ch.deleteQueue(q);
  await c.close();
});

scenario('reject_dlx', async () => {
  const c = await connect();
  const ch = await channel(c);
  const dlx = `${P}.dlx`;
  const dlq = `${P}.dlq`;
  const q = `${P}.work`;
  await ch.assertExchange(dlx, 'fanout');
  await ch.assertQueue(dlq);
  await ch.bindQueue(dlq, dlx, '');
  await ch.assertQueue(q, { deadLetterExchange: dlx });
  ch.sendToQueue(q, Buffer.from('bad'));
  ch.reject(await getWait(ch, q), false);
  const m = await getWait(ch, dlq);
  const death = m.properties.headers['x-death'][0];
  check(m.content.toString() === 'bad', 'body');
  check(death.reason === 'rejected' && death.queue === q && Number(death.count) === 1, `x-death ${JSON.stringify(death)}`);
  for (const n of [q, dlq]) await ch.deleteQueue(n);
  await ch.deleteExchange(dlx);
  await c.close();
});

scenario('topic', async () => {
  const c = await connect();
  const ch = await channel(c, true);
  const x = `${P}.topic`;
  const q1 = `${P}.t1`;
  const q2 = `${P}.t2`;
  await ch.assertExchange(x, 'topic');
  for (const [q, k] of [[q1, 'a.*'], [q2, '#.z']]) {
    await ch.assertQueue(q);
    await ch.bindQueue(q, x, k);
  }
  const returned = [];
  ch.on('return', (m) => returned.push(m.fields.routingKey));
  for (const k of ['a.b', 'a.b.z', 'x.z', 'a.z', 'b.c']) ch.publish(x, k, Buffer.from(k), { mandatory: true });
  await ch.waitForConfirms();
  check(returned.join() === 'b.c', `unroutable ${returned}`);
  const counts = [await count(ch, q1), await count(ch, q2)];
  check(counts.join() === '2,3', `counts ${counts}`);
  for (const q of [q1, q2]) await ch.deleteQueue(q);
  await ch.deleteExchange(x);
  await c.close();
});

scenario('headers', async () => {
  const c = await connect();
  const ch = await channel(c, true);
  const x = `${P}.headers`;
  const qa = `${P}.hall`;
  const qb = `${P}.hany`;
  await ch.assertExchange(x, 'headers');
  await ch.assertQueue(qa);
  await ch.assertQueue(qb);
  await ch.bindQueue(qa, x, '', { 'x-match': 'all', format: 'pdf', type: 'report' });
  await ch.bindQueue(qb, x, '', { 'x-match': 'any', format: 'zip', type: 'log' });
  const returned = [];
  ch.on('return', (m) => returned.push(JSON.stringify(m.properties.headers)));
  const hs = [{ format: 'pdf', type: 'report' }, { format: 'pdf' }, { type: 'log' }, { format: 'zip', type: 'x' }];
  for (const h of hs) ch.publish(x, '', Buffer.from('h'), { headers: h, mandatory: true });
  await ch.waitForConfirms();
  check(returned.join() === JSON.stringify({ format: 'pdf' }), `unroutable ${returned}`);
  const counts = [await count(ch, qa), await count(ch, qb)];
  check(counts.join() === '1,2', `counts ${counts}`);
  for (const q of [qa, qb]) await ch.deleteQueue(q);
  await ch.deleteExchange(x);
  await c.close();
});

scenario('tx', async () => {
  const txSelect = (ch) => ch.rpc(defs.TxSelect, {}, defs.TxSelectOk);
  const txCommit = (ch) => ch.rpc(defs.TxCommit, {}, defs.TxCommitOk);
  const txRollback = (ch) => ch.rpc(defs.TxRollback, {}, defs.TxRollbackOk);
  const c = await connect();
  let ch = await channel(c);
  const q = `${P}.tx`;
  await ch.assertQueue(q);
  await txSelect(ch);
  ch.sendToQueue(q, Buffer.from('rolled back'));
  await txRollback(ch);
  check((await count(ch, q)) === 0, 'rollback left a message');
  ch.sendToQueue(q, Buffer.from('committed'));
  ch.sendToQueue(q, Buffer.from('committed'));
  await txCommit(ch);
  check((await count(ch, q)) === 2, `commit count ${await count(ch, q)}`);
  ch.ack(await ch.get(q));
  await txRollback(ch);
  await ch.close(); // the rolled-back ack leaves the message unacked, so closing requeues it
  ch = await channel(c);
  check((await count(ch, q)) === 2, 'ack was not rolled back');
  await txSelect(ch);
  ch.ack(await ch.get(q));
  await txCommit(ch);
  await ch.close();
  ch = await channel(c);
  check((await count(ch, q)) === 1, 'ack was not committed');
  await ch.deleteQueue(q);
  await c.close();
  return 'via Channel#rpc (amqplib has no public tx API)';
});

scenario('prefetch', async () => {
  const c = await connect();
  let ch = await channel(c, true);
  const q = `${P}.prefetch`;
  await ch.assertQueue(q);
  for (let i = 0; i < 20; i++) ch.sendToQueue(q, Buffer.from(String(i)));
  await ch.waitForConfirms();
  await ch.prefetch(5);
  const msgs = [];
  await ch.consume(q, (m) => msgs.push(m));
  await sleep(1000);
  check(msgs.length === 5, `received ${msgs.length} with prefetch 5`);
  ch.ack(msgs[msgs.length - 1], true);
  await sleep(1000);
  check(msgs.length === 10, `received ${msgs.length} after acking 5`);
  await ch.close();
  ch = await channel(c);
  await ch.deleteQueue(q);
  await c.close();
});

scenario('ttl', async () => {
  const c = await connect();
  const ch = await channel(c, true);
  const q1 = `${P}.ttlq`;
  const q2 = `${P}.ttlm`;
  await ch.assertQueue(q1, { messageTtl: 200 });
  await ch.assertQueue(q2);
  ch.sendToQueue(q1, Buffer.from('x'));
  ch.sendToQueue(q2, Buffer.from('short'), { expiration: '200' });
  ch.sendToQueue(q2, Buffer.from('long'), { expiration: '60000' });
  await ch.waitForConfirms();
  await sleep(700);
  check((await ch.get(q1, { noAck: true })) === false, 'queue TTL did not expire');
  const m = await ch.get(q2, { noAck: true });
  check(m && m.content.toString() === 'long', `per-message TTL: got ${m && m.content}`);
  for (const q of [q1, q2]) await ch.deleteQueue(q);
  await c.close();
});

scenario('priority', async () => {
  const c = await connect();
  const ch = await channel(c, true);
  const q = `${P}.prio`;
  await ch.assertQueue(q, { maxPriority: 10 });
  for (const p of [1, 9, 5, 0, 7]) ch.sendToQueue(q, Buffer.from(String(p)), { priority: p });
  await ch.waitForConfirms();
  const got = [];
  for (let i = 0; i < 5; i++) got.push(Number((await ch.get(q, { noAck: true })).content.toString()));
  check(got.join() === '9,7,5,1,0', `order ${got}`);
  await ch.deleteQueue(q);
  await c.close();
});

scenario('exclusive_autodelete', async () => {
  const a = await connect();
  const b = await connect();
  const cha = await channel(a);
  const { queue: q } = await cha.assertQueue('', { exclusive: true });
  let chb = await channel(b);
  await expectClose(chb.checkQueue(q), 405);
  await a.close();
  chb = await channel(b);
  await expectClose(chb.checkQueue(q), 404);
  chb = await channel(b);
  const ad = `${P}.autodel`;
  await chb.assertQueue(ad, { autoDelete: true });
  const { consumerTag } = await chb.consume(ad, noop);
  await chb.cancel(consumerTag);
  await expectClose(chb.checkQueue(ad), 404);
  await b.close();
});

scenario('direct_reply_to', async () => {
  const c = await connect();
  const client = await channel(c);
  const server = await channel(c);
  const q = `${P}.rpc`;
  await server.assertQueue(q);
  await server.consume(q, (m) => {
    if (m === null) return; // cancelled when the queue is deleted below
    const { replyTo, correlationId } = m.properties;
    server.publish('', replyTo, Buffer.from(m.content.toString().toUpperCase()), { correlationId });
    server.ack(m);
  });
  const replies = [];
  await client.consume('amq.rabbitmq.reply-to', (m) => replies.push([m.properties.correlationId, m.content.toString()]), {
    noAck: true,
  });
  client.sendToQueue(q, Buffer.from('ping'), { replyTo: 'amq.rabbitmq.reply-to', correlationId: 'c1' });
  await waitFor(() => replies.length > 0, 5000, 'reply');
  await sleep(100);
  check(JSON.stringify(replies) === JSON.stringify([['c1', 'PING']]), `replies ${JSON.stringify(replies)}`);
  await server.deleteQueue(q);
  await c.close();
});

scenario('basic_get', async () => {
  const c = await connect();
  const ch = await channel(c, true);
  const q = `${P}.get`;
  await ch.assertQueue(q);
  for (let i = 0; i < 3; i++) ch.sendToQueue(q, Buffer.from(String(i)));
  await ch.waitForConfirms();
  const m = await ch.get(q);
  check(m.fields.messageCount === 2 && m.content.toString() === '0', `get-ok message_count ${m.fields.messageCount}`);
  ch.ack(m);
  await ch.get(q, { noAck: true });
  await ch.get(q, { noAck: true });
  check((await ch.get(q)) === false, 'expected get-empty');
  await ch.deleteQueue(q);
  await c.close();
});

scenario('cancel_notify', async () => {
  const c = await connect();
  const ch = await channel(c);
  const other = await channel(c);
  const q = `${P}.cancel`;
  await ch.assertQueue(q);
  const cancelled = [];
  ch.on('cancel', (fields) => cancelled.push(fields.consumerTag));
  let nullDelivery = false;
  const { consumerTag } = await ch.consume(q, (m) => {
    if (m === null) nullDelivery = true;
  });
  await other.deleteQueue(q);
  await waitFor(() => cancelled.length > 0, 5000, 'basic.cancel');
  check(cancelled.join() === consumerTag, `cancel notifications ${cancelled}`);
  check(nullDelivery, 'consumer callback not called with null');
  await c.close();
});

scenario('connection_blocked', async () => {
  const c = await connect();
  const caps = c.connection.serverProperties.capabilities;
  check(caps && caps['connection.blocked'] === true, `capabilities ${JSON.stringify(caps)}`);
  c.on('blocked', noop);
  c.on('unblocked', noop);
  await c.close();
});

scenario('heartbeat', async () => {
  const c = await connect('?heartbeat=2');
  check(c.connection.heartbeat === 2, `negotiated heartbeat ${c.connection.heartbeat}`);
  let closed = false;
  c.on('close', () => {
    closed = true;
  });
  await sleep(7000);
  check(!closed, 'connection closed while idle');
  const ch = await channel(c);
  await ch.assertQueue('', { exclusive: true });
  await c.close();
  return '2s heartbeat, idle 7s';
});

scenario('large_message', async () => {
  const c = await connect();
  const ch = await channel(c, true);
  const q = `${P}.large`;
  await ch.assertQueue(q);
  const body = crypto.randomBytes(5 * 1024 * 1024);
  ch.sendToQueue(q, body);
  await ch.waitForConfirms();
  const m = await ch.get(q, { noAck: true });
  check(m && m.content.equals(body), 'body mismatch');
  await ch.deleteQueue(q);
  await c.close();
  return '5 MiB';
});

scenario('throughput', async () => {
  const n = 10_000;
  const c = await connect();
  const ch = await channel(c);
  const q = `${P}.tput`;
  await ch.assertQueue(q);
  const body = Buffer.alloc(100, 'x');
  const t0 = performance.now();
  for (let i = 0; i < n; i++) {
    if (!ch.sendToQueue(q, body)) await new Promise((r) => ch.once('drain', r));
  }
  await ch.checkQueue(q); // round trip: everything before it has been processed
  const pub = (performance.now() - t0) / 1000;
  await ch.prefetch(500);
  let got = 0;
  const t1 = performance.now();
  const { consumerTag } = await ch.consume(q, (m) => {
    got++;
    if (got % 100 === 0 || got === n) ch.ack(m, true);
  });
  await waitFor(() => got >= n, 10000, `consume (got ${got})`);
  const con = (performance.now() - t1) / 1000;
  await ch.cancel(consumerTag);
  await ch.deleteQueue(q);
  await c.close();
  return `publish ${Math.round(n / pub)} msg/s, consume ${Math.round(n / con)} msg/s`;
});

async function main() {
  const only = new Set(process.argv.slice(2));
  let failed = 0;
  for (const [name, fn] of SCENARIOS) {
    if (only.size && !only.has(name)) continue;
    const t = performance.now();
    let timer;
    try {
      const timeout = new Promise((_, rej) => {
        timer = setTimeout(() => rej(new Error('scenario timed out after 15s')), 15000);
      });
      const detail = (await Promise.race([fn(), timeout])) || '';
      console.log(`PASS ${name} ${detail} (${((performance.now() - t) / 1000).toFixed(1)}s)`.replace('  ', ' '));
    } catch (e) {
      failed++;
      console.log(`FAIL ${name}: ${e.message}`);
      console.error(e.stack);
    } finally {
      clearTimeout(timer);
      for (const c of open) c.close().catch(noop);
      open.clear();
    }
  }
  process.exit(failed ? 1 : 0);
}

main();
