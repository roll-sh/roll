"""A pytest plugin for running ceph/s3-tests against roll (loaded with `-p roll_plugin`).

roll keeps a GOVERNANCE-locked version that was deleted with a bypass in the bucket's trash until
its lock ends, so the bucket can't be deleted until then (S3 destroys the version at once). The
suite's cleanup (`nuke_bucket`) raises on such a bucket before every later test of the same
worker, which would turn one deliberate difference into hundreds of setup errors.

This changes nothing else: the first time a bucket can't be deleted, the error is raised as usual
(it shows up on the test that left it); later cleanups skip that bucket.
"""
import s3tests.functional as functional

_leftover = set()


def pytest_configure(config):
    original = functional.nuke_bucket

    def nuke_bucket(client, bucket):
        if bucket in _leftover:
            return
        try:
            original(client, bucket)
        except Exception as e:
            if "object lock" in str(e).lower():
                _leftover.add(bucket)
            raise

    functional.nuke_bucket = nuke_bucket
