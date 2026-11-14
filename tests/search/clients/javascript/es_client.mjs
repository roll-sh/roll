// @elastic/elasticsearch 9.x against roll search: the shared client scenario (see
// tests/search/clients/README.md) plus the client's product check and its helpers (bulk,
// scrollDocuments, search, msearch).
//
// Usage: node es_client.mjs   (ES_URL, default http://127.0.0.1:59200)
import { Client, errors } from '@elastic/elasticsearch'
import assert from 'node:assert/strict'

const URL = process.env.ES_URL ?? 'http://127.0.0.1:59200'
const IDX = 'clients-js'
const COLORS = ['red', 'green', 'blue']
const client = new Client({ node: URL, requestTimeout: 60000 })
const failures = []

async function check (name, fn) {
  try {
    await fn()
    console.log(`ok   ${name}`)
  } catch (e) {
    failures.push(name)
    console.log(`FAIL ${name}: ${e?.stack ?? e}`)
    if (e?.meta?.body) console.log(JSON.stringify(e.meta.body))
  }
}

const doc = (i) => ({
  title: `document ${i} ${i % 2 ? 'quick brown fox' : 'lazy dog'}`,
  tag: COLORS[i % 3],
  n: i,
  price: i * 1.5,
  d: `2024-${String(i % 12 + 1).padStart(2, '0')}-${String(i % 28 + 1).padStart(2, '0')}`,
  comments: [{ author: `a${i % 5}`, stars: i % 5 }]
})

await check('info and product check', async () => {
  const info = await client.info()
  assert.equal(info.version.number, '9.5.5')
  assert.equal(info.tagline, 'You Know, for Search')
  assert.equal(await client.ping(), true)
})

await check('create index with mappings', async () => {
  await client.indices.delete({ index: `${IDX}*` }, { ignore: [404] })
  const r = await client.indices.create({
    index: IDX,
    settings: { number_of_shards: 2, number_of_replicas: 0 },
    mappings: {
      properties: {
        title: { type: 'text', fields: { raw: { type: 'keyword' } } },
        tag: { type: 'keyword' },
        n: { type: 'integer' },
        price: { type: 'double' },
        d: { type: 'date' },
        comments: { type: 'nested', properties: { author: { type: 'keyword' }, stars: { type: 'integer' } } }
      }
    }
  })
  assert.equal(r.acknowledged, true)
  assert.equal(await client.indices.exists({ index: IDX }), true)
})

await check('bulk helper', async () => {
  const r = await client.helpers.bulk({
    datasource: Array.from({ length: 100 }, (_, i) => ({ id: String(i), ...doc(i) })),
    onDocument: (d) => { const { id, ...rest } = d; return [{ index: { _index: IDX, _id: id } }, rest] },
    refreshOnCompletion: IDX,
    flushBytes: 2000
  })
  assert.equal(r.successful, 100)
  assert.equal(r.failed, 0)
  assert.equal((await client.count({ index: IDX })).count, 100)
})

await check('search with query and aggregations', async () => {
  const r = await client.search({
    index: IDX,
    query: { bool: { must: [{ match: { title: 'fox' } }], filter: [{ range: { n: { gte: 10, lt: 60 } } }] } },
    aggs: {
      tags: { terms: { field: 'tag' }, aggs: { avg_n: { avg: { field: 'n' } } } },
      months: { date_histogram: { field: 'd', calendar_interval: 'month' } },
      price: { stats: { field: 'price' } },
      stars: { nested: { path: 'comments' }, aggs: { max: { max: { field: 'comments.stars' } } } }
    },
    size: 5,
    sort: [{ n: 'asc' }],
    highlight: { fields: { title: {} } }
  })
  assert.deepEqual(r.hits.total, { value: 25, relation: 'eq' })
  assert.deepEqual(r.hits.hits.map(h => h._id), ['11', '13', '15', '17', '19'])
  assert.deepEqual(r.hits.hits[0].highlight.title, ['document 11 quick brown <em>fox</em>'])
  const b = Object.fromEntries(r.aggregations.tags.buckets.map(x => [x.key, [x.doc_count, x.avg_n.value]]))
  assert.deepEqual(b, { red: [8, 36], green: [8, 34], blue: [9, 35] })
  assert.equal(r.aggregations.price.count, 25)
  assert.equal(r.aggregations.stars.max.value, 4)
  assert.equal(r.aggregations.months.buckets.reduce((a, x) => a + x.doc_count, 0), 25)
})

