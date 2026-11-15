// @opensearch-project/opensearch against roll search's opensearch dialect: the shared client
// scenario through the OpenSearch client and its helpers.
//
// Usage: node os_client.mjs   (OS_URL, default http://127.0.0.1:59201)
import { Client } from '@opensearch-project/opensearch'
import assert from 'node:assert/strict'

const URL = process.env.OS_URL ?? 'http://127.0.0.1:59201'
const IDX = 'clients-osjs'
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

await check('info', async () => {
  const { body } = await client.info()
  assert.equal(body.version.distribution, 'opensearch')
  assert.match(body.version.number, /^3\./)
})

await check('create index', async () => {
  await client.indices.delete({ index: `${IDX}*` }, { ignore: [404] })
  const { body } = await client.indices.create({
    index: IDX,
    body: {
      settings: { number_of_shards: 2, number_of_replicas: 0 },
      mappings: { properties: { title: { type: 'text' }, tag: { type: 'keyword' }, n: { type: 'integer' } } }
    }
  })
  assert.equal(body.acknowledged, true)
})

await check('bulk helper', async () => {
  const r = await client.helpers.bulk({
    datasource: Array.from({ length: 60 }, (_, i) => ({ id: String(i), title: `doc ${i} ${i % 2 ? 'fox' : 'dog'}`, tag: COLORS[i % 3], n: i })),
    onDocument: (d) => { const { id, ...rest } = d; return [{ index: { _index: IDX, _id: id } }, rest] },
    refreshOnCompletion: IDX
  })
  assert.equal(r.successful, 60)
})

await check('search with aggregations', async () => {
  const { body } = await client.search({
    index: IDX,
    body: { query: { match: { title: 'fox' } }, aggs: { tags: { terms: { field: 'tag' } }, avg: { avg: { field: 'n' } } }, sort: [{ n: 'desc' }], size: 3 }
  })
  assert.equal(body.hits.total.value, 30)
  assert.deepEqual(body.hits.hits.map(h => h._id), ['59', '57', '55'])
  assert.equal(body.aggregations.avg.value, 30)
  assert.equal(body.aggregations.tags.buckets.reduce((a, b) => a + b.doc_count, 0), 30)
})

await check('scrollDocuments helper', async () => {
  let n = 0
  for await (const _ of client.helpers.scrollDocuments({ index: IDX, body: { size: 9, query: { match_all: {} } } })) n++
  assert.equal(n, 60)
})

await check('point in time', async () => {
  const { body: pit } = await client.createPit({ index: IDX, keep_alive: '1m' })
  const { body } = await client.search({ body: { pit: { id: pit.pit_id }, size: 100, sort: [{ n: 'asc' }] } })
  assert.equal(body.hits.hits.length, 60)
  await client.deletePit({ body: { pit_id: [pit.pit_id] } })
})

await check('update_by_query and delete_by_query', async () => {
  const { body: u } = await client.updateByQuery({ index: IDX, refresh: true, body: { query: { term: { tag: 'red' } }, script: { source: 'ctx._source.n += 1000' } } })
  assert.equal(u.updated, 20)
  const { body: d } = await client.deleteByQuery({ index: IDX, refresh: true, body: { query: { range: { n: { gte: 1000 } } } } })
  assert.equal(d.deleted, 20)
  const { body: c } = await client.count({ index: IDX })
  assert.equal(c.count, 40)
})

await check('errors', async () => {
  try {
    await client.search({ index: 'clients-osjs-missing' })
    assert.fail('expected 404')
  } catch (e) {
    assert.equal(e.meta.statusCode, 404)
    assert.equal(e.meta.body.error.type, 'index_not_found_exception')
  }
})

await check('delete index', async () => {
  const { body } = await client.indices.delete({ index: `${IDX}*` })
  assert.equal(body.acknowledged, true)
})

await client.close()
if (failures.length) {
  console.log(`@opensearch-project/opensearch: ${failures.length} check(s) failed: ${failures.join(', ')}`)
  process.exit(1)
}
console.log('@opensearch-project/opensearch: all checks passed')
