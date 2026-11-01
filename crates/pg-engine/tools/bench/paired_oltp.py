"""Checked read-only OLTP pairs; CPU covers each complete client invocation."""
import argparse
import hashlib
import json
import math
from pathlib import Path
import resource
import statistics
import subprocess


def parse(output, name, case, clients):
    lines = [line.split('\t') for line in output.splitlines() if line.startswith('RESULT\t')]
    if len(lines) != 1 or lines[0][1:4] != [name, 'oltp', case]:
        raise ValueError('missing, duplicate or unexpected result')
    values = dict(field.split('=', 1) for field in lines[0][4:])
    if values.get('errors') != '0' or 'first_error' in values or int(values['clients']) != clients:
        raise ValueError('client errors or wrong client count')
    ops, elapsed = int(values['ops']), float(values['elapsed_s'])
    if ops <= 0 or not math.isfinite(elapsed) or elapsed <= 0:
        raise ValueError('no completed work')
    for key in ['tps', 'peak_mem_mb', 'mean_ms', 'p50_ms', 'p95_ms', 'p99_ms']:
        number = float(values[key])
        if not math.isfinite(number) or number <= 0:
            raise ValueError('invalid ' + key)
    if abs(float(values['tps']) - ops / elapsed) > .51:
        raise ValueError('TPS disagrees with exact operation count')
    if not float(values['p50_ms']) <= float(values['p95_ms']) <= float(values['p99_ms']):
        raise ValueError('invalid latency quantiles')
    return values


def cpu(probe, ticks):
    if 'pid' in probe:
        fields = Path(f"/proc/{probe['pid']}/stat").read_text().rsplit(')', 1)[1].split()
        return (int(fields[11]) + int(fields[12])) / ticks
    values = dict(line.split() for line in (Path(probe['cgroup']) / 'cpu.stat').read_text().splitlines())
    return int(values['usage_usec']) / 1e6


def fixture(config, target):
    query = "SELECT count(*),sum(id::bigint),sum(owner::bigint),sum(balance::numeric),md5(string_agg(id::text||':'||owner::text||':'||balance::text||':'||note||':'||created::text,',' ORDER BY id)) FROM accounts"
    return subprocess.check_output(['docker', 'exec', '-e', 'PGPASSWORD=pw', config['psql_container'],
        'psql', '-XAt', '-v', 'ON_ERROR_STOP=1', '-h', '127.0.0.1', '-p', str(target['port']),
        '-U', 'postgres', '-d', config['db'], '-c', query], text=True, timeout=120).strip()


