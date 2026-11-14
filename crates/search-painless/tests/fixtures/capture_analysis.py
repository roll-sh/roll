#!/usr/bin/env python3
"""Captures Elasticsearch's answers to analysis predicate scripts as fixtures.

usage: capture_analysis.py <cases.txt> <out.json> [es_url]

Cases are separated by lines of `----`; header lines `#text: "json string"`, `#tokenizer: name`
and `#filters: [json]` (token filters before the script) set the analyzed text; a case holding
only headers sets defaults. Every case runs `_analyze` twice: without the script (the tokens the
script sees) and with a `predicate_token_filter` holding it (the tokens it keeps, or the error).
"""
import json, sys, urllib.request

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
    for c in raw:
        lines = c.split("\n")
        hdr = {}
        while lines and lines[0].startswith("#") and lines[0].split(":")[0] in ("#text", "#tokenizer", "#filters"):
            k, v = lines[0][1:].split(":", 1)
            hdr[k] = json.loads(v.strip()) if k != "tokenizer" else v.strip()
            lines = lines[1:]
        src = "\n".join(lines).strip("\n")
        if not src.strip():
            defaults.update(hdr)
            continue
        merged = dict(defaults); merged.update(hdr)
        out.append((merged, src))
    return out

def call(base, body):
    req = urllib.request.Request(base + "/_analyze", data=json.dumps(body).encode(), method="POST", headers={"content-type": "application/json"})
    try:
        with urllib.request.urlopen(req) as r:
            return r.status, json.load(r)
    except urllib.error.HTTPError as e:
        return e.code, json.load(e)

def main():
    cases_path, out_path = sys.argv[1], sys.argv[2]
    base = sys.argv[3] if len(sys.argv) > 3 else "http://127.0.0.1:19200"
    res = []
    for hdr, src in parse(cases_path):
        body = {"tokenizer": hdr.get("tokenizer", "standard"), "filter": hdr.get("filters", []), "text": hdr.get("text", "")}
        _, plain = call(base, body)
        # the keyword flag only shows in the explained output (of the last stage)
        _, explained = call(base, dict(body, explain=True, attributes=["keyword"]))
        detail = explained.get("detail", {})
        stages = [detail.get("tokenizer", {})] + detail.get("tokenfilters", [])
        last = stages[-1].get("tokens", []) if stages else []
        for t, x in zip(plain.get("tokens", []), last):
            t["keyword"] = bool(x.get("keyword", False))
        scripted = dict(body)
        scripted["filter"] = body["filter"] + [{"type": "predicate_token_filter", "script": {"source": src}}]
        status, resp = call(base, scripted)
        res.append({"request": {"script": {"source": src}, "analyze": body}, "tokens": plain.get("tokens"), "status": status, "response": resp})
    with open(out_path, "w", encoding="utf-8") as f:
        json.dump(res, f, indent=1, ensure_ascii=False)
        f.write("\n")

main()
