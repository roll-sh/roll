<?php
// elasticsearch-php 9.x against roll search: the shared client scenario, plus the client's
// product check (it throws ProductCheckException on responses without X-Elastic-Product).
//
// Usage: php es_client.php   (ES_URL, default http://127.0.0.1:59200)
require __DIR__ . '/vendor/autoload.php';

use Elastic\Elasticsearch\ClientBuilder;
use Elastic\Elasticsearch\Exception\ClientResponseException;

$url = getenv('ES_URL') ?: 'http://127.0.0.1:59200';
const IDX = 'clients-php';
const COLORS = ['red', 'green', 'blue'];
$failures = [];

function check(string $name, callable $fn): void
{
    global $failures;
    try {
        $fn();
        echo "ok   $name\n";
    } catch (Throwable $e) {
        $failures[] = $name;
        echo "FAIL $name: " . get_class($e) . ': ' . $e->getMessage() . "\n";
    }
}

function eq($got, $want, string $what): void
{
    if ($got !== $want) {
        throw new RuntimeException("$what: expected " . json_encode($want) . ', got ' . json_encode($got));
    }
}

function doc(int $i): array
{
    return [
        'title' => "document $i " . ($i % 2 ? 'quick brown fox' : 'lazy dog'),
        'tag' => COLORS[$i % 3],
        'n' => $i,
        'price' => $i * 1.5,
        'd' => sprintf('2024-%02d-%02d', $i % 12 + 1, $i % 28 + 1),
        'comments' => [['author' => 'a' . ($i % 5), 'stars' => $i % 5]],
    ];
}

$es = ClientBuilder::create()->setHosts([$url])->build();

check('info and product check', function () use ($es) {
    $info = $es->info()->asArray();
    eq($info['version']['number'], '9.5.5', 'version');
    eq($es->ping()->asBool(), true, 'ping');
});

check('create index with mappings', function () use ($es) {
    $es->indices()->delete(['index' => IDX . '*', 'ignore_unavailable' => true]);
    $r = $es->indices()->create(['index' => IDX, 'body' => [
        'settings' => ['number_of_shards' => 2, 'number_of_replicas' => 0],
        'mappings' => ['properties' => [
            'title' => ['type' => 'text', 'fields' => ['raw' => ['type' => 'keyword']]],
            'tag' => ['type' => 'keyword'], 'n' => ['type' => 'integer'], 'price' => ['type' => 'double'], 'd' => ['type' => 'date'],
            'comments' => ['type' => 'nested', 'properties' => ['author' => ['type' => 'keyword'], 'stars' => ['type' => 'integer']]],
        ]],
    ]])->asArray();
    eq($r['acknowledged'], true, 'acknowledged');
    eq($es->indices()->exists(['index' => IDX])->asBool(), true, 'exists');
});

check('bulk', function () use ($es) {
    $body = [];
    for ($i = 0; $i < 100; $i++) {
        $body[] = ['index' => ['_index' => IDX, '_id' => (string)$i]];
        $body[] = doc($i);
    }
    $r = $es->bulk(['body' => $body, 'refresh' => 'true'])->asArray();
    eq($r['errors'], false, 'errors');
    eq(count($r['items']), 100, 'items');
    eq($es->count(['index' => IDX])['count'], 100, 'count');
});

check('search with query and aggregations', function () use ($es) {
    $r = $es->search(['index' => IDX, 'body' => [
        'query' => ['bool' => ['must' => [['match' => ['title' => 'fox']]], 'filter' => [['range' => ['n' => ['gte' => 10, 'lt' => 60]]]]]],
        'aggs' => [
            'tags' => ['terms' => ['field' => 'tag'], 'aggs' => ['avg_n' => ['avg' => ['field' => 'n']]]],
            'months' => ['date_histogram' => ['field' => 'd', 'calendar_interval' => 'month']],
            'price' => ['stats' => ['field' => 'price']],
            'stars' => ['nested' => ['path' => 'comments'], 'aggs' => ['max' => ['max' => ['field' => 'comments.stars']]]],
        ],
        'size' => 5, 'sort' => [['n' => 'asc']], 'highlight' => ['fields' => ['title' => new stdClass()]],
    ]])->asArray();
    eq($r['hits']['total'], ['value' => 25, 'relation' => 'eq'], 'total');
    eq(array_map(fn($h) => $h['_id'], $r['hits']['hits']), ['11', '13', '15', '17', '19'], 'ids');
    eq($r['hits']['hits'][0]['highlight']['title'], ['document 11 quick brown <em>fox</em>'], 'highlight');
    $b = [];
    foreach ($r['aggregations']['tags']['buckets'] as $x) {
        $b[$x['key']] = [$x['doc_count'], $x['avg_n']['value']];
    }
    eq($b, ['blue' => [9, 35.0], 'green' => [8, 34.0], 'red' => [8, 36.0]], 'terms');
    eq($r['aggregations']['price']['count'], 25, 'stats.count');
    eq($r['aggregations']['stars']['max']['value'], 4.0, 'nested max');
    eq(array_sum(array_map(fn($x) => $x['doc_count'], $r['aggregations']['months']['buckets'])), 25, 'date_histogram');
});

