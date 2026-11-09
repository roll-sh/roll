"""Public scoreboard output for the single default configuration (no servers required)."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
import scoreboard


class ScoreboardTest(unittest.TestCase):
    def test_retired_benchmark_arm_rejected(self):
        run = Path(__file__).with_name('run.sh')
        with tempfile.TemporaryDirectory() as data:
            env = dict(os.environ, BENCH_SG='1', BENCH_DATA=data)
            for args in [('engperf-start',), ('pgbench', 'engperf')]:
                result = subprocess.run(['sh', str(run), *args], env=env,
                                        capture_output=True, text=True, timeout=5)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('engperf', result.stderr)

    def test_historical_mode_comparison_is_not_relabelled(self):
        with self.assertRaisesRegex(ValueError, 'Historical mode-comparison input'):
            scoreboard.table({}, {'fresh_data_mb': {'engine': 3, 'engperf': 3}}, [], 'quick', 1, 60)

    def test_default_profile_memory_is_informational(self):
        score = {(w, t): [dict(round='1', kind='time', tps='10', ok='1')]
                 for w in ['read'] for t in ['engine', 'pg', 'pgdef', 'pgrust']}
        gate = {k: dict(engine=e, pg=p) for k, e, p in [
            ('idle_cpu_s_5min', 0.01, 0.1),
            ('idle_100_sessions_mb', 36, 100),
            ('fresh_data_mb', 3, 50),
        ]}
        text, failed = scoreboard.table(score, gate, ['read'], 'quick', 1, 60)
        self.assertFalse(failed)
        self.assertIn('| memory with 100 idle sessions (MB) | 36 | 100 | 2.8x | n/a | info |', text)
        self.assertNotIn('engine efficient', text)
        self.assertNotIn('engine performance', text)
        self.assertIn('memory-derived defaults', text)
        self.assertNotIn('--mode', text)


if __name__ == '__main__':
    unittest.main()
