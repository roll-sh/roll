# roll secrets for Ansible

The `roll.secrets` collection brings roll secrets (docs/secrets.md) into playbooks, like
HashiCorp's `community.hashi_vault` and 1Password's `onepassword.connect`:

- the lookup `roll.secrets.secret` reads values from a folder of an environment, with references
  resolved;
- the module `roll.secrets.secret` sets one key's value or deletes the key, with check mode
  supported.

The collection is built on the Python client, `sdk/python/roll_secrets`. Install the client
wherever the collection's code runs: on the controller for the lookup, and on the host that runs
the module (usually `localhost`).

## Install

From a checkout of this repository:

```sh
pip install ./sdk/python/roll_secrets           # the Python client (roll-secrets)
ansible-galaxy collection build sdk/ansible     # roll-secrets-0.1.0.tar.gz
ansible-galaxy collection install roll-secrets-0.1.0.tar.gz
```

It needs ansible-core 2.16 or later.

## Connecting

Both plugins use the same settings as `roll run` and the Python client's `from_env`. You can pass
each one as an option, or set it in the environment:

| Option | Environment | |
| --- | --- | --- |
| `api_url` | `ROLL_API_URL` | The roll API. |
| `token` | `ROLL_TOKEN` | A roll token (`rlt_…`) with a rule on the project. |
| `project` | `ROLL_PROJECT` | The project. Optional when the token is limited to one project. |
| `identity` | `ROLL_IDENTITY` | A workload identity that logs in instead of a token. |
| `oidc_token` | `ROLL_OIDC_TOKEN` | The workload's own OIDC token, for `identity`. |
| `oidc_token_file` | `ROLL_OIDC_TOKEN_FILE` | A file holding that token (a Kubernetes projected token). The file is read at each login. |

If you set `identity` but give no OIDC token, the plugins use the job's token from GitHub Actions
(`permissions: id-token: write`). Its audience is `ROLL_OIDC_AUDIENCE`, which defaults to the
API's URL. This way no long-lived token sits in CI.

## Reading values

```yaml
- name: Configure the API
  ansible.builtin.template:
    src: api.env.j2
    dest: /etc/api.env
    mode: "0600"
  vars:
    db_url: "{{ lookup('roll.secrets.secret', 'DB_URL', environment='production', path='/api', project='shop') }}"
    # The whole folder as a dictionary:
    api: "{{ lookup('roll.secrets.secret', environment='production', path='/api', as_dict=true) }}"
  no_log: true
```

- **One key, several keys, or the whole folder.** Each key you name returns one value, in order.
  `as_dict=true` returns a dictionary of the keys you name. With `as_dict=true` and no key, it
  returns every key in the folder.
- **Dynamic credentials are left out by default** (`dynamic=false`). A dynamic credential is a key
  linked in `dynamic` mode: each read issues a new credential, and the credential ends when its
  lease ends. A playbook can't renew a lease, so by default the plugin asks roll not to issue these
  credentials. Their keys are missing from the folder, and naming one is an error. With
  `dynamic=true` the lookup includes them. Use that only for work that finishes before the
  credential's TTL runs out.
- **Values are cached in memory for `cache_ttl` seconds (default 30).** During that time, reads of
  the same folder are answered from memory. After that, the plugin asks roll what changed since
  (the folder's digest and each key's stamp): nothing while nothing has changed, only the changed
  keys after a change. Ansible templates each task in a
  separate worker process, so the cache covers the lookups of one task (several keys of one folder,
  or a loop), not the whole play. A workload identity also logs in once per worker. The plugin
  never writes a value to disk.
- **Errors never include a value.** An error names the key and gives roll's message: a missing
  key, a reference that doesn't resolve, a refused token.

## Setting and deleting keys

```yaml
- name: Store the generated password
  roll.secrets.secret:
    environment: staging
    path: /db
    key: DB_PASSWORD
    value: "{{ generated_password }}"
    comment: made by the provisioning playbook
  delegate_to: localhost

- name: Remove a flag
  roll.secrets.secret:
    environment: staging
    key: OLD_FLAG
    state: absent
  delegate_to: localhost
```

- **The module is idempotent.** Setting the value a key already has writes nothing and reports
  `changed: false`. So does deleting a key that doesn't exist. A key that only shows through a
  base environment or an import doesn't belong to the folder, so the module never deletes it.
- **Check mode writes nothing.** To predict the result, the module compares the stored value
  (references as written) with the new one. The comparison runs on the host that runs the module,
  so the token needs `reveal`. Without `reveal`, check mode reports a change and warns that it
  couldn't compare.
- **The module never returns the value.** `value`, `token` and `oidc_token` are `no_log`. The
  result has the key, the environment, the folder, the key's `version`, and the `change` number
  when the module wrote something.
- **A protected environment fails the task.** A write there needs an approved change request, so
  the task fails with roll's `approval_required`.
- `change_message` sets the change's message in the environment's history.

## Keeping values out of output

- The lookup marks every value it returns as unsafe, so Ansible never templates a value, even one
  that holds `{{ … }}`.
- A lookup's result can't be marked `no_log`. Set `no_log: true` on any task whose arguments or
  result would print a value, such as `set_fact`, `debug`, or a module that echoes its input.

## Tests

The tests run the lookup and the module's writes in process. They also build and install the
collection with `ansible-galaxy`, then run `tests/playbook.yml` with `ansible-playbook` at `-vvv`.
The playbook reads values, sets a key twice, checks that the second set doesn't change anything,
and deletes the key. The test then checks that no value appears in Ansible's output. Every test
runs against the fake rolld in `tests/fake_rolld.py`.

```sh
pip install ansible-core ./sdk/python/roll_secrets
cd sdk/ansible && python -m unittest discover -s tests
```

CI's `sdk` job runs these tests. `ansible-test sanity --requirements` also passes when you run it
from a copy at `ansible_collections/roll/secrets`. The only entries in `tests/sanity/ignore-*.txt`
are the GPLv3 license header checks.