check('documents: get, update, conflicts, delete', function () use ($es) {
    eq($es->get(['index' => IDX, 'id' => '5'])['_source']['n'], 5, 'get');
    eq($es->exists(['index' => IDX, 'id' => 'nope'])->asBool(), false, 'exists');
    eq($es->update(['index' => IDX, 'id' => '5', 'refresh' => 'true', 'body' => ['doc' => ['n' => 500]]])['result'], 'updated', 'update');
    try {
        $es->index(['index' => IDX, 'id' => '5', 'if_seq_no' => 0, 'if_primary_term' => 1, 'body' => ['n' => 1]]);
        throw new RuntimeException('expected a conflict');
    } catch (ClientResponseException $e) {
        eq($e->getCode(), 409, 'status');
        eq(json_decode((string)$e->getResponse()->getBody(), true)['error']['type'], 'version_conflict_engine_exception', 'type');
    }
    $es->index(['index' => IDX, 'id' => 'tmp', 'refresh' => 'wait_for', 'body' => ['n' => -1]]);
    eq($es->delete(['index' => IDX, 'id' => 'tmp', 'refresh' => 'true'])['result'], 'deleted', 'delete');
});

check('scroll', function () use ($es) {
    $r = $es->search(['index' => IDX, 'scroll' => '1m', 'size' => 7]);
    $seen = count($r['hits']['hits']);
    $sid = $r['_scroll_id'];
    while (true) {
        $s = $es->scroll(['body' => ['scroll_id' => $sid, 'scroll' => '1m']]);
        if (count($s['hits']['hits']) === 0) {
            break;
        }
        $seen += count($s['hits']['hits']);
        $sid = $s['_scroll_id'];
    }
    eq($seen, 100, 'scrolled');
    eq($es->clearScroll(['body' => ['scroll_id' => $sid]])['succeeded'], true, 'clear');
});

check('point in time with search_after', function () use ($es) {
    $pit = $es->openPointInTime(['index' => IDX, 'keep_alive' => '1m'])['id'];
    $seen = [];
    $after = null;
    while (true) {
        $body = ['pit' => ['id' => $pit, 'keep_alive' => '1m'], 'size' => 30, 'sort' => [['n' => 'asc']]];
        if ($after !== null) {
            $body['search_after'] = $after;
        }
        $r = $es->search(['body' => $body]);
        $hits = $r['hits']['hits'];
        if (count($hits) === 0) {
            break;
        }
        foreach ($hits as $h) {
            $seen[$h['_id']] = true;
        }
        $after = end($hits)['sort'];
        $pit = $r['pit_id'];
    }
    eq(count($seen), 100, 'paged');
    eq($es->closePointInTime(['body' => ['id' => $pit]])['succeeded'], true, 'close');
});

check('update_by_query and delete_by_query', function () use ($es) {
    $u = $es->updateByQuery(['index' => IDX, 'refresh' => true, 'body' => ['query' => ['term' => ['tag' => 'blue']], 'script' => ['source' => 'ctx._source.n += params.k', 'params' => ['k' => 1000]]]]);
    eq($u['updated'], 33, 'updated');
    $d = $es->deleteByQuery(['index' => IDX, 'refresh' => true, 'body' => ['query' => ['term' => ['tag' => 'green']]]]);
    eq($d['deleted'], 33, 'deleted');
    eq($es->count(['index' => IDX])['count'], 67, 'count');
});

check('errors', function () use ($es) {
    try {
        $es->search(['index' => 'clients-php-missing']);
        throw new RuntimeException('expected 404');
    } catch (ClientResponseException $e) {
        eq($e->getCode(), 404, 'status');
        eq(json_decode((string)$e->getResponse()->getBody(), true)['error']['type'], 'index_not_found_exception', 'type');
    }
});

check('cat and cluster', function () use ($es) {
    eq($es->cluster()->health(['index' => IDX])['status'], 'green', 'health');
    eq(array_map(fn($r) => $r['index'], $es->cat()->indices(['index' => IDX . '*', 'format' => 'json', 'h' => 'index'])->asArray()), [IDX], 'cat');
});

check('delete index', function () use ($es) {
    eq($es->indices()->delete(['index' => IDX . '*'])['acknowledged'], true, 'acknowledged');
});

if ($failures) {
    echo 'elasticsearch-php: ' . count($failures) . ' check(s) failed: ' . implode(', ', $failures) . "\n";
    exit(1);
}
echo "elasticsearch-php: all checks passed\n";
