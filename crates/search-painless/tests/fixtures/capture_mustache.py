#!/usr/bin/env python3
"""Captures Elasticsearch's `_render/template` answers for Mustache templates, as fixtures.

usage: capture_mustache.py <cases.json> <out.json> [es_url]

cases.json is a list of `{"v": <template fragment producing a JSON value>, "params": {...}}`
(or `{"source": <whole template>, "params": ...}`). A fragment is wrapped so the rendered text
is a valid search body: `{"runtime_mappings":{"r":{"type":"keyword","script":{"source":"x",
"params":{"v":FRAGMENT}}}}}`; the fixture keeps the full template source.
"""
import json, sys, time, urllib.request

PRE = '{"runtime_mappings":{"r":{"type":"keyword","script":{"source":"x","params":{"v":'
POST = '}}}}}'

def main():
    cases = json.load(open(sys.argv[1], encoding="utf-8"))
    base = sys.argv[3] if len(sys.argv) > 3 else "http://127.0.0.1:19200"
    out = []
    for c in cases:
        src = c.get("source") or PRE + c["v"] + POST
        body = {"source": src, "params": c.get("params", {})}
        if "options" in c:
            body["options"] = c["options"]
        req = urllib.request.Request(base + "/_render/template", data=json.dumps(body).encode(), headers={"content-type": "application/json"})
        try:
            with urllib.request.urlopen(req) as r:
                status, resp = r.status, json.load(r)
        except urllib.error.HTTPError as e:
            status, resp = e.code, json.load(e)
        out.append({"request": body, "status": status, "response": resp})
    with open(sys.argv[2], "w", encoding="utf-8") as f:
        json.dump(out, f, indent=1, ensure_ascii=False)
        f.write("\n")

main()
