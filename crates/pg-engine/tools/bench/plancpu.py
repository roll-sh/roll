#!/usr/bin/env python3
"""Planning CPU per statement of the engine against PostgreSQL 18.6.

    tools/bench/plancpu.py --pg-port 53100 --pg-container a155-pg \
        --engine-port 53101 --engine-pid <pid> --db apps --sets apps [--sets tpch] \
        [--protocols simple,extended] [--only q01,oban_fetch] [--target-s 1.0] [--reps 2]

Each statement runs as `EXPLAIN <statement>` through pgbench (one client, `-M simple` or
`-M extended`: an unnamed statement parsed, bound and planned for every execution) from a
helper container of the pinned image on the host network, so the client's CPU is never the
servers'. Server CPU is the engine process's utime + stime (/proc/<pid>/stat) and the
PostgreSQL container's cgroup usage (cpu.stat usage_usec); the count of executions is sized
from a pilot so each measurement takes about --target-s of server CPU. EXPLAIN without
ANALYZE parses, binds and plans the statement and renders the plan on both servers, so the
figure is planning plus EXPLAIN's text; the `explain_floor` row (`EXPLAIN SELECT 1`) shows
that fixed part. Sets: `tpch` (tools/bench/tpch.rs's 22 queries) and the files
`tools/bench/plans/<set>.sql` (`-- name:` sections, as plancmp.py reads them). Prints one line
per statement and protocol (microseconds of server CPU per statement, engine / PostgreSQL),
then the geometric mean of the ratios and the engine's peak RSS (VmHWM).
"""
import argparse
import math
import re
import os
import subprocess
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from plancmp import IMG, file_queries, tpch_queries  # noqa: E402


def docker(*args, **kw):
    return subprocess.run(["docker", *args], capture_output=True, text=True, **kw)


def ensure_helper(name, workdir):
    r = docker("inspect", "-f", "{{.State.Running}}", name)
    if r.stdout.strip() == "true":
        return
    if r.returncode == 0:
        docker("start", name, check=True)
        return
    docker("run", "-d", "--name", name, "--network", "host", "--user", f"{os.getuid()}:{os.getgid()}",
           "-v", f"{workdir}:{workdir}", "--entrypoint", "sleep", IMG, "infinity", check=True)


def engine_cpu(pid):
    f = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
    return (int(f[11]) + int(f[12])) / os.sysconf("SC_CLK_TCK")


def engine_hwm_mb(pid):
    for line in open(f"/proc/{pid}/status"):
        if line.startswith("VmHWM:"):
            return int(line.split()[1]) / 1024
    return 0.0


def container_cpu(cid):
    for line in open(f"/sys/fs/cgroup/system.slice/docker-{cid}.scope/cpu.stat"):
        if line.startswith("usage_usec"):
            return int(line.split()[1]) / 1e6
    raise RuntimeError("no usage_usec")


def pgbench(helper, port, db, script, proto, n):
    r = docker("exec", "-e", "PGPASSWORD=pw", helper, "pgbench", "-n", "-h", "127.0.0.1", "-p", str(port),
               "-U", "postgres", "-M", proto, "-t", str(n), "-f", script, db)
    if r.returncode != 0 or "failed: 0" not in r.stdout and "failed transactions: 0" not in r.stdout:
        raise RuntimeError(f"pgbench on {port} failed:\n{r.stdout[-600:]}\n{r.stderr[-600:]}")


def measure(helper, port, db, script, proto, n, cpu):
    a = cpu()
    pgbench(helper, port, db, script, proto, n)
    return (cpu() - a) / n * 1e6


