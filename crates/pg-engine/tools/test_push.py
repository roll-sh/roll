"""Exercise the push guard against disposable local Git repositories; no network pushes."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

PUSH = Path(__file__).with_name('push.sh').resolve()
BRANCH = 'docs/postgres-architecture-strategy'


class PushGuardTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='push-guard-')
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.repo = self.root / 'checkout'
        self.repo.mkdir()
        self.env = dict(os.environ)
        self.env.pop('PUSH_REMOVAL_REVIEW', None)
        self.env['PUSH_DRY'] = '1'
        self.git('init', '--bare', str(self.root / 'origin.git'))
        self.git('init', '-b', BRANCH)
        self.write('seed.txt', 'initial repository content\n')
        self.commit('initial state')
        self.write('retired.rs', ''.join(f'retired mode statement {i}\n' for i in range(8)))
        self.write('live.rs', ''.join(f'unrelated live statement {i}\n' for i in range(8)))
        self.commit('feature additions')
        self.git('remote', 'add', 'origin', str(self.root / 'origin.git'))
        self.git('push', 'origin', BRANCH)
        self.remote = self.git('rev-parse', 'HEAD')
        (self.repo / 'retired.rs').unlink()
        self.commit('intentional mode removal')

    def git(self, *args):
        result = subprocess.run(
            ['git', '-c', 'user.name=Guard Test', '-c', 'user.email=guard@example.test',
             '-c', 'commit.gpgsign=false', *args],
            cwd=self.repo, env=self.env, text=True, capture_output=True, check=True,
        )
        return result.stdout.strip()

    def write(self, name, contents):
        (self.repo / name).write_text(contents)

    def commit(self, message):
        self.git('add', '-A')
        self.git('commit', '-m', message)

    def review(self):
        return {
            'remote': self.remote,
            'head': self.git('rev-parse', 'HEAD'),
            'reason': 'User requested removal of the retired mode; exact diff reviewed.',
            'files': {'retired.rs': {
                'before': self.git('rev-parse', f'{self.remote}:retired.rs'),
                'after': None,
            }},
        }

    def run_guard(self, review=None):
        env = dict(self.env)
        if review is not None:
            path = self.root / 'review.json'
            path.write_text(json.dumps(review))
            env['PUSH_REMOVAL_REVIEW'] = str(path)
        return subprocess.run(['sh', str(PUSH)], cwd=self.repo, env=env, text=True, capture_output=True)

    def test_removal_without_review_fails(self):
        result = self.run_guard()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('retired.rs', result.stderr)

    def test_exact_review_accepts_only_dry_run(self):
        result = self.run_guard(self.review())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('reviewed removal', result.stderr)
        self.assertEqual(self.git('rev-parse', f'origin/{BRANCH}'), self.remote)

    def test_unreviewed_live_removal_still_fails(self):
        self.write('live.rs', 'unrelated replacement statement\n')
        self.commit('unreviewed stale-tree loss')
        result = self.run_guard(self.review())
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('live.rs', result.stderr)

    def test_changed_head_or_remote_invalidates_review(self):
        for key in ['head', 'remote']:
            with self.subTest(key=key):
                review = self.review()
                review[key] = '0' * 40
                result = self.run_guard(review)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('invalid removal review', result.stderr)

    def test_changed_blobs_invalidate_review(self):
        for key in ['before', 'after']:
            with self.subTest(key=key):
                review = self.review()
                review['files']['retired.rs'][key] = '0' * 40
                result = self.run_guard(review)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('reviewed blobs do not match', result.stderr)

    def test_reason_and_files_are_required(self):
        for key, value in [('reason', ''), ('files', {})]:
            with self.subTest(key=key):
                review = self.review()
                review[key] = value
                self.assertNotEqual(self.run_guard(review).returncode, 0)

    def test_missing_source_cannot_be_reviewed(self):
        review = self.review()
        review['files']['absent.rs'] = {'before': None, 'after': None}
        self.assertNotEqual(self.run_guard(review).returncode, 0)


if __name__ == '__main__':
    unittest.main()
