#!/usr/bin/env python3
"""Generates an Elasticsearch snapshot of known data, and what Elasticsearch answers about it.

usage: generate.py <elasticsearch url> <repository location under path.repo> <docs per index> <expected.json> [global]

Indices: `mixed` (2 shards, routing, nested objects, geo points, a dense vector, deletes and
updates, several segments), `compressed` (best_compression), `excludes` (`_source.excludes`),
`synthetic` (synthetic _source) and `nosource` (`_source.enabled: false`), snapshotted into an
`fs` repository with a 4kb `chunk_size` (Lucene files in parts). `global` also snapshots global
state with a template, a pipeline and a lifecycle policy (Elasticsearch's own templates come
with it, so the committed fixtures leave it out).

The expected answers: every live document (_id, _routing, _source with its vectors,
_seq_no, _version, _primary_term, and _source as Elasticsearch keeps it), counts of a few
queries, and the top hits of a knn search. Run by scripts/search-es-snapshots.sh.
"""
import json
import random
import sys
import urllib.parse
import urllib.request

ES = sys.argv[1].rstrip('/')
LOCATION = sys.argv[2]
N = int(sys.argv[3])
OUT = sys.argv[4]
GLOBAL = len(sys.argv) > 5 and sys.argv[5] == 'global'
REPO = 'fixtures'
random.seed(7)