def planning_us(helper, port, db, script):
    """The median `Planning Time` (us) of a psql script's EXPLAIN (SUMMARY)s, the first
    third (warm-up) left out."""
    r = docker("exec", "-e", "PGPASSWORD=pw", helper, "psql", "-X", "-q", "-At", "-h", "127.0.0.1",
               "-p", str(port), "-U", "postgres", "-d", db, "-f", script)
    t = [float(x) * 1000 for x in re.findall(r"Planning Time: ([0-9.]+) ms", r.stdout)]
    if not t:
        raise RuntimeError(f"no Planning Time from {port}:\n{r.stdout[-400:]}\n{r.stderr[-400:]}")
    t = sorted(t[len(t) // 3:])
    return t[len(t) // 2]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pg-port", type=int, required=True)
    ap.add_argument("--pg-container", required=True)
    ap.add_argument("--engine-port", type=int, required=True)
    ap.add_argument("--engine-pid", type=int, required=True)
    ap.add_argument("--db", default="apps")
    ap.add_argument("--sets", action="append", default=[])
    ap.add_argument("--protocols", default="simple,extended")
    ap.add_argument("--only", default="")
    ap.add_argument("--target-s", type=float, default=1.0)
    ap.add_argument("--reps", type=int, default=1)
    ap.add_argument("--summary", action="store_true", help="each server's EXPLAIN (SUMMARY) Planning Time (median, warm) instead of EXPLAIN CPU")
    ap.add_argument("--helper", default=f"plancpu-pgbench-{os.environ.get('USER', 'u')}")
    ap.add_argument("--workdir", default=os.path.expanduser("~/.cache/plancpu"))
    a = ap.parse_args()
    os.makedirs(a.workdir, exist_ok=True)
    ensure_helper(a.helper, a.workdir)
    cid = docker("inspect", "-f", "{{.Id}}", a.pg_container, check=True).stdout.strip()
    queries = [("explain_floor", "SELECT 1")]
    for s in ",".join(a.sets or ["apps"]).split(","):
        queries += tpch_queries() if s == "tpch" else file_queries(s)
    only = [o for o in a.only.split(",") if o]
    if only:
        queries = [q for q in queries if q[0] == "explain_floor" or any(o == q[0] or q[0].endswith(":" + o) for o in only)]
    sides = [("engine", a.engine_port, lambda: engine_cpu(a.engine_pid)), ("pg", a.pg_port, lambda: container_cpu(cid))]
    ratios = {p: [] for p in a.protocols.split(",")}
    unit = "planning us" if a.summary else "EXPLAIN cpu us"
    print(f"{'statement':40} {'proto':8} {'engine':>10} {'pg':>10} {'ratio':>6}   ({unit})")
    for name, q in queries:
        script = os.path.join(a.workdir, "q.sql")
        with open(script, "w") as f:
            f.write("EXPLAIN " + q + ";\n")
        for proto in ratios:
            res = {}
            for side, port, cpu in sides:
                if a.summary:
                    # EXPLAIN (SUMMARY) 30 times in a session (`\bind \g`: the extended
                    # protocol), each server's own Planning Time.
                    sscript = os.path.join(a.workdir, "s.sql")
                    end = " \\bind \\g\n" if proto == "extended" else ";\n"
                    with open(sscript, "w") as f:
                        f.write(("EXPLAIN (SUMMARY) " + q + end) * 30)
                    res[side] = planning_us(a.helper, port, a.db, sscript)
                    continue
                # A warm-up and pilot, then runs sized to about target_s of server CPU each.
                pilot = measure(a.helper, port, a.db, script, proto, 20, cpu)
                n = int(min(20000, max(20, a.target_s * 1e6 / max(pilot, 1.0))))
                res[side] = min(measure(a.helper, port, a.db, script, proto, n, cpu) for _ in range(a.reps))
            r = res["engine"] / max(res["pg"], 1e-9)
            if name != "explain_floor":
                ratios[proto].append(r)
            print(f"{name:40} {proto:8} {res['engine']:10.1f} {res['pg']:10.1f} {r:6.2f}", flush=True)
    for proto, rs in ratios.items():
        if rs:
            g = math.exp(sum(math.log(r) for r in rs) / len(rs))
            print(f"GEOMEAN {proto} engine/pg={g:.3f} over {len(rs)}; worse than pg: {sum(r > 1 for r in rs)}")
    print(f"ENGINE VmHWM {engine_hwm_mb(a.engine_pid):.0f} MB")


if __name__ == "__main__":
    main()
