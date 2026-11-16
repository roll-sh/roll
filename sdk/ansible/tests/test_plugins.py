"""The lookup and the module's writes in process, against a fake rolld."""

import os
import shutil
import sys
import tempfile
import unittest
from unittest import mock

from ansible.errors import AnsibleLookupError
from ansible.parsing.dataloader import DataLoader
from ansible.plugins.loader import init_plugin_loader, lookup_loader

import collection
import fake_rolld
from fake_rolld import Rolld

API, SERVER, TMP = None, None, None


def setUpModule():
    global API, SERVER, TMP
    TMP = tempfile.mkdtemp(prefix="roll-ansible-")
    init_plugin_loader([collection.linked(TMP)])
    API, SERVER = fake_rolld.serve()


def tearDownModule():
    SERVER.shutdown()
    shutil.rmtree(TMP, ignore_errors=True)


def lookup(*terms, **options):
    plugin = lookup_loader.get("roll.secrets.secret", loader=DataLoader(), templar=None)
    return plugin.run(list(terms), variables={}, **options)


def values_reads():
    return [p for m, p in Rolld.state["requests"] if m == "GET" and "/values" in p]


class Base(unittest.TestCase):
    def setUp(self):
        Rolld.reset()
        sys.modules[type(lookup_loader.get("roll.secrets.secret")).__module__]._CLIENTS.clear()
        env = mock.patch.dict(os.environ, {"ROLL_API_URL": API, "ROLL_TOKEN": fake_rolld.TOKEN})
        for k in ("ROLL_PROJECT", "ROLL_IDENTITY", "ROLL_OIDC_TOKEN", "ROLL_OIDC_TOKEN_FILE"):
            os.environ.pop(k, None)
        env.start()
        self.addCleanup(env.stop)


class TheLookup(Base):
    def test_reads_keys_and_folders_without_issuing_dynamic_credentials(self):
        self.assertEqual(lookup("DB_URL", environment="production", path="/api", project="shop"), ["fake-db-url-not-a-secret"])
        self.assertEqual(lookup("DB_URL", "API_KEY", environment="production", path="/api", as_dict=True), [{"DB_URL": "fake-db-url-not-a-secret", "API_KEY": "fake-api-key-0000"}])
        # The whole folder, an inherited key with it; the dynamic one left out and never issued.
        (folder,) = lookup(environment="production", path="/api", as_dict=True)
        self.assertEqual(sorted(folder), ["API_KEY", "DB_URL", "FROM_BASE"])
        self.assertEqual(Rolld.state["leases"], 0)
        self.assertTrue(all("dynamic=false" in p for p in values_reads()))
        with self.assertRaisesRegex(AnsibleLookupError, "DB_PASSWORD in production:/api holds a dynamic credential"):
            lookup("DB_PASSWORD", environment="production", path="/api")
        # Asked for, a dynamic key is issued with its lease.
        self.assertEqual(lookup("DB_PASSWORD", environment="production", path="/api", dynamic=True), ["fake-dynamic-password-1"])
        self.assertEqual(Rolld.state["leases"], 1)

    def test_keeps_a_folder_within_its_ttl_then_revalidates_it(self):
        # (The client never keeps a folder that names a dynamic key, so this one holds none.)
        lookup("TEMPLATED", environment="production", path="/templates")
        lookup("OTHER", environment="production", path="/templates")
        self.assertEqual(len(values_reads()), 1, "one read for two keys of a folder")
        lookup("OTHER", environment="production", path="/templates", cache_ttl=0)
        self.assertEqual(len(values_reads()), 2, "cache_ttl=0 reads every time")

    def test_errors_name_the_key_never_a_value(self):
        cases = [
            (("NOPE",), {"path": "/api"}, "no key NOPE in production:/api"),
            (("REF",), {"path": "/broken"}, "REF in production:/broken doesn't resolve: references a key that isn't set"),
            ((), {"path": "/broken", "as_dict": True}, "production:/broken doesn't resolve: REF: references"),
            ((), {"path": "/api"}, "name a key, or set as_dict=true"),
        ]
        for terms, options, message in cases:
            with self.assertRaises(AnsibleLookupError) as e:
                lookup(*terms, environment="production", **options)
            self.assertIn(message, str(e.exception))
            for v in Rolld.secret_values():
                self.assertNotIn(v, str(e.exception))
        with mock.patch.dict(os.environ, {"ROLL_TOKEN": "rlt_fake_revoked"}):
            with self.assertRaises(AnsibleLookupError) as e:
                lookup("DB_URL", environment="production", path="/api")
        self.assertIn("no such token (401, unauthorized)", str(e.exception))
        self.assertNotIn("rlt_fake_revoked", str(e.exception))

    def test_a_workload_identity_logs_in_with_its_oidc_token_file(self):
        token = os.path.join(TMP, "oidc-token")
        with open(token, "w", encoding="utf-8") as f:
            f.write(fake_rolld.OIDC + "\n")
        os.environ.pop("ROLL_TOKEN")
        os.environ.update(ROLL_IDENTITY=fake_rolld.IDENTITY, ROLL_OIDC_TOKEN_FILE=token)
        # No project given: the login names the identity's.
        self.assertEqual(lookup("DB_URL", environment="production", path="/api"), ["fake-db-url-not-a-secret"])
        self.assertEqual(lookup("API_KEY", environment="production", path="/api"), ["fake-api-key-0000"])
        self.assertEqual(Rolld.state["logins"], 1, "one login per process")