def run(config):
    targets, rounds, seconds = config['targets'], config.get('rounds', 5), config.get('seconds', 2)
    if len(targets) != 2 or len({t['name'] for t in targets}) != 2 or rounds < 5 or seconds <= 0:
        raise ValueError('need two endpoints, five rounds and positive duration')
    cases = config['cases']
    if not cases or not set(cases) <= {'point_select', 'point_select_simple', 'range_scan_100', 'secondary_lookup', 'select_1'}:
        raise ValueError('only checked read-only cases are supported')
    out = Path(config['out']); out.mkdir(parents=True, exist_ok=False)
    ticks = int(subprocess.check_output(['getconf', 'CLK_TCK'], text=True)); assert ticks > 0
    config['runner_sha256'] = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    config['ticks_per_second'] = ticks
    config['bench_sha256'] = hashlib.sha256(Path(config['bench']).read_bytes()).hexdigest()
    config['source_commit'] = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
    for target in targets:
        if 'pid' in target['probe']:
            executable = Path(f"/proc/{target['probe']['pid']}/exe")
            target['actual_executable'] = str(executable.resolve(strict=True))
            target['actual_executable_sha256'] = hashlib.sha256(executable.read_bytes()).hexdigest()
    config['load_at_start'] = Path('/proc/loadavg').read_text().strip()
    config['client_cpu_measure'] = 'RUSAGE_CHILDREN user + system per invocation'
    config['server_cpu_measure'] = 'user + system process ticks or cgroup usage per complete client invocation, including setup'
    (out/'config.json').write_text(json.dumps(config, indent=2)+'\n')
    for target in targets:
        actual = fixture(config, target)
        (out/f"fixture-before-{target['name']}.txt").write_text(actual+'\n')
        if actual != config['expected_fixture']:
            raise ValueError('fixture differs from deterministic generator: ' + target['name'])
    saved = {}
    for round_no in range(rounds + 1):
        for target in (targets if round_no % 2 == 0 else list(reversed(targets))):
            for case, counts in cases.items():
                for clients in counts:
                    if not isinstance(clients, int) or clients <= 0:
                        raise ValueError('invalid clients')
                    stem = out/f"{round_no:02d}-{target['name']}-{case}-{clients}"
                    command = ['taskset', '-c', config.get('client_cpus', '32-35'), config['bench'], '--target', 'wire',
                        '--label', target['name'], '--port', str(target['port']), '--db', config['db']]
                    probe = target['probe']
                    command += ['--mem-pid', str(probe['pid'])] if 'pid' in probe else ['--mem-cgroup', probe['cgroup']]
                    command += ['oltp', '--rows', str(config['rows']), '--only', case, '--clients', str(clients), '--secs', str(seconds)]
                    before = cpu(probe, ticks)
                    child_before = resource.getrusage(resource.RUSAGE_CHILDREN)
                    process = subprocess.run(command, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=seconds+30)
                    child_after = resource.getrusage(resource.RUSAGE_CHILDREN)
                    server_cpu = cpu(probe, ticks)-before
                    client_cpu = child_after.ru_utime+child_after.ru_stime-child_before.ru_utime-child_before.ru_stime
                    stem.with_suffix('.log').write_text(process.stdout)
                    record = {'command': command, 'exit': process.returncode, 'server_cpu_s': server_cpu, 'client_cpu_s': client_cpu}
                    stem.with_suffix('.json').write_text(json.dumps(record, indent=2)+'\n')
                    if process.returncode:
                        raise ValueError(f'benchmark exit {process.returncode}: {stem}')
                    values = parse(process.stdout, target['name'], case, clients)
                    if server_cpu <= 0 or client_cpu <= 0:
                        raise ValueError('invalid resource measurement')
                    values['server_cpu_us_per_op'] = server_cpu * 1e6 / int(values['ops'])
                    values['client_cpu_us_per_op'] = client_cpu * 1e6 / int(values['ops'])
                    if round_no:
                        saved.setdefault((case,clients,target['name']), []).append(values)
                    print(round_no, target['name'], case, clients, values['ops'], flush=True)
    summary = {}
    for (case, clients, name), samples in saved.items():
        stats = {}
        for key in ['tps','p50_ms','p95_ms','p99_ms','server_cpu_us_per_op','client_cpu_us_per_op','peak_mem_mb']:
            numbers = [float(s[key]) for s in samples]
            stats[key] = {'median': statistics.median(numbers), 'min': min(numbers), 'max': max(numbers)}
        stats['samples'] = len(samples)
        summary.setdefault(case,{}).setdefault(str(clients),{})[name] = stats
    for target in targets:
        actual = fixture(config, target)
        (out/f"fixture-after-{target['name']}.txt").write_text(actual+'\n')
        if actual != config['expected_fixture']:
            raise ValueError('read benchmark changed fixture: ' + target['name'])
    (out/'summary.json').write_text(json.dumps(summary, indent=2)+'\n')


def self_test():
    good = ('RESULT\tx\toltp\tpoint_select\tclients=1\ttps=1000\tops=1000\telapsed_s=1'
            '\tmean_ms=1\tp50_ms=0.5\tp95_ms=1\tp99_ms=2\terrors=0\tpeak_mem_mb=10\n')
    parse(good, 'x', 'point_select', 1)
    invalid = [
        '', good + good,
        good.replace('errors=0', 'errors=1'),
        good.replace('ops=1000', 'ops=0'),
        good.replace('elapsed_s=1', 'elapsed_s=nan'),
        good.replace('tps=1000', 'tps=9999'),
        good.replace('p99_ms=2', 'p99_ms=0.1'),
        good.replace('clients=1', 'clients=8'),
        good.replace('peak_mem_mb=10', 'peak_mem_mb=inf'),
    ]
    for output in invalid:
        try:
            parse(output, 'x', 'point_select', 1)
        except (ValueError, KeyError):
            continue
        raise AssertionError('negative control accepted')
    print(f'OLTP benchmark checks: {len(invalid)} negative controls rejected')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('config', nargs='?', type=Path)
    parser.add_argument('--self-test', action='store_true')
    args = parser.parse_args()
    if args.self_test:
        self_test()
    elif args.config:
        run(json.loads(args.config.read_text()))
    else:
        parser.error('provide a configuration JSON or --self-test')
