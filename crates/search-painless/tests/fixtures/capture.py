#!/usr/bin/env python3
"""Captures Elasticsearch's answers to Painless `_execute` requests as fixtures.

usage: capture.py <cases.txt> <out.json> [es_url]

A cases file holds scripts separated by lines of `----`; a case may start with header
lines `#params: {json}`, `#context: name`, `#setup: {context_setup json}`; a case holding only
`#defaults: {"context": …, "setup": …}` sets those headers for the cases after it. The output maps
each case to Elasticsearch's HTTP status and response body, which `tests/execute.rs`
compares against.
"""
import json, sys, time, urllib.request

def parse(path):
    cases, cur = [], []
    for line in open(path, encoding="utf-8").read().split("\n"):
        if line == "----":
            cases.append("\n".join(cur)); cur = []
        else:
            cur.append(line)
    if any(l.strip() for l in cur):
        cases.append("\n".join(cur))
    out = []
    defaults = {}
    for c in cases:
        lines = c.split("\n")
        hdr = {}
        while lines and lines[0].startswith("#") and ":" in lines[0] and lines[0].split(":")[0] in ("#params", "#context", "#setup", "#name", "#defaults"):
            k, v = lines[0][1:].split(":", 1)
            hdr[k] = v.strip()
            lines = lines[1:]
        src = "\n".join(lines).strip("\n")
        if "defaults" in hdr:
            d = json.loads(hdr.pop("defaults"))
            defaults = {k: (v if isinstance(v, str) else json.dumps(v)) for k, v in d.items()}
        if not src.strip():
            continue
        merged = dict(defaults)
        merged.update(hdr)
        out.append((merged, src))
    return out

def main():
    cases = parse(sys.argv[1])
    url = (sys.argv[3] if len(sys.argv) > 3 else "http://127.0.0.1:19200") + "/_scripts/painless/_execute"
    res = []
    for hdr, src in cases:
        script = {"source": src, "lang": "painless"}
        if "params" in hdr:
            script["params"] = json.loads(hdr["params"])
        body = {"script": script}
        if "context" in hdr:
            body["context"] = hdr["context"]
        if "setup" in hdr:
            body["context_setup"] = json.loads(hdr["setup"])
        while True:
            req = urllib.request.Request(url, data=json.dumps(body).encode(), headers={"content-type": "application/json"})
            try:
                with urllib.request.urlopen(req) as r:
                    status, resp = r.status, json.load(r)
            except urllib.error.HTTPError as e:
                status, resp = e.code, json.load(e)
            # the compilation rate limit (150/5m by default) is not part of what we capture
            if "Too many dynamic script compilations" in json.dumps(resp):
                time.sleep(10)
                continue
            break
        entry = {"request": body, "status": status, "response": resp}
        if "name" in hdr:
            entry["name"] = hdr["name"]
        res.append(entry)
    with open(sys.argv[2], "w", encoding="utf-8") as f:
        json.dump(res, f, indent=1, ensure_ascii=False)
        f.write("\n")

main()