class TheModule(Base):
    def setUp(self):
        super().setUp()
        from ansible_collections.roll.secrets.plugins.module_utils import roll

        self.roll = roll
        self.client = roll.connect(API, fake_rolld.TOKEN, "shop")

    def writes(self):
        return [m for m, _ in Rolld.state["requests"] if m in ("PUT", "DELETE")]

    def test_sets_idempotently_and_check_mode_writes_nothing(self):
        r = self.roll
        new = "fake-new-value-1111"
        self.assertTrue(r.set_key(self.client, "production", "NEW", new, "/api", check_mode=True)["changed"])
        self.assertEqual(self.writes(), [])
        out = r.set_key(self.client, "production", "NEW", new, "/api", comment="made by ansible")
        self.assertEqual((out["changed"], out["version"], out["change"]), (True, 1, 1))
        self.assertNotIn(new, repr(out))
        self.assertEqual(Rolld.state["folders"][("production", "/api")]["NEW"]["value"], new)
        again = r.set_key(self.client, "production", "NEW", new, "/api")
        self.assertEqual((again["changed"], again["version"]), (False, 1))
        self.assertFalse(r.set_key(self.client, "production", "NEW", new, "/api", comment="made by ansible", check_mode=True)["changed"])
        self.assertTrue(r.set_key(self.client, "production", "NEW", new, "/api", comment="other", check_mode=True)["changed"])
        self.assertTrue(r.set_key(self.client, "production", "NEW", "fake-other-value", "/api", check_mode=True)["changed"])
        # Without reveal, check mode can't compare: it says so and assumes a change.
        blind = r.connect(API, fake_rolld.WRITE_ONLY, "shop")
        out = r.set_key(blind, "production", "NEW", new, "/api", check_mode=True)
        self.assertTrue(out["changed"])
        self.assertIn("can't reveal NEW", out["warnings"][0])
        self.assertEqual(self.writes(), ["PUT", "PUT"])

    def test_deletes_only_the_folders_own_keys(self):
        r = self.roll
        self.assertFalse(r.delete_key(self.client, "production", "FROM_BASE", "/api", check_mode=True)["changed"])
        self.assertFalse(r.delete_key(self.client, "production", "FROM_BASE", "/api")["changed"])
        self.assertTrue(r.delete_key(self.client, "production", "DB_URL", "/api", check_mode=True)["changed"])
        self.assertIn("DB_URL", Rolld.state["folders"][("production", "/api")])
        self.assertTrue(r.delete_key(self.client, "production", "DB_URL", "/api")["changed"])
        self.assertFalse(r.delete_key(self.client, "production", "DB_URL", "/api")["changed"])

    def test_a_protected_environment_fails_without_the_value(self):
        with self.assertRaises(Exception) as e:
            self.roll.set_key(self.client, "locked", "K", "fake-locked-value")
        message = self.roll.describe(e.exception)
        self.assertIn("(409, approval_required)", message)
        self.assertNotIn("fake-locked-value", message)


if __name__ == "__main__":
    unittest.main()
