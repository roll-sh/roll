#!/usr/bin/env python3
"""A synthetic ECS-style access-log corpus (objects several levels deep, as Beats and Elastic
Agent send them) and its mapping, for indexing benchmarks.

    scripts/bench-search-ecs.py <docs> <out.json> <mapping.json>

then `scripts/bench-search.sh ingest --file <out.json> --body <mapping.json>`.
"""
import json
import random
import sys

n, out, mapping = int(sys.argv[1]), sys.argv[2], sys.argv[3]
r = random.Random(42)
paths = ["/", "/api/orders", "/api/users/%d", "/static/app.js", "/login", "/search?q=%d", "/cart"]
agents = ["Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/131.0 Safari/537.36",
          "curl/8.10.1", "Go-http-client/2.0", "Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X)"]
with open(out, "w") as f:
    t = 1_731_484_800_000
    for i in range(n):
        t += r.randint(0, 40)
        p = r.choice(paths)
        if "%d" in p:
            p = p % r.randint(1, 100000)
        status = r.choice([200] * 20 + [201, 204, 301, 304, 400, 401, 403, 404, 404, 500, 503])
        doc = {
            "@timestamp": t,
            "host": {"name": "web-%02d" % r.randint(1, 40), "ip": "10.0.%d.%d" % (r.randint(0, 3), r.randint(1, 254))},
            "source": {"ip": "%d.%d.%d.%d" % tuple(r.randint(1, 254) for _ in range(4)), "port": r.randint(1024, 65535)},
            "http": {"request": {"method": r.choice(["GET"] * 8 + ["POST", "PUT", "DELETE"])},
                     "response": {"status_code": status, "body": {"bytes": r.randint(0, 200000)}}},
            "url": {"path": p, "domain": r.choice(["shop.example.com", "api.example.com"])},
            "user_agent": {"original": r.choice(agents)},
            "event": {"duration": r.randint(100_000, 900_000_000), "dataset": "nginx.access", "outcome": "success" if status < 400 else "failure"},
            "message": "%s %s %d" % (r.choice(["GET", "POST"]), p, status),
        }
        f.write(json.dumps(doc, separators=(",", ":")) + "\n")
kw = {"type": "keyword"}
body = {
    "settings": {"index.number_of_shards": 1, "index.number_of_replicas": 0},
    "mappings": {"dynamic": "strict", "properties": {
        "@timestamp": {"type": "date"},
        "host": {"properties": {"name": kw, "ip": {"type": "ip"}}},
        "source": {"properties": {"ip": {"type": "ip"}, "port": {"type": "long"}}},
        "http": {"properties": {"request": {"properties": {"method": kw}},
                                "response": {"properties": {"status_code": {"type": "long"}, "body": {"properties": {"bytes": {"type": "long"}}}}}}},
        "url": {"properties": {"path": kw, "domain": kw}},
        "user_agent": {"properties": {"original": {"type": "keyword", "fields": {"text": {"type": "match_only_text"}}}}},
        "event": {"properties": {"duration": {"type": "long"}, "dataset": kw, "outcome": kw}},
        "message": {"type": "match_only_text"},
    }},
}
json.dump(body, open(mapping, "w"))
