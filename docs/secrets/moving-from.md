# Moving to roll secrets from another manager

This guide moves your secrets into roll secrets from Doppler, Infisical, Vault or OpenBao, AWS
Secrets Manager, 1Password, Kubernetes Secrets, SOPS files, CI variables or `.env` files, and then
switches your apps over.

The rules of every import (limits, key names, what's left out and why) are in the reference:
["Moving from another manager"](../secrets.md#moving-from-another-manager).

## Before you start

1. Install the CLI, log in, and set up the project. See [Getting started](getting-started.md),
   steps 1 to 3.
2. Make the environments you import into, or let the import make them with
   `--create-environments`.
3. Sign in to the old manager the way you already do: its token in its usual variable, or its own
   CLI. The CLI reads the old manager on your machine. roll's server never sees that token.

Every import works the same way:

- **Try it first.** `--dry-run` prints the folders and keys it would import, never a value, and
  writes nothing. Run it before every real import.
- **Nothing half-done.** The import reads everything before it writes anything. If one secret
  can't be read, the import stops, names it, and writes nothing.
- **One change per folder.** You can roll each change back. A protected environment refuses the
  import, or opens a change request with `--request`.
- **Existing keys stay.** A key the folder already has is skipped and named. Add `--overwrite` to
  replace it.
- **Secret for good, if you ask.** `--sensitive` tags what the old manager kept secret
  `sensitive`: no person or token can read it back, and the tag never comes off. See ["Sensitive
  keys"](../secrets.md#sensitive-keys).
- **Check the copy.** After an import with `--from`, run the same command with `--check` instead.
  See ["Check the copy"](#check-the-copy).

## Doppler

Use a token that can read every config you import: a personal token or a service account token.
A service token reads only its own config.

```sh
export DOPPLER_TOKEN=dp.pt…
roll secrets import --from doppler --source shop --all-environments \
  --map dev=development,stg=staging,prd=production --create-environments --dry-run
```

Check the output, then run the same command without `--dry-run`. To import one config only:

```sh
roll secrets import --from doppler --source shop/prd -e production
```

- Doppler's references (`${KEY}`, `${CONFIG.KEY}`) stay references in roll. A reference to another
  project comes in as its current value, and the import names the key.
- A key's note becomes its comment.
- Doppler's own `DOPPLER_*` keys are left out.

For the full mapping, see ["Doppler and Infisical"](../secrets.md#doppler-and-infisical-structure-references-and-comments).

Without an API token, export a config and import the file:

```sh
doppler secrets download --no-file --format json > prod.json
roll secrets import prod.json -e production
```

Then switch over: `doppler run -- <command>` becomes `roll run -- <command>`, and `doppler.yaml`
becomes a `[secrets]` table in `roll.toml` ([Getting started](getting-started.md), step 3).

## Infisical

Use a token that can read the project (`INFISICAL_TOKEN`). For a self-hosted Infisical, also set
`INFISICAL_API_URL`.

```sh
export INFISICAL_TOKEN=…
roll secrets import --from infisical --source <workspace id> --all-environments --recursive \
  --map dev=development,prod=production --dry-run
```

- `--recursive` brings every folder, each into a folder of its own in roll.
- Infisical's secret imports become folder imports in roll ("Imports"). A replication is reported
  and left out, because it copies values.
- References (`${KEY}`, `${env.folder.KEY}`) stay references. A key's comment stays its comment.
- Personal overrides are left out.

To import one environment and folder:

```sh
roll secrets import --from infisical --source <workspace id>/prod/api -e production
```

Without an API token:

```sh
infisical export --env=prod --format=json > prod.json
roll secrets import prod.json -e production
```

Then switch over: `infisical run -- <command>` becomes `roll run -- <command>`.

## Vault and OpenBao

The import reads a KV version 2 secrets engine with `VAULT_ADDR` and `VAULT_TOKEN`:

```sh
export VAULT_ADDR=https://vault.example VAULT_TOKEN=hvs…
roll secrets import --from vault --source secret/shop --recursive -e production --dry-run
```

- `--source` is the mount and the path (`secret/shop`).
- `--recursive` walks the tree under it. Each secret goes into its own folder: `secret/shop/db`
  becomes `/db`.
- The token needs read on each secret and list on each folder.

**OpenBao** serves Vault's API, so set `VAULT_ADDR` and `VAULT_TOKEN` to your OpenBao server and
token (what you have in `BAO_ADDR` and `BAO_TOKEN`). The import is checked against Vault 1.20, not
against OpenBao.

Transit keys move too, if Vault lets you export them. Import each key with `roll secrets transit
import NAME [--vault-version N]`, the key on stdin. See ["Encryption keys"](../secrets.md#encryption-keys).

Then switch over:

| In Vault | In roll |
| --- | --- |
| Vault Agent's cache on `127.0.0.1` | [the local agent](platforms.md#a-vm-or-a-server) |
| Vault Agent templates, consul-template | `roll secrets render config.tpl --out config.yml --watch --exec CMD` |
| The database engine's dynamic credentials | dynamic [links](../secrets.md#links) |
| The AWS engine | [AWS roles](../secrets.md#aws) |
| Kubernetes auth, JWT auth | [workload identities](../secrets.md#workload-identities-oidc) |

## AWS Secrets Manager

The import runs your own `aws` CLI, so it uses your profile, SSO login or role:

```sh
roll secrets import --from aws --source prod/shop/ -e production --dry-run
```

- It reads every secret whose name starts with `--source`.
- A JSON secret's fields become keys. Any other secret becomes one key named after its last
  segment: `prod/shop/sendgrid-key` becomes `SENDGRID_KEY`.
- Binary secrets are left out, and the import names them.

Parameter Store imports the same way, with `--from aws-parameter-store --source /prod/shop`.

Then switch over:

- A Lambda function: replace AWS's Parameters and Secrets extension with roll's. See [AWS
  Lambda](platforms.md#aws-lambda).
- A server or a container that used the Secrets Manager Agent: use roll's local agent. See [A VM or a
  server](platforms.md#a-vm-or-a-server). Both listen on port 2773, but the paths differ.
- IAM access keys kept as secrets: let roll issue short-lived ones from a role instead. See
  ["AWS"](../secrets.md#aws).

## 1Password

The import runs your own `op` CLI. Sign in first (`op signin`), or set
`OP_SERVICE_ACCOUNT_TOKEN`.

```sh
roll secrets import --from 1password --source Prod -e production --dry-run
```

- `--source Prod` imports every item of the vault. Each field becomes `ITEM_FIELD`: the item
  `Main DB` and its field `password` become `MAIN_DB_PASSWORD`.
- `--source "Prod/Main DB"` imports one item, each field under its own name.
- Notes and one-time passwords are left out.
- Add `--sensitive` to tag concealed fields `sensitive`.

Then switch over. A file of `op://` references becomes a file of `roll://` references:

```sh
# before: op run --env-file .env -- npm start
DB_PASSWORD=op://Prod/Main DB/password
# after: roll run --env-file .env -- npm start
DB_PASSWORD=roll://production/MAIN_DB_PASSWORD
```

See ["References in files"](../secrets.md#references-in-files).

## Kubernetes Secrets

A Secret's `data` holds base64. Decode it with `jq` before the import, or roll stores the base64
text:

```sh
kubectl get secret shop -n prod -o json | jq '.data | map_values(@base64d)' \
  | roll secrets import - -e production
```

- `-` reads the import from stdin. A JSON object of `KEY: value` is detected; there's no format flag.
- A roll key is a letter or `_`, then letters, digits and `_`. Keys such as `tls.crt` or
  `api-key` are refused, and nothing is written. Rename them in the same `jq`:
  `jq '.data | map_values(@base64d) | with_entries(.key |= (ascii_upcase | gsub("[^A-Z0-9_]"; "_")))'`.
- A value that isn't text (a keystore, a binary certificate) doesn't decode cleanly. Leave it out
  with `del(.data["keystore.jks"])`, or keep it base64 under a key of its own.
- Do one Secret at a time, into the folder it belongs in (`--path /api`).

Then switch over: [Kubernetes](platforms.md#kubernetes) shows how pods get their values from roll.
After every app reads from roll, delete the old Secrets.

## SOPS files

Decrypt with your own key (age, PGP, KMS) and pipe it into the import:

```sh
sops -d --output-type dotenv secrets.enc.yaml | roll secrets import - -e production
```

A file with nested keys has no dotenv form. Decrypt it to JSON, and pick one level of it with `jq`:

```sh
sops -d --output-type json secrets.enc.yaml | jq '.production' | roll secrets import - -e production
```

The decrypted text goes only through the pipe, never to the disk. Then compare and clean up as for
[`.env` files](#env-files), and delete the encrypted file from the repository when nothing reads
it.

## CI variables

GitHub Actions secrets and GitLab CI/CD variables can't be exported: their APIs never return a
value. Choose one way:

- **Enter them again.** `roll secrets set KEY=…`, or the dashboard. For a few keys, this is the
  fastest way.
- **Run a job once that imports them.** Make a token that can only write the one environment, and
  pipe the variables from a job into the import:

  ```sh
  roll secrets tokens create ci-import --for-env staging --action read --action write --ttl-days 1
  ```

  ```yaml
  # .github/workflows/move-to-roll.yml: run it once, then delete it and the token.
  on: workflow_dispatch
  jobs:
    move:
      runs-on: ubuntu-latest
      steps:
        - run: curl -fsSL https://roll.sh/install.sh | sh
        - run: printf 'STRIPE_KEY=%s\nSENDGRID_KEY=%s\n' "$STRIPE_KEY" "$SENDGRID_KEY" | ~/.local/bin/roll secrets import - -p prj_… -e staging
          env:
            ROLL_API_URL: https://roll.example.com
            ROLL_TOKEN: ${{ secrets.ROLL_IMPORT_TOKEN }}
            STRIPE_KEY: ${{ secrets.STRIPE_KEY }}
            SENDGRID_KEY: ${{ secrets.SENDGRID_KEY }}
  ```

  A token names the project by its id (`prj_…`), since it can't look a name up: `roll secrets
  setup` prints it, and keeps it in `~/.config/roll/secrets.json`. GitLab is the same: a manual job
  whose script pipes its variables into `roll secrets import -`.
  The values go from the runner to roll and are never printed. Revoke the token after the job:
  `roll secrets tokens ls` lists its id, and `roll secrets tokens revoke <id>` ends it.

Then switch the pipelines to roll: [Platforms](platforms.md) shows a workload identity for each CI
system, so no roll token is kept in its settings either.

## `.env` files

Import each file into its environment:

```sh
roll secrets import .env -e development
roll secrets import .env.production -e production
```

The file can be a `.env` file or JSON (an object of `KEY: value`, or an array of `{key, value,
comment}`). A platform that exports dotenv files works the same way:

```sh
vercel env pull .env.production --environment=production
roll secrets import .env.production -e production
```

Then:

1. Check the copy against roll: `roll secrets diff --file .env` compares by fingerprint and never
   prints a value.
2. Delete the file. Tools that need a `.env` file can read one that never touches the disk. See
   [Local development](platforms.md#local-development).
3. Check that no value is still in the repository: `roll secrets scan .`.

## Other managers

The import also reads Bitwarden Secrets Manager (`--from bitwarden`), Google Secret Manager (`--from
gcp`), Azure Key Vault (`--from azure`) and Akeyless (`--from akeyless`), each through its own CLI.
See the table in ["Moving from another manager"](../secrets.md#moving-from-another-manager).

## Check the copy

Run the import again with `--check`. It reads the source as the import does, and compares it with
the environment by fingerprint. It writes nothing and never prints a value:

```sh
roll secrets import --from doppler --source shop/prd -e production --check
```

```
production:/
  - SENDGRID_KEY                     missing here
  ~ STRIPE_KEY                       different here
  + LOG_LEVEL                        only here
  41 the same
```

- `-` is a key the source has and roll doesn't. `~` is a key whose value differs. `+` is a key only
  roll has, such as one you added after the import.
- It exits 1 while a key is missing or different, so a script can wait for a clean check. It
  exits 2 when every key it compared matches but some couldn't be compared (below): check those by
  hand before you switch over. 0 means every key was compared and matches.
- `--map`, `--all-environments` and `--recursive` check every environment and folder they import.
- A value with references in roll, a linked key and a sensitive key you can't read are listed, not
  compared.

For a file, `roll secrets diff --file prod.json` does the same.

## Switch over: move each app, then revoke

roll doesn't sync back into Doppler, Infisical or Vault, so the two managers can't stay in step.
Don't run them side by side. Move one app at a time instead:

1. Import the values that app reads, and check the copy (`--check`).
2. Change how the app starts (`roll run`, the [local agent](platforms.md#a-vm-or-a-server), its
   CI's [workload identity](platforms.md)), and deploy it.
3. Change values in roll only, from now on. A change made in the old manager after the import isn't
   copied. Run `--check` again before the next app.
4. When every app reads from roll, revoke the old manager's tokens, then delete its secrets.

roll can still push values to platforms that read their own store: AWS, Google Cloud, Azure, GitHub
Actions, Vercel and others. See [Syncs](../secrets.md#syncs).

## After the move

- Give CI and services their values: [Platforms](platforms.md).
- Push values to the platforms you deploy to: [Syncs](../secrets.md#syncs).
- Protect production: [Getting started](getting-started.md), step 7.
