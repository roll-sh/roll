"""The collection as Ansible finds it: sdk/ansible under ansible_collections/roll/secrets."""

import os
import pathlib
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent


def linked(tmp=None):
    """A collections directory holding this checkout's collection (a symlink); returns its path."""
    tmp = pathlib.Path(tmp or tempfile.mkdtemp(prefix="roll-ansible-"))
    ns = tmp / "ansible_collections" / "roll"
    ns.mkdir(parents=True, exist_ok=True)
    if not (ns / "secrets").exists():
        os.symlink(ROOT, ns / "secrets")
    return str(tmp)