await check('documents: get, mget, update, conflicts, delete', async () => {
  const g = await client.get({ index: IDX, id: '5' })
  assert.equal(g._source.n, 5)
  assert.equal(await client.exists({ index: IDX, id: 'nope' }), false)
  const m = await client.mget({ index: IDX, ids: ['1', '2', 'nope'] })
  assert.deepEqual(m.docs.map(d => d.found), [true, true, false])
  const u = await client.update({ index: IDX, id: '5', doc: { n: 500 }, refresh: true })
  assert.equal(u.result, 'updated')
  try {
    await client.index({ index: IDX, id: '5', document: { n: 1 }, if_seq_no: 0, if_primary_term: 1 })
    assert.fail('expected a conflict')
  } catch (e) {
    assert.ok(e instanceof errors.ResponseError, String(e))
    assert.equal(e.statusCode, 409)
    assert.equal(e.body.error.type, 'version_conflict_engine_exception')
  }
  await client.index({ index: IDX, id: 'tmp', document: { n: -1 }, refresh: 'wait_for' })
  assert.equal((await client.delete({ index: IDX, id: 'tmp', refresh: true })).result, 'deleted')
})

await check('scrollDocuments helper', async () => {
  let n = 0
  for await (const _ of client.helpers.scrollDocuments({ index: IDX, size: 7, query: { match_all: {} } })) n++
  assert.equal(n, 100)
})

await check('point in time with search_after', async () => {
  let { id } = await client.openPointInTime({ index: IDX, keep_alive: '1m' })
  const seen = []
  let after
  for (;;) {
    const r = await client.search({ pit: { id, keep_alive: '1m' }, size: 30, sort: [{ n: 'asc' }], ...(after ? { search_after: after } : {}) })
    if (r.hits.hits.length === 0) break
    seen.push(...r.hits.hits.map(h => h._id))
    after = r.hits.hits.at(-1).sort
    id = r.pit_id
  }
  assert.equal(seen.length, 100)
  assert.equal(new Set(seen).size, 100)
  assert.equal((await client.closePointInTime({ id })).succeeded, true)
})

await check('msearch helper', async () => {
  const m = client.helpers.msearch({ operations: 2 })
  const [a, b] = await Promise.all([
    m.search({ index: IDX }, { query: { term: { tag: 'red' } }, size: 0 }),
    m.search({ index: IDX }, { query: { term: { tag: 'nope' } } })
  ])
  m.stop()
  assert.equal(a.body.hits.total.value, 34)
  assert.equal(b.body.hits.total.value, 0)
})

await check('update_by_query and delete_by_query', async () => {
  const u = await client.updateByQuery({ index: IDX, query: { term: { tag: 'blue' } }, script: { source: 'ctx._source.n += params.k', params: { k: 1000 } }, refresh: true })
  assert.equal(u.updated, 33)
  const d = await client.deleteByQuery({ index: IDX, query: { term: { tag: 'green' } }, refresh: true })
  assert.equal(d.deleted, 33)
  assert.equal((await client.count({ index: IDX })).count, 67)
})

await check('errors', async () => {
  try {
    await client.search({ index: 'clients-js-missing' })
    assert.fail('expected 404')
  } catch (e) {
    assert.equal(e.statusCode, 404)
    assert.equal(e.body.error.type, 'index_not_found_exception')
  }
  try {
    await client.search({ index: IDX, query: { bogus: {} } })
    assert.fail('expected 400')
  } catch (e) {
    assert.equal(e.statusCode, 400)
    assert.equal(e.body.error.reason, 'unknown query [bogus]')
  }
})

await check('cat and cluster', async () => {
  const h = await client.cluster.health({ index: IDX })
  assert.equal(h.status, 'green')
  const rows = await client.cat.indices({ index: `${IDX}*`, format: 'json', h: 'index,docs.count' })
  assert.deepEqual(rows.map(r => r.index), [IDX])
})

await check('delete index', async () => {
  assert.equal((await client.indices.delete({ index: `${IDX}*` })).acknowledged, true)
})

await client.close()
if (failures.length) {
  console.log(`@elastic/elasticsearch: ${failures.length} check(s) failed: ${failures.join(', ')}`)
  process.exit(1)
}
console.log('@elastic/elasticsearch: all checks passed')
