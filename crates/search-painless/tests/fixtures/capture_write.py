#!/usr/bin/env python3
"""Captures Elasticsearch's answers to update and ingest scripts as fixtures.

usage: capture_write.py update|ubq|reindex|ingest|ingest-if <cases.txt> <out.json> [es_url]

Cases are separated by lines of `----`; header lines set `#doc: {json}` (the stored document,
or the ingest document's `_source`), `#params: {json}`, `#upsert: {json}` (update only: a
scripted upsert of a missing document) and `#meta: {json}` (ingest only: `_index`, `_id`,
`_routing` of the simulated document). A case holding only `#defaults: {...}` sets defaults.

update: every case updates its own fresh document `c<N>` in index `painless-write` and records
the update response and the document afterwards. ingest: every case runs a one-processor
pipeline through `_ingest/pipeline/_simulate?verbose=false` and records the response.
ingest-if: the script is the `if` condition of a `set` processor that sets `_cond: true`.
"""
import json, sys, time, urllib.request

INDEX = "painless-write"

def parse(path):
    raw, cur = [], []
    for line in open(path, encoding="utf-8").read().split("\n"):
        if line == "----":
            raw.append("\n".join(cur)); cur = []
        else:
            cur.append(line)
    if any(l.strip() for l in cur):
        raw.append("\n".join(cur))
    out, defaults = [], {}
    keys = ("#doc", "#params", "#upsert", "#meta", "#defaults", "#name")
    for c in raw:
        lines = c.split("\n")
        hdr = {}
        while lines and lines[0].startswith("#") and lines[0].split(":")[0] in keys:
            k, v = lines[0][1:].split(":", 1)
            hdr[k] = json.loads(v.strip()) if k != "name" else v.strip()
            lines = lines[1:]
        src = "\n".join(lines).strip("\n")
        if "defaults" in hdr:
            defaults = hdr.pop("defaults")
        if not src.strip():
            continue
        merged = dict(defaults); merged.update(hdr)
        out.append((merged, src))
    return out

def call(base, method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    while True:
        req = urllib.request.Request(base + path, data=data, method=method, headers={"content-type": "application/json"})
        try:
            with urllib.request.urlopen(req) as r:
                status, resp = r.status, json.load(r)
        except urllib.error.HTTPError as e:
            status, resp = e.code, json.load(e)
        if "Too many dynamic script compilations" in json.dumps(resp):
            time.sleep(10)
            continue
        return status, resp

def main():
    mode, cases_path, out_path = sys.argv[1], sys.argv[2], sys.argv[3]
    base = sys.argv[4] if len(sys.argv) > 4 else "http://127.0.0.1:19200"
    res = []
    if mode in ("update", "ubq", "reindex"):
        call(base, "DELETE", "/" + INDEX)
        call(base, "PUT", "/" + INDEX, {"settings": {"number_of_replicas": 0}, "mappings": {"dynamic": False}})
    for n, (hdr, src) in enumerate(parse(cases_path)):
        script = {"source": src, "lang": "painless"}
        if "params" in hdr:
            script["params"] = hdr["params"]
        entry = {"request": {"script": script}}
        if mode == "update":
            doc_id = "c%d" % n
            entry["request"]["id"] = doc_id
            body = {"script": script}
            if "upsert" in hdr:
                body["scripted_upsert"] = True
                body["upsert"] = hdr["upsert"]
                entry["request"]["upsert"] = hdr["upsert"]
                entry["request"]["upsert_raw"] = json.dumps(hdr["upsert"], ensure_ascii=False)
            else:
                doc = hdr.get("doc", {})
                entry["request"]["doc"] = doc
                entry["request"]["doc_raw"] = json.dumps(doc, ensure_ascii=False)
                call(base, "PUT", "/%s/_doc/%s" % (INDEX, doc_id), doc)
            status, resp = call(base, "POST", "/%s/_update/%s" % (INDEX, doc_id), body)
            _, after = call(base, "GET", "/%s/_doc/%s" % (INDEX, doc_id))
            entry["after"] = after.get("_source") if after.get("found") else None
            if after.get("found"):
                # the stored bytes, as the script's map was serialized
                with urllib.request.urlopen(base + "/%s/_source/%s" % (INDEX, doc_id)) as r:
                    entry["after_raw"] = r.read().decode("utf-8")
        elif mode in ("ubq", "reindex"):
            doc_id = "c%d" % n
            doc = hdr.get("doc", {})
            entry["request"]["id"] = doc_id
            entry["request"]["doc"] = doc
            entry["request"]["doc_raw"] = json.dumps(doc, ensure_ascii=False)
            call(base, "PUT", "/%s/_doc/%s?refresh=true" % (INDEX, doc_id), doc)
            query = {"ids": {"values": [doc_id]}}
            if mode == "ubq":
                status, resp = call(base, "POST", "/%s/_update_by_query?refresh=true" % INDEX, {"query": query, "script": script})
                with urllib.request.urlopen(base + "/%s/_search?q=_id:%s" % (INDEX, doc_id)) as r:
                    hits = json.load(r)["hits"]["hits"]
            else:
                dest = INDEX + "-dest"
                call(base, "DELETE", "/" + dest)
                call(base, "PUT", "/" + dest, {"settings": {"number_of_replicas": 0}, "mappings": {"dynamic": False}})
                status, resp = call(base, "POST", "/_reindex?refresh=true", {"source": {"index": INDEX, "query": query}, "dest": {"index": dest}, "script": script})
                with urllib.request.urlopen(base + "/%s/_search" % dest) as r:
                    hits = json.load(r)["hits"]["hits"]
            entry["after"] = [{k: h.get(k) for k in ("_index", "_id", "_routing", "_source")} for h in hits]
            for k in ("took",):
                resp.pop(k, None)
        else:
            doc = {"_source": hdr.get("doc", {})}
            doc.update(hdr.get("meta", {}))
            entry["request"]["doc"] = doc
            entry["request"]["doc_raw"] = json.dumps(doc, ensure_ascii=False)
            entry["request"]["source_raw"] = json.dumps(hdr.get("doc", {}), ensure_ascii=False)
            if mode == "ingest-if":
                # the script is the condition of a processor that marks the document
                processor = {"set": {"if": script, "field": "_cond", "value": True}}
            else:
                processor = {"script": script}
            body = {"pipeline": {"processors": [processor]}, "docs": [doc]}
            status, resp = call(base, "POST", "/_ingest/pipeline/_simulate", body)
        if "name" in hdr:
            entry["name"] = hdr["name"]
        entry["status"] = status
        entry["response"] = resp
        res.append(entry)
    if mode in ("update", "ubq", "reindex"):
        call(base, "DELETE", "/" + INDEX)
        call(base, "DELETE", "/" + INDEX + "-dest")
    with open(out_path, "w", encoding="utf-8") as f:
        json.dump(res, f, indent=1, ensure_ascii=False)
        f.write("\n")

main()
