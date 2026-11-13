# elasticsearch-ruby 9.x against roll search: the shared client scenario, plus the client's
# product check (it verifies the X-Elastic-Product header before the first request).
#
# Usage: ruby es_client.rb   (ES_URL, default http://127.0.0.1:59200)
require 'elasticsearch'
require 'elasticsearch/helpers/bulk_helper'
require 'elasticsearch/helpers/scroll_helper'

URL = ENV.fetch('ES_URL', 'http://127.0.0.1:59200')
IDX = 'clients-rb'
COLORS = %w[red green blue].freeze
$failures = []

def check(name)
  yield
  puts "ok   #{name}"
rescue StandardError, ScriptError => e
  $failures << name
  puts "FAIL #{name}: #{e.class}: #{e.message}"
  puts e.backtrace.first(5)
end

def eq(got, want, what)
  raise "#{what}: expected #{want.inspect}, got #{got.inspect}" unless got == want
end

def doc(i)
  { title: "document #{i} #{i.odd? ? 'quick brown fox' : 'lazy dog'}", tag: COLORS[i % 3], n: i, price: i * 1.5,
    d: format('2024-%02d-%02d', i % 12 + 1, i % 28 + 1), comments: [{ author: "a#{i % 5}", stars: i % 5 }] }
end

client = Elasticsearch::Client.new(url: URL, request_timeout: 60)

check('info and product check') do
  info = client.info
  eq(info['version']['number'], '9.5.5', 'version')
  eq(client.ping, true, 'ping')
end

check('create index with mappings') do
  client.indices.delete(index: "#{IDX}*", ignore_unavailable: true)
  r = client.indices.create(index: IDX, body: {
    settings: { number_of_shards: 2, number_of_replicas: 0 },
    mappings: { properties: {
      title: { type: 'text', fields: { raw: { type: 'keyword' } } }, tag: { type: 'keyword' }, n: { type: 'integer' },
      price: { type: 'double' }, d: { type: 'date' },
      comments: { type: 'nested', properties: { author: { type: 'keyword' }, stars: { type: 'integer' } } }
    } }
  })
  eq(r['acknowledged'], true, 'acknowledged')
  eq(client.indices.exists(index: IDX), true, 'exists')
end

check('bulk') do
  body = (0...100).flat_map { |i| [{ index: { _index: IDX, _id: i.to_s } }, doc(i)] }
  r = client.bulk(body: body, refresh: true)
  eq(r['errors'], false, 'errors')
  eq(client.count(index: IDX)['count'], 100, 'count')
end

check('BulkHelper') do
  # The helper indexes without ids (and updates by an 'id' key); its slices are separate bulks.
  helper = Elasticsearch::Helpers::BulkHelper.new(client, "#{IDX}-helper")
  slices = 0
  helper.ingest((0...40).map { |i| { n: i } }, { refresh: true, slice: 15 }) { |r, _| slices += 1 unless r['errors'] }
  eq(slices, 3, 'slices')
  eq(client.count(index: "#{IDX}-helper")['count'], 40, 'count')
  ids = client.search(index: "#{IDX}-helper", body: { size: 3, sort: [{ n: 'asc' }] })['hits']['hits'].map { |h| h['_id'] }
  helper.update(ids.map { |id| { 'id' => id, 'n' => 1000 } }, refresh: true)
  eq(client.count(index: "#{IDX}-helper", body: { query: { term: { n: 1000 } } })['count'], 3, 'updated')
  helper.delete(ids, refresh: true)
  eq(client.count(index: "#{IDX}-helper")['count'], 37, 'deleted')
end

check('search with query and aggregations') do
  r = client.search(index: IDX, body: {
    query: { bool: { must: [{ match: { title: 'fox' } }], filter: [{ range: { n: { gte: 10, lt: 60 } } }] } },
    aggs: {
      tags: { terms: { field: 'tag' }, aggs: { avg_n: { avg: { field: 'n' } } } },
      months: { date_histogram: { field: 'd', calendar_interval: 'month' } },
      price: { stats: { field: 'price' } },
      stars: { nested: { path: 'comments' }, aggs: { max: { max: { field: 'comments.stars' } } } }
    },
    size: 5, sort: [{ n: 'asc' }], highlight: { fields: { title: {} } }
  })
  eq(r['hits']['total'], { 'value' => 25, 'relation' => 'eq' }, 'total')
  eq(r['hits']['hits'].map { |h| h['_id'] }, %w[11 13 15 17 19], 'ids')
  eq(r['hits']['hits'][0]['highlight']['title'], ['document 11 quick brown <em>fox</em>'], 'highlight')
  buckets = r['aggregations']['tags']['buckets'].to_h { |b| [b['key'], [b['doc_count'], b['avg_n']['value']]] }
  eq(buckets, { 'red' => [8, 36.0], 'green' => [8, 34.0], 'blue' => [9, 35.0] }, 'terms')
  eq(r['aggregations']['price']['count'], 25, 'stats.count')
  eq(r['aggregations']['stars']['max']['value'], 4.0, 'nested max')
  eq(r['aggregations']['months']['buckets'].sum { |b| b['doc_count'] }, 25, 'date_histogram')
