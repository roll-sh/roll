#!/usr/bin/python
# -*- coding: utf-8 -*-

from __future__ import absolute_import, division, print_function

__metaclass__ = type

DOCUMENTATION = r"""
module: secret
author: roll (@roll-sh)
short_description: Set or delete one key in roll secrets
description:
  - Sets one key's value in a folder of a roll secrets environment (O(state=present)), or deletes the
    key (O(state=absent)), as C(roll secrets set) and C(roll secrets delete) do.
  - Idempotent. Setting the value a key already has writes nothing, and deleting a key that isn't
    there is no change. A key that only shows through a base or an import isn't the folder's own.
  - Never returns the value. O(value) and the credentials are C(no_log).
  - Built on the Python client C(roll_secrets) (C(pip install roll-secrets)), which must be
    installed on the host the module runs on (usually C(localhost) with C(delegate_to)).
  - In a protected environment a write needs an approved change request; the module fails with
    roll's C(approval_required) instead.
version_added: 0.1.0
options:
  key:
    description: The key, such as C(DB_URL).
    type: str
    required: true
  value:
    description:
      - The value, required with O(state=present). It may hold references (C(${staging.DB_HOST})),
        kept as written. A link on the key is replaced by the value.
    type: str
  state:
    description: Whether the key holds O(value) or doesn't exist.
    type: str
    choices: [present, absent]
    default: present
  environment:
    description: The environment.
    type: str
    required: true
  path:
    description: The folder.
    type: str
    default: /
  comment:
    description: The key's comment. Left as it is when not given.
    type: str
  change_message:
    description: The change's message in the history.
    type: str
  project:
    description: The project's id (or C(ROLL_PROJECT)). Optional when the token is limited to one project.
    type: str
  api_url:
    description: The roll API's URL (or C(ROLL_API_URL)).
    type: str
  token:
    description:
      - A roll token (or C(ROLL_TOKEN)) with C(write) (and C(delete) for O(state=absent)) on the
        folder. Check mode compares the stored value, which needs C(reveal) too.
    type: str
  identity:
    description: A workload identity (or C(ROLL_IDENTITY)) to log in with instead of a token.
    type: str
  oidc_token:
    description: The workload's OIDC token for O(identity) (or C(ROLL_OIDC_TOKEN)).
    type: str
  oidc_token_file:
    description: A file holding the workload's OIDC token (or C(ROLL_OIDC_TOKEN_FILE)), read at each login.
    type: path
  timeout:
    description: Seconds to wait for roll.
    type: float
    default: 30
attributes:
  check_mode:
    support: full
    description: Compares the stored value without writing (needs C(reveal); without it, reports a change and warns).
  diff_mode:
    support: none
    description: A diff would show the value.
notes:
  - An error names the key and roll's message, never a value.
seealso:
  - plugin: roll.secrets.secret
    plugin_type: lookup
"""

EXAMPLES = r"""
- name: Set a key
  roll.secrets.secret:
    environment: production
    path: /api
    key: LOG_LEVEL
    value: info
  delegate_to: localhost

- name: Copy a generated password into roll
  roll.secrets.secret:
    environment: staging
    key: DB_PASSWORD
    value: "{{ generated_password }}"
    comment: made by the provisioning playbook
  delegate_to: localhost

- name: Delete a key
  roll.secrets.secret:
    environment: staging
    key: OLD_FLAG
    state: absent
  delegate_to: localhost
"""

RETURN = r"""
key:
  description: The key.
  returned: always
  type: str
environment:
  description: The environment.
  returned: always
  type: str
path:
  description: The folder.
  returned: always
  type: str
version:
  description: The key's version after the write.
  returned: when O(state=present) and not in check mode
  type: int
change:
  description: The change's sequence number in the environment's history, when there was one.
  returned: when changed and not in check mode
  type: int
"""

from ansible.module_utils.basic import AnsibleModule, env_fallback, missing_required_lib

from ansible_collections.roll.secrets.plugins.module_utils.roll import (
    HAS_ROLL_SECRETS,
    ROLL_SECRETS_IMPORT_ERROR,
    connect,
    delete_key,
    describe,
    set_key,
)


def main():
    module = AnsibleModule(
        argument_spec=dict(
            key=dict(type="str", required=True, no_log=False),
            value=dict(type="str", no_log=True),
            state=dict(type="str", choices=["present", "absent"], default="present"),
            environment=dict(type="str", required=True),
            path=dict(type="str", default="/"),
            comment=dict(type="str"),
            change_message=dict(type="str"),
            project=dict(type="str", fallback=(env_fallback, ["ROLL_PROJECT"])),
            api_url=dict(type="str", fallback=(env_fallback, ["ROLL_API_URL"])),
            token=dict(type="str", no_log=True, fallback=(env_fallback, ["ROLL_TOKEN"])),
            identity=dict(type="str", fallback=(env_fallback, ["ROLL_IDENTITY"])),
            oidc_token=dict(type="str", no_log=True, fallback=(env_fallback, ["ROLL_OIDC_TOKEN"])),
            oidc_token_file=dict(type="path", fallback=(env_fallback, ["ROLL_OIDC_TOKEN_FILE"])),
            timeout=dict(type="float", default=30.0),
        ),
        required_if=[("state", "present", ["value"])],
        supports_check_mode=True,
    )
    if not HAS_ROLL_SECRETS:
        module.fail_json(msg=missing_required_lib("roll-secrets"), exception=ROLL_SECRETS_IMPORT_ERROR)
    p = module.params
    result, error = None, None
    try:
        client = connect(p["api_url"], p["token"], p["project"], p["identity"], p["oidc_token"], p["oidc_token_file"], timeout=p["timeout"])
        if p["state"] == "present":
            result = set_key(client, p["environment"], p["key"], p["value"], p["path"], p["comment"], p["change_message"], module.check_mode)
        else:
            result = delete_key(client, p["environment"], p["key"], p["path"], module.check_mode)
    except Exception as e:
        error = describe(e)
    if error is not None:
        # Outside the except: Ansible attaches the exception being handled to a failure, and only
        # roll's message goes out.
        module.fail_json(msg="%s (%s in %s:%s)" % (error, p["key"], p["environment"], p["path"]))
    for w in result.pop("warnings"):
        module.warn(w)
    module.exit_json(**result)


if __name__ == "__main__":
    main()
