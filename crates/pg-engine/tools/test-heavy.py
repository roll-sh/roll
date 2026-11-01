#!/usr/bin/env python3
"""Exercise heavy.sh cache ownership with real concurrent processes and private slots."""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time


def wait_for(path, processes, seconds=10):
    deadline = time.monotonic() + seconds
    while not path.exists():
        if time.monotonic() >= deadline:
            raise AssertionError(f"timed out waiting for {path.name}")
        for process in processes:
            if process.poll() is not None:
                raise AssertionError(f"job exited early: {process.returncode}")
        time.sleep(0.02)


def check(script):
    with tempfile.TemporaryDirectory(prefix="heavy-check-") as directory:
        root = Path(directory)
        scripts = []
        for name in ("one", "two"):
            target = root / name / "crates/pg-engine/tools/heavy.sh"
            target.parent.mkdir(parents=True)
            shutil.copyfile(script, target)
            scripts.append(target)
        lock = root / "machine-lock"
        Path(str(lock) + ".slots").write_text("2\n")
        Path(str(lock) + ".jobs").write_text("1\n")
        env = dict(os.environ, HEAVY_LOCK=str(lock), HEAVY_MIN_FREE_GB="0",
                   HEAVY_MAX_INCREMENTAL_GB="0")
        sentinel = root / "one/target/debug/incremental/live-cache"
        release = root / "release"
        ready = root / "ready"
        same_started = root / "same-started"
        cross_started = root / "cross-started"
        holder = """
import pathlib, sys, time
sentinel, ready, release = map(pathlib.Path, sys.argv[1:])
sentinel.parent.mkdir(parents=True, exist_ok=True)
sentinel.write_text('live compiler data')
ready.touch()
while not release.exists():
    time.sleep(0.02)
assert sentinel.exists(), 'another job deleted the live cache'
"""
        processes = []
        log = (root / "jobs.log").open("w+")

        def launch(wrapper, *command):
            process = subprocess.Popen(["sh", str(wrapper), *command], env=env,
                                       stdout=log, stderr=log)
            processes.append(process)
            return process

        try:
            first = launch(scripts[0], sys.executable, "-c", holder,
                           str(sentinel), str(ready), str(release))
            wait_for(ready, [first])
            second = launch(scripts[0], "touch", str(same_started))
            cross = launch(scripts[1], "touch", str(cross_started))
            wait_for(cross_started, [first])
            cross.wait(timeout=10)
            # Give the second command time to run if the wrapper lacks serialization.
            deadline = time.monotonic() + 6
            while time.monotonic() < deadline:
                assert sentinel.exists(), "same-worktree invocation deleted the live cache"
                assert not same_started.exists(), "same-worktree commands overlapped"
                time.sleep(0.02)
            assert cross.returncode == 0, "different worktrees should share machine slots"
            release.touch()
            assert first.wait(timeout=10) == 0
            assert second.wait(timeout=10) == 0
            assert same_started.exists(), "waiting command did not resume"
            failure = launch(scripts[0], "sh", "-c", "exit 23")
            assert failure.wait(timeout=10) == 23, "command exit status was lost"
            print("PASS: cache protected; cross-worktree concurrency; waiter resumes; exit status")
        finally:
            release.touch()
            for process in processes:
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
            log.seek(0)
            print(log.read(), end="")
            log.close()


if __name__ == "__main__":
    check(Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).with_name("heavy.sh"))
