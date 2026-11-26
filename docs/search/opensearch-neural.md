# Semantic search on OpenSearch-flavor clusters: design

`## Next` item 14 in [README.md](README.md). Status: phase 1 built (see "Phase 1 as built" at the end).

## Why

An OpenSearch customer who wants semantic search follows AWS's and OpenSearch's guides. The steps
are:
1. a connector to a model provider;
2. a remote model registered on that connector and deployed;
3. an ingest pipeline with a `text_embedding` processor;
4. a `knn_vector` field;
5. `neural` queries, often inside `hybrid` with a `normalization-processor` search pipeline.

On a roll OpenSearch-flavor cluster, step 1 already fails. The ML Commons API
(`_plugins/_ml/…`) isn't served. `neural` and `neural_sparse` are only names in
`meta/aliases.rs`, and the neural-search processors aren't served either. So those customers
can't move their semantic search to roll.

roll already runs model inference for Elasticsearch-flavor clusters:
- `_inference` endpoints over OpenAI, Cohere, Anthropic, Azure OpenAI, Bedrock (SigV4), Vertex AI,
  AI Studio, Mistral, Voyage, Jina, DeepSeek, Hugging Face and `custom`;
- keys sealed to the cluster, upstreams reached through `roll_net::PublicOnly`;
- tokens metered;
- `semantic_text`, the semantic highlighter and `rrf`/`linear` on top of it;
- E5 running on roll's own CPUs once #563 lands.

The OpenSearch half already has `hybrid`, its phase-results processors and search pipelines
(`opensearch/hybrid.rs`, `opensearch/search_pipeline.rs`). What's missing is OpenSearch's
*vocabulary* for the same engine.

## The shape: OpenSearch's APIs over roll's inference engine

One model registry per cluster, two dialects:

- **An ML Commons remote model** on an OpenSearch-flavor cluster is stored as roll's inference
  endpoint. It has the same service code, sealed keys, upstream client, metering and limits.
- **An ML Commons connector** is the service configuration the endpoint is made from.

roll answers in OpenSearch 3.9's shapes (ids, `_search` results, task documents). Underneath,
everything is the one engine, so a fix or a limit in inference applies to both dialects.

Nothing changes on Elasticsearch-flavor clusters. `_plugins/_ml` exists only in the OpenSearch
dialect, as OpenSearch's other plugin APIs do.

### What's served (phase 1)

| OpenSearch 3.9 | roll |
|---|---|
| `POST /_plugins/_ml/connectors/_create`, `GET/PUT/DELETE …/connectors/{id}`, `POST …/connectors/_search` | a connector record: `name`, `version`, `protocol` (`http`, `aws_sigv4`), `parameters`, `credential`, `actions` |
| `POST /_plugins/_ml/model_groups/_register`, `…/model_groups/{id}`, `_search` | model groups with `access_mode` (`public`, `private`, `restricted`) over OpenSearch Security's backend roles, as roll's OpenSearch Security already models them |
| `POST /_plugins/_ml/models/_register` (`function_name: remote`, `connector_id` or an inline `connector`), `?deploy=true`, `…/models/{id}/_deploy`, `_undeploy`, `GET/DELETE …/models/{id}`, `_search` | an inference endpoint made from the connector. `_register` answers with a `task_id` that is already `COMPLETED`, and `GET /_plugins/_ml/tasks/{id}` answers it. Deploying is a state flag, because a remote model holds no node memory. |
| `POST /_plugins/_ml/models/{id}/_predict` | one inference call, answered in ML Commons' `inference_results` shape |
| ingest processor `text_embedding` (`model_id`, `field_map`) | runs the endpoint on the mapped fields, as `semantic_text`'s inference runs today |
| query `neural` (`query_text`, `model_id`, `k`, `min_score` or `max_distance`, `filter`) | embeds `query_text` with the model, then runs the `knn` query roll already serves on `knn_vector` |
| search request processor `neural_query_enricher` (`default_model_id`, `neural_field_default_id`) | fills a `neural` query's missing `model_id` |

### Connectors become services

An ML Commons connector is a URL template, headers, a request body template, and pre- and
post-process functions. roll recognises the published blueprints by their action's URL host and
body shape, and maps each onto the matching roll service with its settings:
- OpenAI embeddings;
- Amazon Bedrock Titan and Cohere embed;
- Cohere embed;
- Azure OpenAI;
- Vertex AI;
- SageMaker isn't a roll service; see "Not in phase 1".

