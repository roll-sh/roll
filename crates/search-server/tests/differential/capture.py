"""Captures a hand-written differential fixture from a real Elasticsearch.

    python3 -I capture.py <es-url> <steps.json> <out.json> [--known REASON]

`steps.json` is a list of steps in the fixture format (`method`, `path`, then `body` for JSON
or `lines` for NDJSON, optionally `"compare": false`). Every step is sent to Elasticsearch in
order and its answer (status, the headers the suite compares, body) is stored as the step's
`es`, which `replay_cases` in differential.rs compares roll's answer with. Index names should
carry a prefix of their own; the fixture deletes nothing, so end the steps with a cleanup.
Only the standard library is used.
"""

import json
import sys
import urllib.error
import urllib.request

HEADERS = ("content-type", "x-elastic-product", "warning")


def send(base, step):
    data = None
    ctype = None
    if "body" in step:
        # Compact with sorted keys, as the suite sends bodies (serde_json maps): error
        # locations count its columns.
        data = json.dumps(step["body"], separators=(",", ":"), ensure_ascii=False, sort_keys=True).encode()
        ctype = "application/json"
    elif "lines" in step:
        data = "".join(json.dumps(line, separators=(",", ":"), ensure_ascii=False, sort_keys=True) + "\n" for line in step["lines"]).encode()
        ctype = "application/x-ndjson"
    path = step["path"] if step["path"].startswith("/") else "/" + step["path"]
    req = urllib.request.Request(base.rstrip("/") + path, data=data, method=step["method"])
    if ctype:
        req.add_header("content-type", ctype)
    try:
        resp = urllib.request.urlopen(req, timeout=120)
    except urllib.error.HTTPError as e:
        resp = e
    raw = resp.read()
    headers = {}
    for k in HEADERS:
        vals = resp.headers.get_all(k) or []
        if vals:
            headers[k] = ", ".join(vals)
    if step["method"] == "HEAD":
        body = None
    else:
        try:
            body = json.loads(raw)
        except ValueError:
            body = raw.decode("utf-8", "replace")
    return {"status": resp.status, "headers": headers, "body": body}


def main():
    args = sys.argv[1:]
    known = None
    if "--known" in args:
        i = args.index("--known")
        known = args[i + 1]
        del args[i : i + 2]
    base, steps_path, out = args
    with open(steps_path) as f:
        steps = json.load(f)
    for s in steps:
        s["es"] = send(base, s)
    fx = {"signature": "hand-written", "steps": steps}
    if known:
        fx["known"] = known
    with open(out, "w") as f:
        json.dump(fx, f, indent=2)
        f.write("\n")


if __name__ == "__main__":
    main()