def req(method, path, body=None, ndjson=False, ok=()):
    data, headers = None, {}
    if body is not None:
        data = body.encode() if ndjson else json.dumps(body).encode()
        headers['Content-Type'] = 'application/x-ndjson' if ndjson else 'application/json'
    r = urllib.request.Request(ES + path, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(r) as resp:
            raw = resp.read()
            return json.loads(raw) if raw[:1] in (b'{', b'[') else raw.decode()
    except urllib.error.HTTPError as e:
        if e.code in ok:
            return None
        raise SystemExit(f'{method} {path}: {e.code} {e.read().decode()}')


WORDS = 'alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau'.split()
MAPPING = {
    'properties': {
        'title': {'type': 'text'},
        'tag': {'type': 'keyword'},
        'count': {'type': 'long'},
        'price': {'type': 'double'},
        'when': {'type': 'date'},
        'flag': {'type': 'boolean'},
        'loc': {'type': 'geo_point'},
        'people': {'type': 'nested', 'properties': {'name': {'type': 'keyword'}, 'age': {'type': 'integer'}}},
        'vec': {'type': 'dense_vector', 'dims': 4, 'index': True, 'similarity': 'cosine'},
        'secret': {'type': 'keyword'},
    }
}


def doc(i):
    return {
        'title': ' '.join(random.choice(WORDS) for _ in range(random.randint(2, 6))),
        'tag': random.choice(['red', 'green', 'blue', 'ünïcødé', 'a b']),
        'count': i,
        'price': round(random.uniform(0, 1000), 2),
        'when': f'2024-{1 + i % 12:02d}-{1 + i % 28:02d}T{i % 24:02d}:00:00Z',
        'flag': i % 2 == 0,
        'loc': {'lat': round(random.uniform(-80, 80), 4), 'lon': round(random.uniform(-170, 170), 4)},
        'people': [{'name': random.choice(WORDS), 'age': random.randint(1, 90)} for _ in range(i % 3)],
        'vec': [round(random.uniform(-1, 1), 3) for _ in range(4)],
        'secret': f's{i}',
    }


def id_of(i):
    # Every way Elasticsearch encodes an _id: numeric, generated (base64), UTF-8.
    return [str(1000 + i), None, f'doc-{i}', f'ïd/{i} with space', '0' + str(i)][i % 5]


def make(index, settings, mapping=MAPPING, segments=4, routing=False):
    req('DELETE', '/' + index, ok=(404,))
    req('PUT', '/' + index, {'settings': settings, 'mappings': mapping, 'aliases': {index + '-alias': {}}})
    per = max(1, N // segments)
    i = 0
    for _ in range(segments):
        lines = []
        for _ in range(per):
            meta = {'_index': index}
            if id_of(i) is not None:
                meta['_id'] = id_of(i)
            if routing and i % 3 == 0:
                meta['routing'] = f'r{i % 4}'
            lines += [json.dumps({'index': meta}), json.dumps(doc(i), ensure_ascii=False)]
            i += 1
        r = req('POST', '/_bulk?refresh=true', '\n'.join(lines) + '\n', ndjson=True)
        assert not r['errors'], r


def delete_and_update(index):
    hits = req('POST', f'/{index}/_search', {'size': 10000, 'sort': ['count']})['hits']['hits']
    for h in hits:
        c = h['_source']['count']
        q = f"?routing={urllib.parse.quote(h['_routing'])}" if '_routing' in h else ''
        path = f"/{index}/_doc/{urllib.parse.quote(h['_id'], safe='')}{q}"
        if c % 7 == 3:
            req('DELETE', path)
        elif c % 7 == 5:
            src = h['_source']
            src['title'] += ' updated'
            req('PUT', path, src)
    req('POST', f'/{index}/_refresh')


QUERIES = {
    'match_beta': {'match': {'title': 'beta'}},
    'term_red': {'term': {'tag': 'red'}},
    'range_count': {'range': {'count': {'gte': 10, 'lt': 30}}},
    'nested_age': {'nested': {'path': 'people', 'query': {'range': {'people.age': {'gte': 50}}}}},
    'geo': {'geo_distance': {'distance': '5000km', 'loc': {'lat': 0, 'lon': 0}}},
    'date': {'range': {'when': {'gte': '2024-06-01'}}},
}


def expected(index, nine):
    src = {'exclude_vectors': False} if nine else True
    hits = req('POST', f'/{index}/_search?seq_no_primary_term=true&version=true', {'size': 10000, 'sort': ['_doc'], '_source': src})['hits']['hits']
    docs = []
    for h in hits:
        q = f"?routing={urllib.parse.quote(h['_routing'])}" if '_routing' in h else ''
        raw = req('GET', f"/{index}/_source/{urllib.parse.quote(h['_id'], safe='')}{q}")
        raw = raw if isinstance(raw, str) else json.dumps(raw)
        docs.append({'_id': h['_id'], '_routing': h.get('_routing'), '_source': h.get('_source'), '_seq_no': h['_seq_no'],
                     '_version': h['_version'], '_primary_term': h['_primary_term'], 'source_raw': raw})
    counts = {k: req('POST', f'/{index}/_count', {'query': q})['count'] for k, q in QUERIES.items()}
    knn = req('POST', f'/{index}/_search', {'size': 3, '_source': False, 'knn': {'field': 'vec', 'query_vector': [0.1, 0.2, -0.3, 0.4], 'k': 3, 'num_candidates': 100}})
    return {'count': len(docs), 'docs': docs, 'counts': counts, 'knn': [h['_id'] for h in knn['hits']['hits']]}


def main():
    nine = int(req('GET', '/')['version']['number'].split('.')[0]) >= 9
    # Synthetic _source needs more than the basic licence: without it Elasticsearch stores it.
    req('POST', '/_license/start_trial?acknowledge=true', ok=(403,))
    req('DELETE', '/_snapshot/' + REPO, ok=(404,))
    req('PUT', '/_snapshot/' + REPO, {'type': 'fs', 'settings': {'location': LOCATION, 'chunk_size': '4kb'}})
    out = {}
    make('mixed', {'number_of_shards': 2, 'number_of_replicas': 0, 'refresh_interval': -1}, routing=True)
    delete_and_update('mixed')
    make('compressed', {'number_of_shards': 1, 'number_of_replicas': 0, 'refresh_interval': -1, 'codec': 'best_compression'}, segments=3)
    delete_and_update('compressed')
    m = json.loads(json.dumps(MAPPING))
    m['_source'] = {'excludes': ['secret']}
    make('excludes', {'number_of_shards': 1, 'number_of_replicas': 0}, mapping=m, segments=1)
    small = {'properties': {'tag': {'type': 'keyword'}, 'count': {'type': 'long'}}}
    make('synthetic', {'number_of_shards': 1, 'number_of_replicas': 0, 'index.mapping.source.mode': 'synthetic'}, mapping=small, segments=1)
    make('nosource', {'number_of_shards': 1, 'number_of_replicas': 0}, mapping={'_source': {'enabled': False}, **small}, segments=1)
    for index in ['mixed', 'compressed', 'excludes']:
        out[index] = expected(index, nine)
    if GLOBAL:
        req('PUT', '/_index_template/fixture-template', {'index_patterns': ['fixture-*'], 'template': {'settings': {'number_of_shards': 1}}})
        req('PUT', '/_ingest/pipeline/fixture-pipeline', {'processors': [{'set': {'field': 'x', 'value': 1}}]})
        req('PUT', '/_ilm/policy/fixture-policy', {'policy': {'phases': {'hot': {'actions': {'rollover': {'max_age': '1d'}}}}}})
    req('PUT', f'/_snapshot/{REPO}/snap-1?wait_for_completion=true', {'indices': 'mixed,compressed,excludes,synthetic,nosource', 'include_global_state': GLOBAL})
    with open(OUT, 'w') as f:
        json.dump(out, f, ensure_ascii=False, sort_keys=True, indent=0)
    print(json.dumps({k: v['count'] for k, v in out.items()}))


main()
