"""The collection as a user gets it: built and installed with ansible-galaxy, then a playbook run
with ansible-playbook (local connection) against the fake rolld, at -vvv, printing no value."""

import os
import shutil
import subprocess
import sys
import tempfile
import unittest

import collection
import fake_rolld
from fake_rolld import Rolld

NEW = "fake-new-value-from-ansible"


def run(*args, **kw):
    return subprocess.run(args, capture_output=True, text=True, timeout=120, check=False, **kw)


class APlaybook(unittest.TestCase):
    def test_reads_sets_idempotently_and_deletes_printing_no_value(self):
        api, server = fake_rolld.serve()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        tmp = tempfile.mkdtemp(prefix="roll-ansible-")
        self.addCleanup(shutil.rmtree, tmp, True)
        bindir = os.path.dirname(sys.executable)

        built = run(os.path.join(bindir, "ansible-galaxy"), "collection", "build", str(collection.ROOT), "--output-path", tmp)
        self.assertEqual(built.returncode, 0, built.stdout + built.stderr)
        tarball = os.path.join(tmp, "roll-secrets-0.1.0.tar.gz")
        installed = run(os.path.join(bindir, "ansible-galaxy"), "collection", "install", tarball, "-p", os.path.join(tmp, "collections"))
        self.assertEqual(installed.returncode, 0, installed.stdout + installed.stderr)

        env = {k: v for k, v in os.environ.items() if not k.startswith(("ROLL_", "ANSIBLE_"))}
        env.update(
            ROLL_API_URL=api,
            ROLL_TOKEN=fake_rolld.TOKEN,
            FAKE_NEW_VALUE=NEW,
            ANSIBLE_COLLECTIONS_PATH=os.path.join(tmp, "collections"),
            ANSIBLE_HOME=os.path.join(tmp, "home"),
            ANSIBLE_LOCAL_TEMP=os.path.join(tmp, "tmp"),
            ANSIBLE_REMOTE_TMP=os.path.join(tmp, "remote"),
            ANSIBLE_PYTHON_INTERPRETER=sys.executable,
            ANSIBLE_NOCOLOR="1",
            ANSIBLE_RETRY_FILES_ENABLED="0",
        )
        play = run(os.path.join(bindir, "ansible-playbook"), "-i", "localhost,", "-vvv", str(collection.ROOT / "tests" / "playbook.yml"), env=env, cwd=tmp)
        out = play.stdout + play.stderr
        self.assertEqual(play.returncode, 0, out[-6000:])
        for v in Rolld.secret_values() + [NEW]:
            self.assertNotIn(v, out, "a value reached Ansible's output")
        puts = [p for m, p in Rolld.state["requests"] if m == "PUT"]
        self.assertEqual(len(puts), 3, "check mode writes nothing: two sets and the protected environment's refusal")
        self.assertNotIn("NEW_KEY", Rolld.state["folders"][("production", "/api")])
        self.assertEqual(Rolld.state["leases"], 0)


if __name__ == "__main__":
    unittest.main()
