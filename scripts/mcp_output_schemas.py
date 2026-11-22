"""Output schemas for roll's MCP tools, learned from real answers (docs/mcp.md, "Output schemas").

roll's routes have no response schemas, so the MCP sweep (scripts/mcp-sweep.py) records what each
tool answered on a fresh rolld and this module turns those answers into loose JSON Schemas: the
fields seen and their types, every value nullable, more fields always allowed, nothing required,
at most DEPTH levels. A client running code against a tool learns its field names; one that
validates answers never trips on a field a product adds. `check` is what the sweep fails on: an
answer whose type no longer matches the schema it was published with.

    python3 -I scripts/mcp_output_schemas.py <samples.json>   # widen crates/roll-mcp/src/output-schemas.json
"""
import json, sys

DEPTH = 4
# An object below the top with more keys than this is a map (configs, per-index counts): its keys
# are data, so it's described as an object without them.
MAP_KEYS = 24
SCHEMAS = "crates/roll-mcp/src/output-schemas.json"


def kind(v):
    if v is None:
        return None
    if isinstance(v, bool):
        return "boolean"
    if isinstance(v, (int, float)):
        return "number"
    if isinstance(v, str):
        return "string"
    if isinstance(v, list):
        return "array"
    return "object"


def infer(v, depth=0, words=frozenset()):
    """A schema for one value: {} (anything) for null or past DEPTH. An object keyed by one of
    `words` (the call's own argument values: an index's name) is a map, as is a big one."""
    k = kind(v)
    if k is None or depth >= DEPTH:
        return {}
    s = {"type": [k, "null"]}
    if k == "object":
        if not words.intersection(v) and (depth == 0 or len(v) <= MAP_KEYS):
            s["properties"] = {key: infer(x, depth + 1, words) for key, x in sorted(v.items())}
    elif k == "array":
        items = None
        for x in v:
            items = infer(x, depth + 1, words) if items is None else merge(items, infer(x, depth + 1, words))
        if items is not None:
            s["items"] = items
    return s


def merge(a, b):
    """The narrowest schema both fit: types joined, properties joined; {} takes anything."""
    if a == {} or b == {}:
        return {}
    s = {"type": sorted(set(a["type"]) | set(b["type"]), key=lambda t: (t == "null", t))}
    # One side saw it as a map: it stays one.
    if ("object" in a["type"] and "properties" not in a) or ("object" in b["type"] and "properties" not in b):
        pass
    elif "properties" in a or "properties" in b:
        pa, pb = a.get("properties", {}), b.get("properties", {})
        s["properties"] = {k: merge(pa[k], pb[k]) if k in pa and k in pb else pa.get(k, pb.get(k)) for k in sorted(set(pa) | set(pb))}
    if "items" in a or "items" in b:
        s["items"] = merge(a["items"], b["items"]) if "items" in a and "items" in b else a.get("items", b.get("items"))
    return s


def top(s):
    """A tool's published schema: structuredContent is always an object."""
    s = dict(s)
    s["type"] = "object"
    s.setdefault("properties", {})
    return s


def check(v, s, at="$"):
    """Where `v` breaks `s`, or None."""
    if s == {}:
        return None
    types = s["type"] if isinstance(s["type"], list) else [s["type"]]
    k = kind(v)
    if (k or "null") not in types:
        return f"{at} is {k or 'null'}, the schema says {'/'.join(types)}"
    if k == "object":
        for key, x in v.items():
            if key in s.get("properties", {}):
                bad = check(x, s["properties"][key], f"{at}.{key}")
                if bad:
                    return bad
    if k == "array" and "items" in s:
        for i, x in enumerate(v):
            bad = check(x, s["items"], f"{at}[{i}]")
            if bad:
                return bad
    return None


def widen(schemas, samples):
    """`schemas` widened by `samples` ({tool: [{"answer": structuredContent, "words": [the call's
    argument values]}, …]}): what the sweep saw joins."""
    out = dict(schemas)
    for tool, answers in samples.items():
        for a in answers:
            got = top(infer(a["answer"], 0, frozenset(a["words"])))
            out[tool] = top(merge(out[tool], got)) if tool in out else got
    return dict(sorted(out.items()))


if __name__ == "__main__":
    samples = json.load(open(sys.argv[1]))
    try:
        schemas = json.load(open(SCHEMAS))
    except FileNotFoundError:
        schemas = {}
    out = widen(schemas, samples)
    with open(SCHEMAS, "w") as f:
        json.dump(out, f, indent=1, sort_keys=True)
        f.write("\n")
    print(f"{len(out)} tools have an output schema ({len(out) - len(schemas)} new), {len(json.dumps(out, separators=(',', ':'))) // 1024} KB")
