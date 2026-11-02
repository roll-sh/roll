"""Inspect retained, independently validated CPU stacks; never run a workload."""
from collections import Counter
from pathlib import Path
import hashlib
import json
import sys

root = Path(sys.argv[1])
results = []
for path in sorted(root.glob('*-classified.json')):
    data = json.loads(path.read_text())
    if data['summary']['label'] not in [f'{i}-{mode}' for i in range(3) for mode in ['auto-8', 'custom-1', 'generic-1']]:
        continue
    self_weight, inclusive, paths = Counter(), Counter(), Counter()
    total = 0
    for sample in data['samples']:
        weight = sample['period']
        names = [f['symbol'] or '<unresolved>' for f in sample['frames']]
        total += weight
        self_weight[names[0]] += weight
        for name in set(names):
            inclusive[name] += weight
        if any('pg_engine::exec::sizing::fetched' == n for n in names):
            paths['heap_fetch_size_model'] += weight
        if any('pg_engine::exec::parallel_part::start' == n or 'pg_engine::exec::parallel_part::gathers' == n for n in names):
            paths['execution_gather_discovery'] += weight
        if any('pg_engine::sql::explain::' in n and ('::show' in n or '::name' in n or 'index_cond' in n) for n in names):
            paths['explain_expression_display'] += weight
        if any(n == 'pg_engine::session::plan_cost_estimate' or n == '<pg_engine::session::Session>::plan_cost' for n in names):
            paths['root_costing'] += weight
        if any(n == '<pg_engine::session::Session>::bound_custom' for n in names):
            paths['custom_binding'] += weight
    assert total == data['summary']['total_period']
    assert len(data['samples']) == data['summary']['samples']
    def top(counter):
        return [{'symbol': n, 'sample_weight_fraction': w / total} for n, w in counter.most_common(25)]
    results.append({'label': data['summary']['label'], 'input_sha256': hashlib.sha256(path.read_bytes()).hexdigest(),
                    'samples': len(data['samples']), 'period_weight': total,
                    'recovered_ancestry_fraction': {n:w/total for n,w in paths.items()},
                    'top_self': top(self_weight), 'top_inclusive': top(inclusive)})
assert len(results) == 9
print(json.dumps({'captures': results, 'limits': 'Reanalysis of retained profiles, not a fresh capture or speedup experiment. Inclusive ancestry categories overlap and finite stacks can omit ancestors. Do not add categories or treat them as removable CPU. Original maps/ELF/CFI validation is recorded in the planning-cpu checkpoint.'}, indent=2))
