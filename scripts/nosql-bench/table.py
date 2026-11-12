"""Prints a markdown table from scripts/nosql-bench.sh output: throughput, p50/p99/p99.9 (ms) and
peak memory (rolld's RSS; the container's memory for the references) per workload and target.

    python3 table.py OUT [OUT ...]

With several result directories (rounds), each number is the mean over the rounds and the last
column is the range of the throughput.
"""
import os
import re
import sys

dirs = sys.argv[1:]
targets = ["roll-scylla", "roll-cassandra", "scylla", "cassandra", "scylla-shared"]
workloads = ["write", "read", "unprepared", "mixed", "lwt", "batch", "wide"]


def num(s):
    return float(s.replace(",", ""))


def cassandra_stress(text):
    """The `Results:` summary of cassandra-stress."""
    if "Results:" not in text:
        return None
    text = text[text.rindex("Results:"):]
    g = lambda k: re.search(rf"{k}\s*:\s*([\d,\.]+)", text)
    rate, p50, p99, p999 = g("Op rate"), g("Latency median"), g(r"Latency 99th percentile"), g(r"Latency 99\.9th percentile")
    if not rate:
        return None
    return num(rate.group(1)), num(p50.group(1)), num(p99.group(1)), num(p999.group(1))


def latte(text):
    """latte's summary: `Throughput` (op/s) and the cycle latency percentiles (ms)."""
    m = re.search(r"Throughput\s+\[op/s\]\s+([\d\.]+)", text)
    if not m:
        return None
    pct = lambda p: re.search(rf"^\s*{re.escape(p)}\s+([\d\.]+)", text, re.M)
    vals = [pct(p) for p in ("50", "99", "99.9")]
    if not all(vals):
        return None
    return (num(m.group(1)), *[num(v.group(1)) for v in vals])


def scylla_bench(text):
    """scylla-bench's results (operations/s; latencies with units)."""
    m = re.search(r"Operations/s:\s+([\d\.]+)", text)
    if not m:
        return None

    def lat(k):
        x = re.search(rf"{k}:\s+([\d\.]+)(µs|us|ms|s)", text)
        if not x:
            return float("nan")
        v = float(x.group(1))
        return {"µs": v / 1000, "us": v / 1000, "ms": v, "s": v * 1000}[x.group(2)]

    return float(m.group(1)), lat("median"), lat("99th"), lat("99.9th")


def peak_memory(path):
    """Peak of the sampled memory: bytes (rolld's VmRSS) or `docker stats` sizes (`1.2GiB`)."""
    if not os.path.exists(path):
        return None
    units = {"B": 1, "KiB": 2**10, "MiB": 2**20, "GiB": 2**30, "kB": 1e3, "MB": 1e6, "GB": 1e9}
    vals = []
    for x in open(path).read().split():
        m = re.fullmatch(r"([\d\.]+)([A-Za-z]*)", x)
        if m:
            vals.append(float(m.group(1)) * units.get(m.group(2) or "B", 1))
    return max(vals) / 2**20 if vals else None


def mean(xs):
    return sum(xs) / len(xs)


rows = []
for w in workloads:
    for t in targets:
        runs, mems = [], []
        for d in dirs:
            p = os.path.join(d, t, f"{w}.txt")
            if not os.path.exists(p):
                continue
            text = open(p, errors="replace").read()
            r = cassandra_stress(text) or latte(text) or scylla_bench(text)
            if r is not None:
                runs.append(r)
            m = peak_memory(os.path.join(d, t, f"{w}.rss"))
            if m is not None:
                mems.append(m)
        if not runs and not mems:
            continue
        mem = f"{max(mems):.0f} MiB" if mems else ""
        if not runs:
            rows.append(f"| {w} | {t} | failed | | | | {mem} | |")
            continue
        ops = [r[0] for r in runs]
        spread = f"{min(ops):,.0f}–{max(ops):,.0f}" if len(runs) > 1 else ""
        rows.append(
            f"| {w} | {t} | {mean(ops):,.0f} | {mean([r[1] for r in runs]):.2f} | {mean([r[2] for r in runs]):.2f} | "
            f"{mean([r[3] for r in runs]):.2f} | {mem} | {spread} |"
        )

print("| workload | target | op/s | p50 ms | p99 ms | p99.9 ms | peak memory | op/s range |")
print("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |")
print("\n".join(rows))
