#!/usr/bin/env python3
"""Reject incomplete/failed footprint work even if the scoreboard wrapper returns zero."""
import math
from pathlib import Path
import re
import sys

root = Path(sys.argv[1])
raw = (root / 'score-quick.tsv').read_text()
log = (root / 'logs/scoreboard-quick.log').read_text()
metrics = {'fresh_data_mb', 'idle_100_sessions_mb', 'idle_cpu_s_5min'}
expected = {(side, metric) for side in ('pg', 'engine') for metric in metrics}
found = {}
for line in raw.splitlines():
    fields = line.split('\t')
    assert len(fields) == 4 and fields[0] == 'GATE', line
    key = (fields[1], fields[2])
    assert key in expected and key not in found, key
    value = float(fields[3])
    assert math.isfinite(value) and value >= 0, line
    if fields[2] != 'idle_cpu_s_5min':
        assert value > 0, line
    found[key] = value
assert set(found) == expected, (set(found), expected)
assert len(re.findall(r'number of transactions actually processed: 100/100', log)) == 2
assert len(re.findall(r'number of failed transactions: 0 \(', log)) == 2
assert 'PostgreSQL 18.6 ' in log
assert not re.search(r'\b(?:ERROR|FATAL|PANIC)\b|command not found|No such file', log), log
print('Verified six finite gate values, both 100-session workloads, PostgreSQL18.6 and no logged failures.')
