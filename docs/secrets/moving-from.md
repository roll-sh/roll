# Moving to roll secrets from another manager

This guide moves your secrets into roll secrets from Doppler, Infisical, Vault or OpenBao, AWS
Secrets Manager, 1Password, or `.env` files, and then switches your apps over.

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

## After the move

- Give CI and services their values: [Platforms](platforms.md).
- Push values to the platforms you deploy to: [Syncs](../secrets.md#syncs).
- Protect production: [Getting started](getting-started.md), step 7.
- When your apps read from roll, revoke the old manager's tokens and delete its secrets.
