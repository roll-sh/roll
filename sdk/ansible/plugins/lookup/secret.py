# -*- coding: utf-8 -*-

from __future__ import absolute_import, division, print_function

__metaclass__ = type

DOCUMENTATION = r"""
name: secret
author: roll (@roll-sh)
short_description: Read values from roll secrets
description:
  - Returns the values of keys in one folder of a roll secrets environment, references resolved,
    as C(roll run) gives them to a process.
  - With O(as_dict=true) and no key, returns the whole folder as a dictionary.
  - Built on the Python client C(roll_secrets) (C(pip install roll-secrets)), which must be
    installed where Ansible runs.
  - Values are marked unsafe, so Ansible never templates them. A lookup's result can't be marked
    C(no_log), so set C(no_log=true) on any task whose arguments or result print a value.
  - Keys holding a dynamic credential (a lease) are left out by default and no credential is issued
    for them, since nothing renews a lease during a play. See O(dynamic).
  - Answers are kept in memory for O(cache_ttl) seconds, then revalidated by digest (a 304 while
    unchanged). Ansible templates each task in its own worker process, so the cache spans the
    lookups of one task (several keys of a folder, a loop), not the whole play. Nothing is ever
    written to disk.
options:
  _terms:
    description: The keys to read. None with O(as_dict=true) returns the whole folder.
    type: list
    elements: str
    required: false
  environment:
    description: The environment (C(production), C(staging), a preview's slug).
    type: str
    required: true
  path:
    description: The folder.
    type: str
    default: /
  project:
    description: The project's id. Optional when the token is limited to one project.
    type: str
    env:
      - name: ROLL_PROJECT
  api_url:
    description: The roll API's URL.
    type: str
    env:
      - name: ROLL_API_URL
  token:
    description: A roll token (C(rlt_...)) with C(read) and C(reveal) on the environment.
    type: str
    env:
      - name: ROLL_TOKEN
  identity:
    description:
      - A workload identity (C(idn_...)) to log in with instead of a token, using the workload's own
        OIDC token from O(oidc_token), O(oidc_token_file) or GitHub Actions
        (C(permissions.id-token=write); audience C(ROLL_OIDC_AUDIENCE), default the API's URL).
    type: str
    env:
      - name: ROLL_IDENTITY
  oidc_token:
    description: The workload's OIDC token, for O(identity).
    type: str
    env:
      - name: ROLL_OIDC_TOKEN
  oidc_token_file:
    description: A file holding the workload's OIDC token (a Kubernetes projected token), read at each login.
    type: path
    env:
      - name: ROLL_OIDC_TOKEN_FILE
  as_dict:
    description:
      - Return one dictionary of key to value instead of a list of values. With no key, every key
        in the folder.
    type: bool
    default: false
  dynamic:
    description:
      - Include keys that hold a dynamic credential. Each read issues a fresh credential with a
        lease that ends with its link's TTL; a playbook doesn't renew it, so use one only for work
        that finishes within the TTL.
      - When false, those keys are left out and asking for one is an error.
    type: bool
    default: false
  cache_ttl:
    description: Seconds to answer repeated reads of a folder from memory before revalidating it. 0 reads every time.
    type: float
    default: 30
  timeout:
    description: Seconds to wait for roll.
    type: float
    default: 30
notes:
  - An error names the key and roll's message, never a value.
seealso:
  - module: roll.secrets.secret
"""

EXAMPLES = r"""
- name: One value
  ansible.builtin.set_fact:
    db_url: "{{ lookup('roll.secrets.secret', 'DB_URL', environment='production', path='/api', project='shop') }}"
  no_log: true

- name: Several keys of one folder (one read)
  ansible.builtin.template:
    src: app.env.j2
    dest: /etc/app.env
    mode: "0600"
  vars:
    api: "{{ lookup('roll.secrets.secret', 'DB_URL', 'STRIPE_KEY', environment='production', path='/api', wantlist=true) }}"
  no_log: true

- name: The whole folder as a dictionary
  ansible.builtin.copy:
    content: "{{ lookup('roll.secrets.secret', environment='staging', as_dict=true) | to_nice_json }}"
    dest: /etc/app/secrets.json
    mode: "0600"
  no_log: true
"""

RETURN = r"""
_raw:
  description:
    - The values of the keys asked for, in order; or with O(as_dict=true) one dictionary of key to value.
  type: list
  elements: raw
"""

from ansible.errors import AnsibleLookupError
from ansible.module_utils.common.text.converters import to_native
from ansible.plugins.lookup import LookupBase
from ansible.utils.unsafe_proxy import wrap_var

from ansible_collections.roll.secrets.plugins.module_utils.roll import (
    HAS_ROLL_SECRETS,
    ROLL_SECRETS_IMPORT_ERROR,
    connect,
    describe,
    read_values,
)

# One client per connection in this process: its cache (and a workload identity's login) is shared
# by every lookup the process runs.
_CLIENTS = {}


def _client(o):
    settings = (o["api_url"], o["token"], o["project"], o["identity"], o["oidc_token"], o["oidc_token_file"], o["cache_ttl"], o["timeout"])
    c = _CLIENTS.get(settings)
    if c is None:
        c = connect(*settings)
        _CLIENTS[settings] = c
    return c


class LookupModule(LookupBase):
    def run(self, terms, variables=None, **kwargs):
        if not HAS_ROLL_SECRETS:
            raise AnsibleLookupError("roll.secrets.secret needs the roll_secrets Python client (pip install roll-secrets): %s" % ROLL_SECRETS_IMPORT_ERROR)
        self.set_options(var_options=variables, direct=kwargs)
        names = ("environment", "path", "project", "api_url", "token", "identity", "oidc_token", "oidc_token_file")
        names += ("as_dict", "dynamic", "cache_ttl", "timeout")
        o = {k: self.get_option(k) for k in names}
        if not o["environment"]:
            raise AnsibleLookupError("roll.secrets.secret: set environment")
        keys = [to_native(t) for t in terms or []]
        if not keys and not o["as_dict"]:
            raise AnsibleLookupError("roll.secrets.secret: name a key, or set as_dict=true for the whole folder")
        where = "%s:%s" % (o["environment"], o["path"] or "/")
        try:
            v = read_values(_client(o), o["environment"], o["path"] or "/", dynamic=o["dynamic"])
        except Exception as e:
            # from None: the cause isn't shown under the message.
            raise AnsibleLookupError("%s (reading %s)" % (describe(e), where)) from None

        problems = {p.key: p.problem for p in v.problems}
        if not keys:
            if problems:
                raise AnsibleLookupError("roll secrets: %s doesn't resolve: %s" % (where, "; ".join("%s: %s" % kp for kp in sorted(problems.items()))))
            return [wrap_var(dict(v.values))]
        found = []
        for k in keys:
            if k in problems:
                raise AnsibleLookupError("roll secrets: %s in %s doesn't resolve: %s" % (k, where, problems[k]))
            if k not in v.values:
                if k in v.dynamic and not o["dynamic"]:
                    raise AnsibleLookupError(
                        "roll secrets: %s in %s holds a dynamic credential; set dynamic=true to issue one"
                        " (its lease isn't renewed during the play)" % (k, where)
                    )
                raise AnsibleLookupError("roll secrets: no key %s in %s" % (k, where))
            found.append(v.values[k])
        if o["as_dict"]:
            return [wrap_var(dict(zip(keys, found)))]
        return [wrap_var(x) for x in found]
