# -*- coding: utf-8 -*-
"""What the lookup and the module share: a client from the Python client (``roll_secrets``,
sdk/python/roll_secrets), reads without dynamic credentials, and one key's writes. Nothing here
puts a value in a result, a message or an exception."""

from __future__ import absolute_import, division, print_function

__metaclass__ = type

import hmac
import traceback

try:
    import roll_secrets

    HAS_ROLL_SECRETS = True
    ROLL_SECRETS_IMPORT_ERROR = None
except ImportError:
    roll_secrets = None
    HAS_ROLL_SECRETS = False
    ROLL_SECRETS_IMPORT_ERROR = traceback.format_exc()


class ConfigError(Exception):
    """Settings that can't make a client. Never holds a value or a token."""


def connect(api_url, token=None, project=None, identity=None, oidc_token=None, oidc_token_file=None, cache_ttl=0.0, timeout=30.0):
    """A ``roll_secrets.Client`` as ``roll_secrets.from_env`` makes one, from explicit settings:
    a roll token, or a workload identity's login with the workload's own OIDC token (given, read
    from a file for each login, or GitHub Actions')."""
    if not api_url:
        raise ConfigError("set api_url (or ROLL_API_URL)")
    if token:
        return roll_secrets.Client(api_url, token, project=project or None, timeout=timeout, cache_ttl=cache_ttl)
    if not identity:
        raise ConfigError("set token (ROLL_TOKEN), or identity (ROLL_IDENTITY) for a workload identity")

    def source():
        if oidc_token:
            return oidc_token
        if oidc_token_file:
            with open(oidc_token_file, encoding="utf-8") as f:
                return f.read()
        # GitHub Actions' token, as from_env asks for it (ROLL_OIDC_AUDIENCE, default the API's URL).
        return roll_secrets.oidc_token_from_env(api_url)

    # Called for each login, so a projected token replaced on disk is read afresh, as from_env does.
    return roll_secrets.login_with_identity(api_url, identity, source, project=project or None, timeout=timeout, cache_ttl=cache_ttl)


def read_values(client, environment, path="/", dynamic=False):
    """``environment``'s values under ``path``, references resolved. Without ``dynamic`` the server
    issues no dynamic credential (``dynamic=false``): those keys are left out and named in
    ``Values.dynamic``, since nothing renews a playbook's lease. The client's cache applies
    either way."""
    return client.values(environment, path, dynamic=dynamic)


def _own(client, environment, key, path):
    """The key's record in this folder, or None when it isn't the environment's own (absent, or only
    showing through a base or an import)."""
    s = client.secret(environment, key, path or "/")
    if s is None or s.source.kind != "own":
        return None
    return s


def _would_set(client, environment, key, value, path, comment):
    """Check mode: whether setting ``value`` would change the key. Compares the stored value
    (``resolve=false``, references as written; ``reveal``) here, in constant time, and keeps it
    nowhere. Returns (changed, warning)."""
    s = _own(client, environment, key, path)
    if s is None or s.link_kind:
        return True, None
    if comment is not None and comment != s.comment:
        return True, None
    try:
        current = client.stored_value(environment, key, path or "/")
    except roll_secrets.SecretsError as e:
        if e.status == 403:
            return True, "the token can't reveal %s, so check mode can't tell whether its value differs" % key
        if e.code == "unresolved":
            return True, None
        raise
    return not hmac.compare_digest(current.encode("utf-8"), value.encode("utf-8")), None


def set_key(client, environment, key, value, path="/", comment=None, message=None, check_mode=False):
    """Sets ``key`` to ``value`` (a link on the key is replaced by the value). Unchanged is no
    change: the server writes nothing for the same value. Returns a result without the value."""
    out = {"changed": False, "key": key, "environment": environment, "path": path or "/", "warnings": []}
    if check_mode:
        out["changed"], warning = _would_set(client, environment, key, value, path, comment)
        if warning:
            out["warnings"].append(warning)
        return out
    w = client.set(environment, key, value, path=path, comment=comment, message=message)
    change, secret = w.get("change"), w.get("secret") or {}
    out["changed"] = change is not None
    if change:
        out["change"] = change.get("seq")
    if secret.get("version") is not None:
        out["version"] = secret["version"]
    out["warnings"].extend(w.get("warnings") or [])
    return out


def delete_key(client, environment, key, path="/", check_mode=False):
    """Deletes ``key`` from the folder; a key that isn't there (or only shows through a base or an
    import) is no change."""
    out = {"changed": False, "key": key, "environment": environment, "path": path or "/", "warnings": []}
    if check_mode:
        out["changed"] = _own(client, environment, key, path) is not None
        return out
    w = client.delete(environment, key, path=path)
    if not w["deleted"]:
        return out
    change = w.get("change")
    out["changed"] = change is not None
    if change:
        out["change"] = change.get("seq")
    return out


def describe(e):
    """An error's message for Ansible: roll's own message, status and code (they never carry a
    value), or the exception's type for anything else."""
    if roll_secrets is not None and isinstance(e, roll_secrets.SecretsError):
        return "roll secrets: %s (%s%s)" % (e, e.status, ", " + e.code if e.code else "")
    if isinstance(e, ConfigError):
        return "roll secrets: %s" % e
    if isinstance(e, (OSError, ValueError)):
        # urllib's connection errors and a missing OIDC token file: no value can be in them.
        return "roll secrets: %s: %s" % (type(e).__name__, e)
    return "roll secrets: %s" % type(e).__name__