A connector that matches no blueprint becomes roll's `custom` service: its `url`, `headers` and
`request_body` with `${parameters.…}` substitutions become the custom service's request template.
Its `post_process_function` must be one of ML Commons' built-in names
(`connector.post_process.openai.embedding`, `connector.post_process.cohere.embedding`,
`connector.post_process.default.embedding`, and so on). A Painless pre- or post-process script
fails validation in phase 1 with a clear error. Running tenant Painless there needs the
per-execution memory limits from #591 first.

### Credentials and egress

These are the same rules as `_inference`. None of them is new code.
- **Sealing:** `credential` values are sealed when the connector is written, through
  `seal::seal_given_with` (the one path, #601), with the cluster and connector ids as associated
  data. A node without `ROLL_SECRET_KEY` refuses them.
- **Answers:** `GET` and `_search` never return them, which is also OpenSearch's answer: it
  returns no `credential`.
- **Reuse:** a sealed value copied from another cluster doesn't open, and a branch must give its
  connectors' credentials again before they run, as with Elasticsearch-flavor inference keys.
- **Upstream calls:** every call goes through roll's egress check (`fix/search-egress-fairness`:
  public addresses only, no redirects, the node's deny lists) with the inference timeouts, rate
  limits and response caps.
- **SigV4:** `aws_sigv4` uses the Bedrock service's signing. roll's own AWS credentials are never
  used for a tenant's connector.

### Local models

OpenSearch can register pretrained models it runs itself
(`huggingface/sentence-transformers/all-MiniLM-L6-v2`, `amazon/neural-sparse/…`). roll runs one
model family on its own CPUs: E5 (#563).
- **Supported:** registering a local E5 name maps to that built-in model.
- **Refused:** any other pretrained name fails with an error that lists what roll runs. Different
  models give different vectors, so silently substituting one would corrupt every index built
  with it.
- **Uploads:** custom TorchScript and ONNX uploads (`model_format`, `model_content_hash_value`) are
  refused. roll doesn't run tenant model files.

### Access, branches and metering

- **Permissions:** OpenSearch Security's ML permissions (`cluster:admin/opensearch/ml/*`) and model
  groups' access modes are checked as OpenSearch checks them. `ml_full_access` and
  `ml_readonly_access` are roll's built-in roles for them.
- **Browser keys** are refused every `_plugins/_ml` API, as they are `_inference`.
- **Branches:** a branch inherits connectors and models without their credentials. Its models fail
  with "credentials must be given again" until a writer re-puts the connector, the same adoption
  rule as connectors that sync.
- **Metering:** tokens are metered per model, as `_inference` endpoints are. The dashboard's
  Inference tab lists an OpenSearch-flavor cluster's models with the same columns.

## Not in phase 1

1. **Sparse:**
   - `neural_sparse` and the `sparse_encoding` processor, which need a sparse model. roll has none
     locally; a remote one through a connector is phase 2.
   - `neural_sparse_two_phase_processor`.
2. **Chunking and multimodal:** `text_chunking`, `text_image_embedding`, and the `semantic` field
   type (OpenSearch 3.1+, the counterpart of `semantic_text`).
3. **ML Commons' generic processors:** `ml_inference` (ingest, search request, search response).
   Today roll only parses the request `ext` sections these plugins register (`rerank`,
   `ml_inference`, `ubi`; `opensearch/mod.rs` `parse_ext`).
4. **Rerank:** the `rerank` response processor, both its `ml_opensearch` cross-encoder form and
   its `by_field` form, neither of which is served yet.
5. **Agents and generative search:** agents, memory, `agentic` search and
   `retrieval_augmented_generation` belong with RAG, item 11.
6. **Scripts:** Painless pre- and post-process functions (after #591).
7. **SageMaker** connectors: no roll service yet.

## Tests

- **Shapes:** a private `opensearchproject/opensearch:3.9.0` container is the oracle for
  ML Commons' answers. roll's answers are diffed against it for create, get, search, register,
  deploy, predict and the task documents. A mock provider records request bodies, and the bodies
  roll sends must match OpenSearch's byte for byte for each blueprint.
- **Where tests go:** `search-es/tests/` (REST, against the recordings) and
  `search-control/tests/control.rs` (credentials sealed, refused on a keyless node, never in an
  answer, a branch can't use the parent's).
- **One end-to-end flow:** connector, then model, then ingest pipeline, then index, then `neural`
  inside `hybrid`, through the guides' commands.

## Phases

1. **Phase 1:** connectors, remote models, predict, `text_embedding`, `neural` and
   `neural_query_enricher` (the table above), plus a guide section for OpenSearch customers.
2. **Phase 2:** `neural_sparse`, `sparse_encoding`, `text_chunking`, the `semantic` field, and
   `ml_inference` processors.
3. **Phase 3:** agents and agentic search, with RAG (item 11).

## Phase 1 as built

Code: `crates/search-es/src/opensearch/ml/` (`mod.rs` the APIs, `connector.rs` connectors,
`ingest.rs` the processor) and `opensearch/neural.rs`. Test:
`search-control/tests/control.rs::opensearch_semantic_search_follows_the_guide`.

### Where it departs from the design

Each is the stricter choice, or what the code shows the design assumed wrongly.

- **Connectors run as ML Commons runs them, not as a roll service.** roll renders the connector's
  URL, headers and body template itself, with ML Commons' substitution rules: a string parameter is
  escaped as commons-text's `escapeJson` writes it, and lists are written as Gson writes them. So
  the provider gets OpenSearch's body byte for byte (checked against OpenSearch 3.9.0), which
  mapping onto roll's OpenAI or Cohere service wouldn't give. The call still goes through the
  inference client (`xpack/inference/http.rs`), with its egress check, timeouts, retries and
  response cap. SigV4 is signed with the connector's own keys.
- **A model isn't an `_inference` endpoint row.** An OpenSearch cluster has no `.inference`
  index. Connectors, model groups, models and tasks are records homed at
  `.plugins-ml-connector`, `-model-group`, `-model` and `-task`. A connector's credential is
  sealed as an inference key is, bound to the cluster (`inference::SECRET_FAMILIES`), so
  branches and keyless nodes treat it the same way.
- **Credentials reach headers and the URL only.** Only what the connector stores fills the URL
  and headers. A `_predict` call's `parameters` reach the body only, so a caller can't send the
  credential to a host it picks. OpenSearch fills the URL from predict-time parameters too.
- **Functions:** only the built-in embedding pre- and post-process functions run (`openai`,
  `cohere`, `default` and `bedrock` `.embedding`). Any other name, and any Painless, is refused
  when the connector is written. OpenSearch accepts any name and fails later.
- **Methods:** only `POST` predict actions (the inference client posts).
- **Access control:**
  - Model groups' `access_mode` and `backend_roles` apply only when
    `plugins.ml_commons.model_access_control_enabled` is set. Otherwise they're refused with
    OpenSearch's message.
  - With access control on, a group registered without a mode is `private`.
  - Connector access control isn't served: its fields are refused as OpenSearch refuses them when
    it's off.
  - OpenSearch's read-only role is `ml_read_access`, not `ml_readonly_access`. Both roles come
    from the security plugin's captured configuration.
- **Tasks** are `COMPLETED` when they're answered. `_register?deploy=true` also records the
  deploy task.
- **`text_embedding`:**
  - A processor reads its texts where it runs. The vectors are written when its pipeline ends,
    before the next pipeline or the write. So a later processor in the same pipeline doesn't see
    them, and a failed call fails the document (`ignore_failure` and `on_failure` don't catch it).
  - Nested `field_map` objects are refused; use dotted names.
  - `skip_existing` is accepted, but every write embeds.
  - `_simulate` doesn't call the model.
- **`neural`:**
  - It's embedded on the coordinator before the search runs, wherever it is in the search's query
    (`hybrid` included).
  - In an aggregation's filter or a rescore query it's refused.
  - `query_image` is refused (multimodal is phase 2).
- **Local models:** E5 is pre-registered as `.multilingual-e5-small` and registers under
  `intfloat/multilingual-e5-small`. A node without the embedder answers as Elasticsearch-flavor
  E5 does without one.
- **Not here yet:**
  - the dashboard's Inference tab doesn't list OpenSearch models;
  - remote calls aren't metered per model. `_inference` doesn't meter provider tokens either; the
    provider's key is the tenant's.

### For OpenSearch customers

OpenSearch's semantic search tutorial runs unchanged:

1. `POST /_plugins/_ml/connectors/_create` with the provider's blueprint. Every published
   embedding blueprint that uses the built-in functions works: OpenAI, Cohere, and Amazon Bedrock
   Titan over `aws_sigv4`. A blueprint with a Painless function isn't run yet. The `credential`
   is sealed and never shown again.
2. `POST /_plugins/_ml/model_groups/_register`, then
   `POST /_plugins/_ml/models/_register?deploy=true` with `"function_name": "remote"` and the
   `connector_id`.
3. `PUT /_ingest/pipeline/<name>` with `text_embedding` (`model_id`, `field_map`), and an index
   with `"index.knn": true`, `default_pipeline` and a `knn_vector` field of the model's dimension.
4. Search with `neural` (`query_text`, `model_id`, `k`). For hybrid search, use a search pipeline
   with a `normalization-processor`, and `neural_query_enricher` to name the model once.

Without a provider, register nothing: `.multilingual-e5-small` embeds on roll's own CPUs.

