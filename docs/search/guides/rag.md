# Retrieval for RAG

Semantic and hybrid search over your own documents, then an LLM answer grounded in what was
found. roll embeds text for you through an inference endpoint, so your code sends and searches
plain text. About fifteen minutes.

## What works today

- `semantic_text` fields, the `semantic` query, and `rrf` and `linear` retrievers work as on
  Elasticsearch 9.5.5.
- Embeddings come from a provider you configure: OpenAI or any OpenAI-compatible server, Cohere,
  Mistral, Voyage AI, Jina AI, Google, Azure OpenAI, Amazon Bedrock, Hugging Face. The full list
  is compatibility.md's "Inference". You need that provider's API key.
- **Semantic search with no key is coming with #563** (in review, not on main): roll will run
  `multilingual-e5-small` itself as Elasticsearch's preconfigured
  `.multilingual-e5-small-elasticsearch` endpoint. Until it lands, that endpoint (and
  `.elser-2-elasticsearch`, the default `inference_id` of a `semantic_text` field) answers as a
  cluster without ML nodes does, so name your own endpoint in every `semantic_text` field.
- **roll has no chat API yet.** The last step below calls a completion endpoint from your own
  script. Chat over hits with citations, and an API for it, are `## Next` item 11 in
  [../README.md](../README.md).

## Before you start

