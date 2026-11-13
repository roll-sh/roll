# Elasticsearch fixtures

Facts about Elasticsearch 9.5.5 behaviour captured from `docker.elastic.co/elasticsearch/elasticsearch:9.5.5`
(single node, security off). They are expected outputs, not Elastic code.

- `es-9.5.5-automata.json`: a `keyword` field `t` holding `terms`; for each `fuzzy` query (value,
  fuzziness, prefix_length, transpositions, `max_expansions: 1000`) and `regexp` query (default
  flags `ALL`) the matching terms, or the `caused_by` exception type and reason for invalid
  patterns.
- `es-9.5.5-geo.json`: `geo_point` values and the coordinates `docvalue_fields` returns for them
  (Lucene's 32-bit quantization, decoded).
- `es-9.5.5-half-float.json`: `half_float` values and the values `docvalue_fields` returns.
- `es-9.5.5-norms.json`: field lengths and the `dl` (decoded norm) BM25 explain reports for them.
- `es-9.5.5-wildcard.json`: `wildcard` queries (with `case_insensitive`) over a `keyword` field and their matches.
- `es-9.5.5-vectors.json`: `dense_vector` fields with `index_options: {"type": "flat"}` (exact
  scores), two docs each; `knn` queries (`k: 2`, `num_candidates: 10`) and the `_score` per doc.

- `es-9.5.5-automata-random.json`: pseudo-random keyword terms (regexp/wildcard metacharacters,
  multi-byte and case-folding characters such as `ß`/`ẞ`, `σ`/`ς`/`Σ`, KELVIN SIGN) and
  pseudo-random `fuzzy`, `regexp` (also `case_insensitive`, stacked quantifiers, patterns around
  the 10000 determinization work limit), `wildcard` and `prefix` queries with the indices of the
  terms Elasticsearch matched, or the exception under the shard failure. Regenerate with
  `python3 -I gen_automata_random.py http://127.0.0.1:19200 > es-9.5.5-automata-random.json`
  (it creates and deletes the index `codec-automata-random`).

The other files were captured by indexing the terms/vectors and replaying the queries (that
generator lives outside the repo; see the commit that added these files).
