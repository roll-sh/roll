#!/usr/bin/env python3
"""Capture golden responses from a reference Alternator (ScyllaDB 2026.3.3).

Usage: capture.py [--url http://127.0.0.1:18000/] FILE...

A golden file is a script of requests. Each request is one line:

    > Op BODY          BODY is sent verbatim (it need not be valid JSON)
    > - BODY           no X-Amz-Target header
    ! sleep SECONDS    wait (ignored by the Rust replay)
    # comment

The capture rewrites FILE, putting the reference's answer after each request:

    < STATUS JSON

Volatile fields (timestamps, ids, ARNs, shard iterators) are replaced by "*", which the replay
in tests/golden.rs accepts as any value. A request may use `$Name` to substitute the last value
of field `Name` seen in an earlier response (for example `$LatestStreamArn`), and
`$repeat(TEXT,COUNT)` expands to TEXT repeated COUNT times (TEXT holds no `,` or `)`), for
large payloads. Before and after
the run, every table named in the file is deleted on the reference.
"""
import json, re, sys, time, urllib.request, urllib.error

VOLATILE = {
    "CreationDateTime", "LastUpdateToPayPerRequestDateTime", "LastIncreaseDateTime",
    "LastDecreaseDateTime", "TableId", "LatestStreamArn", "LatestStreamLabel", "StreamArn",
    "StreamLabel", "ShardIterator", "NextShardIterator", "SequenceNumber",
    "StartingSequenceNumber", "EndingSequenceNumber", "ApproximateCreationDateTime", "eventID",
    "ShardId", "ParentShardId", "LastEvaluatedShardId", "LastEvaluatedStreamArn",
    "CreationRequestDateTime", "TableSizeBytes", "IndexSizeBytes", "ItemCount",
}

def call(url, op, body):
    h = {"Content-Type": "application/x-amz-json-1.0"}
    if op != "-":
        h["X-Amz-Target"] = "DynamoDB_20120810." + op
    req = urllib.request.Request(url, data=body.encode(), headers=h, method="POST")
    try:
        r = urllib.request.urlopen(req, timeout=60)
        return r.status, r.read().decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace")

def harvest(v, seen):
    if isinstance(v, dict):
        for k, x in v.items():
            if k in VOLATILE and isinstance(x, (str, int, float)):
                seen[k] = x
            harvest(x, seen)
    elif isinstance(v, list):
        for x in v:
            harvest(x, seen)

def normalize(v):
    if isinstance(v, dict):
        return {k: ("*" if k in VOLATILE and not isinstance(x, (dict, list)) else normalize(x))
                for k, x in v.items()}
    if isinstance(v, list):
        return [normalize(x) for x in v]
    return v

def substitute(body, seen):
    body = re.sub(r"\$repeat\(([^,)]*),(\d+)\)", lambda m: m.group(1) * int(m.group(2)), body)
    def rep(m):
        k = m.group(1)
        if k not in seen:
            return m.group(0)
        x = seen[k]
        return x if isinstance(x, str) else json.dumps(x)
    return re.sub(r"\$([A-Za-z]+)", rep, body)

def tables(lines):
    names = set()
    for l in lines:
        if l.startswith("> "):
            names.update(re.findall(r'"TableName"\s*:\s*"([^"]+)"', l))
    return sorted(n for n in names if n.startswith("alt_"))

def drop(url, names):
    for n in names:
        call(url, "DeleteTable", json.dumps({"TableName": n}))

def run(url, path):
    lines = [l.rstrip("\n") for l in open(path, encoding="utf-8")]
    lines = [l for l in lines if not l.startswith("< ")]
    names = tables(lines)
    drop(url, names)
    out, seen = [], {}
    try:
        for l in lines:
            out.append(l)
            if l.startswith("! sleep "):
                time.sleep(float(l.split()[2]))
            elif l.startswith("> "):
                op, _, body = l[2:].partition(" ")
                status, text = call(url, op, substitute(body, seen))
                try:
                    v = json.loads(text)
                    harvest(v, seen)
                    text = json.dumps(normalize(v), separators=(",", ":"), ensure_ascii=False)
                except ValueError:
                    text = json.dumps(text)  # non-JSON body: a JSON string
                out.append(f"< {status} {text}")
    finally:
        drop(url, names)
    with open(path, "w", encoding="utf-8") as f:
        f.write("\n".join(out) + "\n")

if __name__ == "__main__":
    args = sys.argv[1:]
    url = "http://127.0.0.1:18000/"
    if args[:1] == ["--url"]:
        url, args = args[1], args[2:]
    for p in args:
        run(url, p)
        print("captured", p)
