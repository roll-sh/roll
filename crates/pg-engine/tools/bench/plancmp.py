#!/usr/bin/env python3
"""EXPLAIN plan shapes of the engine against PostgreSQL 18.6, query by query.

    tools/bench/plancmp.py --pg-port 52320 --engine-port 52321 [--db tpch] \
        [--sets tpch,orm,tpcds] [--serial] [--show q18,orm_07] [--out DIR]

Runs `EXPLAIN` (costs on) of every query of the sets on both servers through psql in a
helper container of the pinned image on the host network (`--psql-container`, default
started on demand as `plancmp-psql-$USER`), and reports per query whether the plan's shape
matches (node lines: node type, relation, index and alias, indentation; costs, rows and
widths left out), whether the whole text matches with costs left out (conditions, sort keys,
Workers Planned too), and the engine's total cost over PostgreSQL's. Sets: `tpch` (the 22
queries of tools/bench/tpch.rs), and the files `tools/bench/plans/<set>.sql`, whose queries
are separated by `-- name: <name>` lines (optionally with `-- setup:` statements run in
the same session first). `--serial` sets max_parallel_workers_per_gather = 0. The last line
is `SUMMARY shape=N/M text=N/M`.
"""
import argparse
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
IMG = "postgres@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873"


def tpch_queries():
    src = open(os.path.join(HERE, "tpch.rs"), encoding="utf-8").read()
    body = src[src.index("pub const QUERIES"):]
    return [(n, q) for n, q in re.findall(r'\(\s*"(q\d\d)",\s*"((?:[^"\\]|\\.)*)"\s*\)', body)]


def file_queries(name):
    out, cur, buf = [], None, []
    for line in open(os.path.join(HERE, "plans", name + ".sql"), encoding="utf-8"):
        m = re.match(r"--\s*name:\s*(\S+)", line)
        if m:
            if cur:
                out.append((cur, " ".join(buf).strip().rstrip(";")))
            cur, buf = m.group(1), []
        elif cur and not line.lstrip().startswith("--"):
            buf.append(line.strip())
    if cur:
        out.append((cur, " ".join(buf).strip().rstrip(";")))
    return [(f"{name}:{n}", q) for n, q in out]


def ensure_helper(name):
    r = subprocess.run(["docker", "inspect", "-f", "{{.State.Running}}", name], capture_output=True, text=True)
    if r.stdout.strip() == "true":
        return
    if r.returncode == 0:
        subprocess.run(["docker", "start", name], check=True, capture_output=True)
        return
    subprocess.run(["docker", "run", "-d", "--name", name, "--network", "host", "--entrypoint", "sleep",
                    IMG, "infinity"], check=True, capture_output=True)


def explain_all(helper, port, db, queries, serial, timeout):
    script = ["\\set ON_ERROR_STOP 0", "\\pset pager off"]
    if serial:
        script.append("SET max_parallel_workers_per_gather = 0;")
    for name, q in queries:
        script.append(f"\\echo @@@ {name}")
        script.append(f"EXPLAIN {q};")
    r = subprocess.run(["docker", "exec", "-i", "-e", "PGPASSWORD=pw", helper, "psql", "-h", "127.0.0.1", "-p",
                        str(port), "-U", "postgres", "-d", db, "-AtX", "-v", "VERBOSITY=terse"],
                       input="\n".join(script) + "\n", capture_output=True, text=True, timeout=timeout)
    plans, cur, in_jit = {}, None, False
    for line in (r.stdout + "\n" + r.stderr).splitlines():
        if line.startswith("@@@ "):
            cur, in_jit = line[4:].strip(), False
            plans[cur] = []
        elif cur is not None and line.strip() and not line.startswith("SET"):
            # (PostgreSQL's JIT block: the engine is a PostgreSQL without a JIT provider.)
            in_jit = line.startswith("JIT:") or (in_jit and line.startswith("  "))
            if not in_jit:
                plans[cur].append(line)
    return plans


COST = re.compile(r"\s+\(cost=([\d.]+)\.\.([\d.]+) rows=\d+ width=\d+\)")


def shape(lines):
    out = []
    for i, l in enumerate(lines):
        if i == 0 or "->" in l:
            node = COST.sub("", l).rstrip()
            out.append(node)
    return out


def text(lines):
    return [COST.sub("", l).rstrip() for l in lines]


def total_cost(lines):
    m = COST.search(lines[0]) if lines else None
    return float(m.group(2)) if m else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pg-port", type=int, required=True)
    ap.add_argument("--engine-port", type=int, required=True)
    ap.add_argument("--db", default="tpch")
    ap.add_argument("--sets", default="tpch")
    ap.add_argument("--serial", action="store_true")
    ap.add_argument("--show", default="", help="comma-separated query names to print side by side")
    ap.add_argument("--diff", action="store_true", help="print both plans of every shape mismatch")
    ap.add_argument("--psql-container", default=f"plancmp-psql-{os.environ.get('USER', 'x')}")
    ap.add_argument("--timeout", type=int, default=1800)
    a = ap.parse_args()
    queries = []
    for s in a.sets.split(","):
        queries += tpch_queries() if s == "tpch" else file_queries(s)
    ensure_helper(a.psql_container)
    pg = explain_all(a.psql_container, a.pg_port, a.db, queries, a.serial, a.timeout)
    en = explain_all(a.psql_container, a.engine_port, a.db, queries, a.serial, a.timeout)
    show = set(x for x in a.show.split(",") if x)
    n_shape = n_text = n = 0
    for name, _ in queries:
        p, e = pg.get(name, []), en.get(name, [])
        if not p or "cost=" not in p[0]:
            print(f"{name:24} SKIP (reference: {' '.join(p)[:100]})")
            continue
        n += 1
        sm, tm = shape(p) == shape(e), text(p) == text(e)
        n_shape += sm
        n_text += tm
        pc, ec = total_cost(p), total_cost(e)
        ratio = f"{ec / pc:.3f}" if pc and ec else "-"
        print(f"{name:24} shape={'ok' if sm else 'DIFF'} text={'ok' if tm else 'diff'} cost={ratio}")
        if name in show or (a.diff and not sm):
            print("  --- PostgreSQL"); print("\n".join("  " + l for l in p))
            print("  --- engine"); print("\n".join("  " + l for l in e))
    print(f"SUMMARY shape={n_shape}/{n} text={n_text}/{n}")


if __name__ == "__main__":
    sys.exit(main())
