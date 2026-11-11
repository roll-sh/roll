#!/usr/bin/env python3
"""Alternate two bench endpoints; reject errors, missing work, and differing results."""
import argparse
import hashlib
import json
import math
import statistics
import subprocess
from pathlib import Path


def results(output, expected):
    found = {}
    for line in output.splitlines():
        if not line.startswith("RESULT\t"):
            continue
        fields = line.split("\t")
        if len(fields) < 5 or fields[2] != "analytic":
            raise ValueError(f"unexpected result: {line}")
        name = fields[3]
        if name in found or name not in expected:
            raise ValueError(f"duplicate or unexpected case: {name}")
        values = dict(field.split("=", 1) for field in fields[4:])
        if "error" in values or int(values.get("errors", "0")):
            raise ValueError(f"query failed: {name}: {values}")
        for key in ("min_s", "median_s", "max_s", "peak_mem_mb"):
            value = float(values[key])
            if not math.isfinite(value) or value <= 0:
                raise ValueError(f"missing or invalid {key}: {name}")
        if not float(values["min_s"]) <= float(values["median_s"]) <= float(values["max_s"]):
            raise ValueError(f"inconsistent timing range: {name}")
        cpu = float(values["cpu_s"])
        if not math.isfinite(cpu) or cpu < 0:
            raise ValueError(f"invalid CPU measurement: {name}")
        if int(values["rows"]) != expected[name] or expected[name] <= 0:
            raise ValueError(f"wrong or empty result: {name}: {values['rows']}")
        digest = values["hash"]
        if len(digest) != 16 or any(c not in "0123456789abcdef" for c in digest):
            raise ValueError(f"invalid result hash: {name}")
        found[name] = values
    if set(found) != set(expected):
        raise ValueError(f"missing cases: {set(expected) - set(found)}")
    return found


def equal_outputs(left, right):
    if set(left) != set(right):
        raise ValueError("case sets differ")
    for name in left:
        for key in ("rows", "hash"):
            if left[name][key] != right[name][key]:
                raise ValueError(f"{name}: {key} differs: {left[name][key]} != {right[name][key]}")


def self_test():
    good = "RESULT\tx\tanalytic\ta\tmin_s=1\tmedian_s=1\tmax_s=1\trows=1\thash=0123456789abcdef\tpeak_mem_mb=1\tcpu_s=0.01\n"
    parsed = results(good, {"a": 1})
    equal_outputs(parsed, parsed)
    checks = [
        lambda: results("", {"a": 1}),
        lambda: results(good + good, {"a": 1}),
        lambda: results(good.replace("rows=1", "rows=0"), {"a": 1}),
        lambda: results(good.replace("median_s=1", "median_s=nan"), {"a": 1}),
        lambda: results(good.rstrip() + "\terror=failed\n", {"a": 1}),
        lambda: results(good.replace("hash=0123456789abcdef", "hash="), {"a": 1}),
        lambda: equal_outputs(parsed, results(good.replace("0123456789abcdef", "1123456789abcdef"), {"a": 1})),
    ]
    pair = {"a": {"rows": "1", "hash": "a"}, "b": {"rows": "1", "hash": "b"}}
    checks.append(lambda: equal_outputs(pair, {"a": pair["b"], "b": pair["a"]}))
    for check in checks:
        try:
            check()
        except (ValueError, KeyError):
            continue
        raise AssertionError("negative control was accepted")
    print(f"paired benchmark checks: {len(checks)} negative controls rejected")


def run(config):
    out = Path(config["out"])
    out.mkdir(parents=True, exist_ok=False)
    bench = Path(config["bench"]).resolve()
    expected = config["expected_rows"]
    targets = config["targets"]
    rounds = config.get("rounds", 5)
    if len(targets) != 2 or rounds < 5 or len({t["name"] for t in targets}) != 2:
        raise ValueError("need two named endpoints and at least five rounds")
    config["bench_sha256"] = hashlib.sha256(bench.read_bytes()).hexdigest()
    config["source_commit"] = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    config["load_at_start"] = Path("/proc/loadavg").read_text().strip()
    (out / "config.json").write_text(json.dumps(config, indent=2) + "\n")
    saved = {t["name"]: {case: [] for case in expected} for t in targets}
    reference = None
    for round_no in range(rounds + 1):
        order = targets if round_no % 2 == 0 else list(reversed(targets))
        for target in order:
            command = ["taskset", "-c", config.get("client_cpus", "32-35"), str(bench),
                       "--target", "wire", "--label", target["name"], "--port", str(target["port"]),
                       "--db", config["db"], "analytic", "--reps", "1", "--only", ",".join(expected),
                       "--timeout", str(config.get("query_timeout", 600))]
            probe = target["probe"]
            if "pid" in probe:
                if not Path(f"/proc/{probe['pid']}/status").exists():
                    raise ValueError("server PID missing")
                command += ["--mem-pid", str(probe["pid"])]
            else:
                for filename in ("memory.stat", "cpu.stat"):
                    if not (Path(probe["cgroup"]) / filename).is_file():
                        raise ValueError(f"missing cgroup probe: {filename}")
                command += ["--mem-cgroup", probe["cgroup"]]
            command += config.get("args", [])
            print(f"round {round_no}: {target['name']}", flush=True)
            process = subprocess.run(command, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                     timeout=config.get("command_timeout", 7200))
            stem = out / f"{round_no:02d}-{target['name']}"
            stem.with_suffix(".log").write_text(process.stdout)
            stem.with_suffix(".json").write_text(json.dumps({"command": command, "exit": process.returncode}, indent=2) + "\n")
            if process.returncode:
                raise ValueError(f"benchmark exit {process.returncode}; see {stem}.log")
            values = results(process.stdout, expected)
            if reference is None:
                reference = values
            equal_outputs(reference, values)
            if round_no:
                for case in expected:
                    saved[target["name"]][case].append(values[case])
    report = {}
    for case in expected:
        report[case] = {}
        for target in targets:
            samples = saved[target["name"]][case]
            stats = {}
            for key in ("median_s", "cpu_s", "peak_mem_mb"):
                numbers = [float(sample[key]) for sample in samples]
                stats[key] = {"median": statistics.median(numbers), "min": min(numbers), "max": max(numbers)}
            stats["samples"] = len(samples)
            stats["rows"] = expected[case]
            stats["hash"] = samples[0]["hash"]
            report[case][target["name"]] = stats
    (out / "summary.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("config", nargs="?", type=Path)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
    elif args.config:
        run(json.loads(args.config.read_text()))
    else:
        parser.error("provide a configuration JSON or --self-test")