end

check('documents: get, update, conflicts, delete') do
  eq(client.get(index: IDX, id: '5')['_source']['n'], 5, 'get')
  eq(client.exists(index: IDX, id: 'nope'), false, 'exists')
  eq(client.mget(index: IDX, body: { ids: %w[1 2 nope] })['docs'].map { |d| d['found'] }, [true, true, false], 'mget')
  eq(client.update(index: IDX, id: '5', body: { doc: { n: 500 } }, refresh: true)['result'], 'updated', 'update')
  begin
    client.index(index: IDX, id: '5', body: { n: 1 }, if_seq_no: 0, if_primary_term: 1)
    raise 'expected a conflict'
  rescue Elastic::Transport::Transport::Errors::Conflict => e
    raise "unexpected message #{e.message}" unless e.message.include?('version_conflict_engine_exception')
  end
  client.index(index: IDX, id: 'tmp', body: { n: -1 }, refresh: 'wait_for')
  eq(client.delete(index: IDX, id: 'tmp', refresh: true)['result'], 'deleted', 'delete')
end

check('ScrollHelper') do
  helper = Elasticsearch::Helpers::ScrollHelper.new(client, IDX, { size: 7, query: { match_all: {} } })
  n = 0
  helper.each { |_| n += 1 }
  helper.clear
  eq(n, 100, 'scrolled')
end

check('point in time with search_after') do
  pit = client.open_point_in_time(index: IDX, keep_alive: '1m')['id']
  seen = []
  after = nil
  loop do
    body = { pit: { id: pit, keep_alive: '1m' }, size: 30, sort: [{ n: 'asc' }] }
    body[:search_after] = after if after
    r = client.search(body: body)
    hits = r['hits']['hits']
    break if hits.empty?

    seen += hits.map { |h| h['_id'] }
    after = hits.last['sort']
    pit = r['pit_id']
  end
  eq(seen.uniq.size, 100, 'paged')
  eq(client.close_point_in_time(body: { id: pit })['succeeded'], true, 'close')
end

check('update_by_query and delete_by_query') do
  u = client.update_by_query(index: IDX, refresh: true, body: { query: { term: { tag: 'blue' } }, script: { source: 'ctx._source.n += params.k', params: { k: 1000 } } })
  eq(u['updated'], 33, 'updated')
  d = client.delete_by_query(index: IDX, refresh: true, body: { query: { term: { tag: 'green' } } })
  eq(d['deleted'], 33, 'deleted')
  eq(client.count(index: IDX)['count'], 67, 'count')
end

check('errors') do
  begin
    client.search(index: 'clients-rb-missing')
    raise 'expected 404'
  rescue Elastic::Transport::Transport::Errors::NotFound => e
    raise "unexpected message #{e.message}" unless e.message.include?('index_not_found_exception')
  end
  begin
    client.search(index: IDX, body: { query: { bogus: {} } })
    raise 'expected 400'
  rescue Elastic::Transport::Transport::Errors::BadRequest => e
    raise "unexpected message #{e.message}" unless e.message.include?('unknown query [bogus]')
  end
end

check('cat and cluster') do
  eq(client.cluster.health(index: IDX)['status'], 'green', 'health')
  eq(client.cat.indices(index: "#{IDX}*", format: 'json', h: 'index').map { |r| r['index'] }.sort, [IDX, "#{IDX}-helper"], 'cat')
end

check('delete index') do
  eq(client.indices.delete(index: "#{IDX}*")['acknowledged'], true, 'acknowledged')
end

if $failures.empty?
  puts 'elasticsearch-ruby: all checks passed'
else
  puts "elasticsearch-ruby: #{$failures.size} check(s) failed: #{$failures.join(', ')}"
  exit 1
end