You need a cluster, its URL and a key, as in [the first query](README.md#the-first-query-in-a-minute):

```sh
roll search clusters create kb
roll search keys create kb app --expires-in 30d
export ELASTICSEARCH_URL=…        # printed by the commands above
export ELASTICSEARCH_API_KEY=…
```

The requests use the `es` helper from [the guides' README](README.md#a-shorter-way-to-send-requests).
A `data` key can index, search and run inference; creating endpoints is done with `roll search
inference create`, which uses your roll login.

## Create an embedding endpoint

The best place for the provider's key is a roll secret, so rotating it updates the endpoint.
Put the cluster on a project's canvas in the dashboard, store the key in that project (you need
reveal on it), then:

```sh
roll secrets -p kb-project -e production set OPENAI_API_KEY      # paste the key on stdin
roll search inference create kb emb --model text-embedding-3-small --secret OPENAI_API_KEY
```

`--secret` takes a name in `--environment` (`production` by default) or a reference such as
`roll://production/OPENAI_API_KEY`. Without roll secrets, the key comes from the environment and
is sealed in the cluster:

```sh
OPENAI_API_KEY=… roll search inference create kb emb --model text-embedding-3-small
roll search inference test kb emb "how do I reset my password?"
```

Other providers: `--service cohere --model embed-v4.0`, or `--url` for an OpenAI-compatible server
(vLLM, Ollama, LiteLLM) at a public address. The dashboard's **Inference** tab creates, tests and
deletes endpoints too. Elasticsearch's own `PUT /_inference/text_embedding/emb` works with an admin
key, the provider key in `service_settings.api_key`.

## Map a semantic field

```sh
es PUT /kb '{
  "mappings": {
    "properties": {
      "title":   {"type": "text"},
      "url":     {"type": "keyword"},
      "content": {"type": "text", "copy_to": "content_semantic"},
      "content_semantic": {"type": "semantic_text", "inference_id": "emb"}
    }
  }
}'
```

`content` is for keyword search. `copy_to` sends the same text to `content_semantic`, which roll
splits into chunks and embeds with `emb` when the document is indexed. You never handle vectors.

## Index documents

```sh
es PUT '/kb/_doc/reset-password?refresh=true' '{
  "title": "Reset your password",
  "url": "https://example.com/help/reset-password",
  "content": "If you forgot your password, open the sign-in page and choose Forgot password. We email you a link that works for 30 minutes."
}'
```

Use `_bulk` for many documents: roll embeds all the texts of one bulk request together, in as few
provider calls as it can. A document whose embedding fails is reported in the bulk answer with an
`inference_exception`; the rest are indexed. Updating a document re-embeds only fields whose text
changed.

## Search by meaning

```sh
es POST /kb/_search '{
  "_source": ["title", "url"],
  "query": {"semantic": {"field": "content_semantic", "query": "I can not log in anymore"}},
  "highlight": {"fields": {"content_semantic": {"type": "semantic", "number_of_fragments": 2}}}
}'
```

The query shares no words with the document and still finds it. The highlight returns the best
matching chunks, which is what you pass to an LLM.

## Mix keyword and semantic search

Semantic search misses exact terms (error codes, product names); keyword search misses
paraphrases. Reciprocal rank fusion (`rrf`) combines both rankings:

```sh
es POST /kb/_search '{
  "_source": ["title", "url"],
  "retriever": {
    "rrf": {
      "retrievers": [
        {"standard": {"query": {"match": {"content": "error E1042 on sign in"}}}},
        {"standard": {"query": {"semantic": {"field": "content_semantic", "query": "error E1042 on sign in"}}}}
      ],
      "rank_window_size": 50
    }
  }
}'
```

To weight the two parts instead, use the `linear` retriever. The shorthand
`{"rrf": {"query": "…", "fields": ["content", "content_semantic"]}}` expands to the same two
retrievers. With a rerank endpoint (`--task rerank`, for example Cohere's), wrap the retriever in a
`text_similarity_reranker` to reorder the top hits.

The dashboard's **Playground** tab compares keyword, semantic and hybrid versions of a query side
by side, explains each hit's score and copies a request as curl, Python or JavaScript.

## Create a completion endpoint

The same command makes an endpoint for a model that writes the answer. Anthropic's needs
`--max-tokens`:

```sh
roll secrets -p kb-project -e production set ANTHROPIC_API_KEY
roll search inference create kb llm --service anthropic --task completion \
  --model claude-sonnet-5-5 --max-tokens 1024 --secret ANTHROPIC_API_KEY
roll search inference test kb llm "Say hello in five words."
```

`--service openai --task completion` (or `chat_completion`) works the same way.

## Retrieve, then generate

This script searches `kb`, puts the best chunks in a prompt and asks `llm` for an answer with
citations. It needs `pip install elasticsearch`.

```python
# ask.py: python ask.py "how do I reset my password?"
import os, sys
from elasticsearch import Elasticsearch

es = Elasticsearch(os.environ["ELASTICSEARCH_URL"], api_key=os.environ["ELASTICSEARCH_API_KEY"])
question = sys.argv[1]

hits = es.search(
    index="kb",
    size=4,
    source=["title", "url"],
    retriever={"rrf": {"retrievers": [
        {"standard": {"query": {"match": {"content": question}}}},
        {"standard": {"query": {"semantic": {"field": "content_semantic", "query": question}}}},
    ]}},
    highlight={"fields": {"content_semantic": {"type": "semantic", "number_of_fragments": 2}}},
)["hits"]["hits"]

sources = "\n\n".join(
    f"[{i}] {h['_source']['title']}\n" + "\n".join(h.get("highlight", {}).get("content_semantic", []))
    for i, h in enumerate(hits, 1)
)
prompt = (
    "Answer the question using only the sources below and cite them as [n]. "
    "If the sources don't answer it, say you don't know.\n\n"
    f"Sources:\n{sources}\n\nQuestion: {question}"
)

answer = es.inference.completion(inference_id="llm", input=prompt)
print(answer["completion"][0]["result"])
for i, h in enumerate(hits, 1):
    print(f"[{i}] {h['_source']['url']}")
```

The model call goes out from roll with the key sealed in the cluster, so the script only holds
the cluster's key. Each call waits at most two minutes. For streaming, call
`POST /_inference/completion/llm/_stream` (or `_inference/chat_completion/llm/_stream` for a
`chat_completion` endpoint): the answer comes as server-sent events while the model writes it.

Documents can hold text that reads like instructions. The Playground's Chat tab (in the
dashboard) does this flow with the documents fenced as quoted data: a random boundary per
request, `<`, `>` and the boundary escaped in their text, and a system message that says never
to follow them. Its "View code" gives the same flow in curl, Python or JavaScript, and that code
runs on Elasticsearch too. Render the answer as text: a link or image the model writes could
send another document's text to someone else's server.

## Good to know

- Embedding happens at index and query time through your provider, and its usage is billed by
  them. roll stores the vectors with the document, so branches, snapshots and reindexing don't
  embed again.
- `semantic_text` follows field level security and document level security like any field, so a
  tenant-filtered key ([saas-tenants.md](saas-tenants.md)) only retrieves its tenant's chunks.
- OpenSearch-flavor clusters don't serve OpenSearch's neural search yet (ML Commons connectors,
  the `neural` query): `## Next` item 14.
