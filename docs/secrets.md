# roll secrets

roll secrets stores the secrets and environment variables of a workspace project
(docs/workspace.md). A project has environments (`development`, `staging`, `production` and any the
team adds), each environment has folders, and each folder has secrets. Every write is versioned, an
environment can be rolled back to any point in time, values can reference other values, and a secret
can be **linked** to a roll resource in the project (a Postgres branch, a Redis database, a bucket,
a stream, a search cluster…) so it stays correct when that resource changes or rotates.

Infisical is the reference for depth (projects, environments, folders, references, imports,
versions, point-in-time recovery, personal overrides, rotation, dynamic secrets, approvals, audit,
`infisical run`). roll does better where it hosts the resources itself: links instead of pasted
connection strings, rotation and short-lived credentials issued by the product, and preview
environments that inherit from a base environment instead of copying it.

- Code: `crates/roll-secrets` (store, crypto, references, router), mounted by
  `crates/rolld/src/secrets.rs`, which also implements the link providers; the sync kinds are
  `crates/rolld/src/secrets/syncs.rs`.
- Client: `apps/dashboard/src/lib/secrets-api.ts`. Change it and this contract together.
- CLI: `roll secrets …` and `roll run` in `crates/roll-cli/src/secrets.rs`.
- Tests: `crates/roll-secrets/tests/api.rs` (the router in process, on SQLite or Postgres),
  `crates/roll-secrets/tests/nodes.rs` (two stores racing on one database),
  `crates/roll-cli/tests/secrets.rs` (`roll run` against a live rolld),
  `apps/dashboard/src/lib/secrets-*.test.ts`.

## Getting started

Everything below works the same from the dashboard (a project's **Secrets**) and the CLI.

1. **Pick a project and its environments.** A workspace project has `development`, `staging` and
   `production` the first time you open its secrets. Add your own (`roll secrets envs add qa`) and
   remember the project for this directory:

   ```
   roll secrets setup -p shop -e development
   ```
2. **Add values**, one by one or from a `.env` file (one change, which you can roll back):

   ```
   roll secrets set STRIPE_KEY=sk_test_… LOG_LEVEL=debug
   roll secrets import .env
   roll secrets set DATABASE_URL='postgres://app:${PASSWORD}@${staging.DB_HOST}/shop'   # references
   ```
3. **Link roll resources** instead of pasting their credentials. A linked value follows the
   resource: a rotated password, a new key. In the dashboard, **Link a resource**: a Postgres
   branch's URL, a Redis URL, a bucket's own access key, a stream's API key, a search API key.
4. **Run with them.** Nothing is written to disk; `--watch` restarts on a change:

   ```
   roll run -- npm run dev
   roll run -e production --watch -- ./server
   ```
5. **Promote** what changed from one environment to the next, and **protect** production so writes
   become change requests someone else approves:

   ```
   roll secrets diff --from development --to staging
   roll secrets promote --from staging --to production STRIPE_KEY
   roll secrets envs protect production --approvals 1
   ```
6. **CI and services** get a scoped token for one project, some environments, some folders and
   only the actions they need, with an expiry:

   ```
   roll secrets tokens create ci --for-env staging --action read --action reveal --ttl-days 90
   ROLL_TOKEN=rlst_… roll run -e staging -- make test
   ```

   Or push an environment out with a **sync** (GitHub Actions secrets, a sealed webhook, a deploy
   notification). roll's own checks (`secrets = "development"` in `roll.toml`), previews, apps and
   sandboxes read their environment directly.
7. **From code**, a service reads its environment with `@roll/secrets` (`packages/secrets`) and the
   same token. `resolve` with `sealTo` returns the values sealed to a key only that process holds,
   and `watch` follows the digest and renews dynamic credentials' leases, as `roll run --watch` does:

   ```ts
   import { createClient, generateKeyPair } from "@roll/secrets";

   const secrets = createClient({ api: process.env.ROLL_API_URL!, token: process.env.ROLL_TOKEN! });
   const { values } = await secrets.values("staging");
   const keys = generateKeyPair();
   const env = secrets.open(await secrets.resolve("production", { consumer: "app:web", sealTo: keys.publicKey }), keys.secretKey);
   const stop = secrets.watch("staging", { onChange: (v) => reconfigure(v.values) });

   // An encryption key (below, "Encryption keys"): the key stays in roll.
   const cards = secrets.transit("cards");
   const ct = await cards.encrypt(cardNumber, `order:${id}`);
   const index = await cards.hmac(email); // a lookup column over encrypted emails
   ```

   In Go, `sdk/go/secrets` has the same surface, and `FromEnv` logs a workload identity in with
   its own OIDC token as `roll run` does (`ROLL_IDENTITY`):

   ```go
   c, err := secrets.FromEnv(ctx)
   err = c.Watch(ctx, "production", secrets.Watch{OnChange: func(v *secrets.Values) error { return reload(v.Values) }})
   ```

   In Python, `sdk/python/roll_secrets` (`roll-secrets`, the standard library but for sealed
   resolves, which need the `seal` extra):

   ```python
   c = roll_secrets.from_env()
   c.watch("production", lambda v: reload(v.values))
   ```
8. **When something goes wrong**: `roll secrets history`, `roll secrets rollback --at 2h`, the
   audit log (`roll secrets audit`, or streamed to your SIEM), and `roll secrets share KEY` to hand
   one value to someone outside roll through a link that works once.

## Terraform

`sdk/terraform-provider-roll` manages roll secrets as code, as Doppler's and Infisical's providers
do. It speaks the same HTTP API as the CLI and the clients, with a roll token or the operator's.

- **`roll_secrets_environment`** ensures an environment (`PUT …/environments/{env}`) and sets its name
  and whether it's protected. The policy is only written when `protected` changes, so approvers and
  a notify URL set in the dashboard stay. Destroying one passes its confirmation.
- **`roll_secret`** is one key in a folder: a value, or a `link` to a resource on the canvas. On
  refresh it reads the stored value back (`resolve=false`, references as written; the token needs
  `reveal`), so a change made outside Terraform shows in the plan. A key that only shows through a
  base or an import isn't the resource's. In a protected environment a write opens a change
  request instead, which Terraform reports as an error.
- **`roll_secrets_identity`** is a workload identity (CI's OIDC login) and **`roll_secrets_sync`** a
  sync to a platform. roll changes neither in place, so every attribute replaces them. roll makes
  identities for people and the operator only, never for a token. A sync's target is listed by host
  only, so a target changed outside Terraform isn't seen; its signing secret (webhooks) is shown
  once and kept, sensitive, in the state.
- **`roll_secrets_token`** is a token for CI or a service: a roll token with one rule on the
  project's secrets, made through `POST /v1/access/tokens` as the dashboard's New token. Its
  secret is in the state (sensitive). Any change replaces it, and destroying it revokes it.
- **`data.roll_secrets_values`** is an environment's resolved values, for another provider.

Values land in Terraform's state, as with every provider: keep the state as private as the values.
## GitHub Actions
`sdk/github-action` puts an environment into a job, as Doppler's and Infisical's actions do:
```yaml
permissions:
  id-token: write
steps:
  - uses: roll-sh/roll/sdk/github-action@main
    with:
      api-url: https://api.roll.example
      identity: idn_…          # a workload identity for this repo (roll secrets identities add ci --github owner/repo --for-env staging)
      environment: staging
  - run: make test            # DATABASE_URL and the rest are in the environment
```
- The job logs in with its own OIDC token (audience: the API's URL), so no token sits in the
  repository's settings; `token` takes a roll token instead.
- Every line of every value, the OIDC token and the roll token are masked (`::add-mask::`) before
  anything else is printed. Values are exported to `GITHUB_ENV` (or, with `export: false`, the
  step's outputs) inside a random heredoc delimiter no value contains, so a multi-line value
  (a PEM key) arrives whole and can't add variables of its own. A key that doesn't resolve fails
  the step, naming it.
- No dependencies: Node's own `fetch`, nothing to bundle or audit.
- **`scan: true`** guards a repository on GitHub the way roll code guards the ones it hosts: the
  lines a pull request adds (`git diff base...HEAD`; check out with `fetch-depth: 0`) are cut into
  candidates as `roll secrets scan` cuts them and checked with `POST …/leaks` (fingerprints; only
  values the token may reveal), and each stored value found fails the check with an annotation on
  its file and line naming the key and environment, never the value.
## Kubernetes
Two ways, as the best products offer:
- **Pull (`sdk/kubernetes-operator`)**: a `RollSecret` names an environment and a folder (and a
  project or an identity, when not the operator's own), and the operator keeps a Secret beside it
  in step. It logs in with its own service account token through a workload identity, so roll
  holds no credential for the cluster. It checks the digest every 30 s and reads values only when
  it moved; it writes only Secrets it made (labelled, owned by their RollSecret, so they go with
  it) and leaves a same-named Secret someone else made alone; values that don't resolve leave the
  Secret as it was. Its status says what happened.
- **Push (the `kubernetes` sync)**: roll writes a Secret with credentials for the cluster it holds,
  sealed. For clusters roll can reach and the operator can't run in.
## Customer-managed keys
A project's data key can be wrapped once more by a KMS key in the customer's own AWS account, as
Infisical's external KMS does (`PUT …/kms {key: "arn:aws:kms:…"}`, `roll secrets kms set ARN`;
admin). Opening it then needs both roll's deployment key and the customer's key, so the customer
can make the project's values unreadable to roll at once by revoking roll's grant or disabling the
key. The values themselves aren't re-encrypted; only the data key is wrapped.
- **The key policy** grants roll's role `kms:Encrypt` and `kms:Decrypt` with the conditions
  `kms:EncryptionContext:roll = secrets` and `kms:EncryptionContext:project = prj_…`. Setting a key
  encrypts and decrypts with it first, and also tries a made-up project: a key that would wrap any
  project's data key (another customer's, which roll's role may be allowed to use) is refused, so
  no tenant can bind their project to someone else's key.
- **Format**: `kms:v1:<key>:<deployment key id>:<ciphertext>`, wrapping the data key as roll's
  deployment key seals it; the KMS encryption context is `{roll: secrets, project: <id>}`, in the
  key's own region.
- **Revocation** takes effect within a minute: an unwrapped key (still sealed by the deployment
  key) is kept in memory for 60 s, so a request doesn't wait on KMS. After that the project answers
  423 `kms_denied`, naming the key and KMS's reason, never a value (and `roll run --fallback`
  doesn't start from its file on it); a KMS that doesn't answer is 503. A refusal is remembered as
  long, so a revoked project doesn't ask KMS at every request. Changing the wrapping (`PUT` or
  `DELETE …/kms`) always asks KMS, never the memory: a revoked key can't be swapped out.
- **Rotating the deployment key**: the id of the deployment key inside tells health which wrapped
  projects are still sealed by the previous one, and the sweeper reseals them (50 a pass), so the
  previous key is dropped only once health says `all`. A reseal KMS won't wrap leaves the project
  readable and is tried again. `DELETE …/kms` unwraps it (KMS must still answer). Every change is
  in the audit log (`project.kms`).
## AWS
Short-lived AWS credentials for each lease, as Vault's AWS engine and Infisical's dynamic secrets
give: a project registers a role in its own account (`POST …/aws-roles {name, role_arn}`,
`roll secrets aws-roles add`, the Integrations tab; admin), and three keys link to it in `dynamic`
mode (`access_key_id`, `secret_access_key`, `session_token`: one credential, so they share one
session per resolve). Each resolve assumes the role (STS AssumeRole with roll's own credentials) for
the lease's TTL plus five minutes (15 minutes to 12 hours, within the role's own maximum).
- **The trust policy** lets roll's role assume it with the condition `sts:ExternalId =
  roll-secrets:<project>`. Registering a role assumes it once, and first tries a made-up project's
  ExternalId: a role that takes any (another customer's that trusts roll, say) is refused, so no
  tenant can register and assume someone else's role.
- **Ending a lease** doesn't end its session early (STS can't revoke one session): it stays valid at
  AWS until it expires, the TTL plus five minutes at most. To cut every session at once, revoke the
  role's older sessions in IAM. A session can't be renewed: resolve again for a new one.
- A role keys still link to can't be deleted (409 `in_use`); deleting the project deletes its roles.

**IAM users' access keys**, rotated by roll as Infisical's AWS IAM rotation does, for what can't
use a role's sessions (a vendor that takes only a key pair, an old CI). A project registers an IAM
user next to one of its roles (`POST …/aws-users {name, role, user}`, `roll secrets aws-users add
NAME USER --role ROLE`, the Integrations tab; admin): roll acts as that role (with the project's
ExternalId), whose policy allows `iam:ListAccessKeys`, `iam:CreateAccessKey` and
`iam:DeleteAccessKey` on the user, and registering lists the user's keys to check. It's the link kind
`aws_iam_user`, `managed` (`access_key_id` and `secret_access_key`, one credential `key`).

- **Two key slots take turns.** Each issue (linking, every `rotate_days`, `…/rotate`) creates a key,
  and the key it replaces is deleted when its grace period ends.
- **Nothing is pruned to make room.** A user that already holds two keys is refused (409 `in_use`,
  naming both): one may be in use (a key of the customer's, the previous one still in its grace,
  another environment's). Rotating twice within one grace period is refused the same way.
- A user keys still link to, or with a retired key still to delete, can't be deleted, nor a role
  that users act as.

## Where it runs

roll secrets is always on, like workspace projects: it is not a product in `ROLL_PRODUCTS`.

- On a single node, the data is SQLite at `<home>/secrets.db`.
- With the system database (`ROLL_SYSTEM_DATABASE`), it is Postgres, in the schema `secrets`. Nodes
  share it without leases: every read-then-write is one `code_sql` write transaction, and write
  closures only touch the database (audit rows are written in the same transaction; anything
  outside the database, such as revoking a credential, happens after `write` returns).
- Background work (expiring ephemeral environments and leases, due rotations) runs on every node,
  and each item is claimed in a write before it is acted on, so two nodes never act on one item.

## Encryption

Values are encrypted at rest with envelope encryption:

```
ROLL_SECRET_KEY ──HKDF-SHA256(salt "roll-secrets", info "kek:v1")──▶ KEK (AES-256-GCM)
KEK seals each project's random 32-byte data key  (aad "roll-secrets:dek:<project>")
data key encrypts every value                     (aad "roll-secrets:v1:<project>:<secret id>:<version>")
```

- **The deployment key** is `ROLL_SECRET_KEY` (at least 32 characters), the key that already seals
  review keys, chat tokens and sandboxes. Without it, a single node generates a key file at
  `<home>/secrets.key` (mode 0600) and logs that it did. A node using the system database refuses to
  serve secrets without `ROLL_SECRET_KEY` (every route answers 503 and says why), because nodes must
  share the key.
- **The deployment key from AWS KMS.** Instead of the key itself, a node can hold it encrypted:
  `ROLL_SECRET_KEY_KMS` is the base64 `CiphertextBlob` of
  `aws kms encrypt --key-id <key> --plaintext fileb://key --encryption-context roll=secrets`.
  rolld decrypts it once at startup with the standard AWS credentials (an instance or task role on
  roll cloud), so the plaintext key is never in the environment or on disk, a stolen environment
  is useless without KMS access, and every startup is a CloudTrail event. The encryption context
  binds the ciphertext to roll secrets. Rotation works the same with `ROLL_SECRET_KEY_KMS_PREVIOUS`.
  If KMS can't decrypt it, every route answers 503 with KMS's error code (never key material), and
  health reports `"key": "kms"`.
- **Key rotation.** Set the new key as `ROLL_SECRET_KEY` and the old one as
  `ROLL_SECRET_KEY_PREVIOUS`. A data key that opens only with the previous key is resealed with the
  new key on first use, so the previous key can be dropped once `GET /v1/secrets/health` reports
  `"resealed": all`.
- **Values are bound to their row.** The associated data names the project, the secret's id and its
  version. A ciphertext copied to another row, project or version doesn't decrypt.
- **Fingerprints.** Each version stores `fp`, the first 8 bytes of HMAC-SHA256(fingerprint key,
  value), where the fingerprint key is derived from the project's data key. The overview grid uses
  fingerprints to flag environments whose values differ, without revealing any value. They reveal
  equality inside one project only.
- **Never in the clear outside a reveal.** Values never appear in logs, `Debug` output, errors,
  audit rows or change summaries. The `Value` type redacts itself in `Debug` and `Display` and is
  zeroized on drop. Error messages name keys, never values.
- **Pending values** (change requests waiting for approval) are encrypted the same way, with
  `roll-secrets:request:<request id>:<n>` as the associated data.

## Model

```
project (a workspace project, prj_…)
└─ environment (env_…)  slug, name, position, kind: standard | ephemeral
   ├─ base: another environment (ephemeral environments only)
   ├─ policy: protected, approvals, approvers
   └─ folder "/", "/api", "/api/stripe"
      ├─ imports: [{environment, path}]
      └─ secret (sec_…)  key, value, comment, tags, type (shared | personal), link
         └─ version 1, 2, 3 … (each made by a change)
```

### Environments

A project starts with `development`, `staging` and `production`, in that order, the first time
anything touches its secrets. Teams add, rename, reorder and delete their own environments.

- **Slug**: 1–32 characters of `[a-z0-9-]`, not starting or ending with `-`, unique in the project.
  References, the CLI and the URLs use the slug. **Renaming** an environment's slug rewrites every
  reference to it in the project (`${staging.X}` becomes `${qa.X}`) in the same transaction, as one
  change per environment that had a reference, so nothing breaks.
- **Name**: what the dashboard shows, 1–48 characters.
- **Position**: the order of the dashboard's columns and of promotion (dev → staging → prod).
- **Delete** removes the environment's folders, secrets, versions, requests and leases (leases are
  revoked first). A protected environment can only be deleted with `?confirm=<slug>`. The last
  standard environment can't be deleted.

### Ephemeral environments (previews)

roll previews (docs/previews.md) makes one environment per change or pull request. An **ephemeral**
environment is a first-class kind, built to be cheap:

- It has a **base** (a standard environment, e.g. `staging`). It stores only its overrides: a key it
  doesn't set is read from the base, at the same path, at read time. Creating one copies nothing, so
  creating or deleting hundreds is a few rows each.
- It carries **labels** (`{"change": "chg_42", "pr": "117"}`, keys and values of `[a-z0-9_.-]`, up
  to 16) and an optional **expiry**. It is deleted when it expires (its leases are revoked first),
  and `DELETE …/environments?label=pr=117` deletes every environment with that label in one call.
- One call creates the environment together with its first secrets and links. roll previews
  writes its branched resources' addresses (the preview's Postgres branch, its Redis branch) as
  plain values: a branch is the preview's own resource, not a link to a parent on the canvas.
- It is hidden from the overview grid's columns (the grid shows a "Previews" count instead), is
  never a promotion target, and can't be protected. Its slug is any free slug (`pr-117`).
- **Ensuring one** is a single idempotent call, `PUT …/environments/{env}` with what it should be
  (`{base?, kind?, name?, labels?, expires_at? | ttl_minutes?, trusted?, path?, secrets?: Op[],
  replace?}`). If the environment is missing, it's created (201). Otherwise its labels, expiry and
  name are brought to what's given, and `secrets` are applied to its own keys at `path` as one
  change, with `replace` also deleting the folder's other own keys (never a linked or personal
  one). An unchanged value is no change, so ensuring what's there records nothing (`change: null`).
  Its kind, base and trust aren't changed in place: 409 `mismatch`, delete it and ensure it again.
  previews' reconcile loop and apps' environments are each one call.
- It is **untrusted** unless made with `"trusted": true` (which needs `admin` on its base): it
  runs a change's code, so reading it never reaches a protected environment (its base, an import, a
  reference), whoever reads it. A preview of a same-repo change in a project that allows it is made
  trusted; a fork's never is. `Environment.trusted` says which.

### Folders

A path is `/` or `/` followed by 1–8 segments of 1–64 characters of `[A-Za-z0-9_-]`, joined by `/`.
Folders exist explicitly (they can be empty). Writing a secret creates the folders on its path.
Deleting a folder deletes what is under it, as one change.

### Secrets

- **Key**: `[A-Za-z_][A-Za-z0-9_]*`, at most 256 characters, unique per environment, path and
  owner. Keys are case-sensitive, like environment variables.
- **Value**: UTF-8, at most 64 KiB (multi-line values such as PEM keys are fine).
- **Comment**: at most 1,000 characters. **Tags**: at most 16, each 1–32 characters of
  `[a-z0-9-]`.
- **Type**: `shared` (everyone with access reads it) or `personal`. A personal value is an override
  that belongs to one principal: when that principal reads the environment, its personal value
  replaces the shared one. No other caller ever sees a personal value, and tokens can't own one.
- **Link** (optional): the value comes from a roll resource. See Links.

### Changes and versions

Every write is a **change**: an atomic batch of operations on one environment, with a message, an
author and a time. Every change gets the next number in its project (`seq`). Each operation makes a
new version of its secret, and a delete makes a tombstone version, so history is complete.

- **Check-and-set**, as Vault KV's `cas`: a set or delete may carry `if_version` (`PUT
  …/secrets/{key} {…, if_version}`, `DELETE …/secrets/{key}?if_version=`, each op of a batch), the
  version the writer read; `0` means the key must not exist yet. It's checked in the same
  transaction as the change: if the key moved, the whole change is refused (409 `stale`, naming
  the key and its version now) and nothing is written. The dashboard's editor sends the version it
  showed, and so does setting a key across environments from the grid (each environment's own
  entry, or 0 where it had none), so two people editing one key can't overwrite each other unseen
  (the second sees the error and the new version). On a protected environment a stale read is
  told so before anything else, and a change request keeps the check: `if_version` is checked as
  the request is opened, in the write that records what it's based on, and applying it later needs
  the environment unchanged since. `if_seq` on a batch is the same for the whole environment
  (`roll secrets edit` uses it).
- **History** of an environment is its changes, newest first. A change's detail lists each key with
  what happened (`added`, `changed`, `deleted`, `comment`, `tags`, `link`), its old and new
  fingerprints, and never a value. Values of old versions can be revealed one at a time.
- **Point-in-time rollback.** `POST …/rollback {"to": seq}` or `{"at": ms}` computes the
  environment's state at that point from its versions and writes one new change that brings the
  environment back to it: it sets keys that changed, restores keys that were deleted and deletes
  keys that didn't exist yet. Rollback never rewrites history: the rollback itself can be rolled
  back.
- **Snapshot.** `GET …/changes/{seq}` with `?state=1` lists every key as it was after that change
  (metadata and fingerprints).
- **Values as they were.** `GET …/values?seq=` (or `?at=` a time) answers a folder's own values as
  they were stored after that change, without rolling anything back: "what did production have
  before the incident". They're as written then: a reference isn't resolved against the past, and
  a live or dynamic link, which stores nothing, and a purged value come back as problems. It needs
  `reveal`, is audited as an export ("as of change #N"), and takes `format=` like any export.
  `roll secrets export --change N` (or `--at 2h`), and the history's "Values then" in the
  dashboard.

### Purge and retention

After a leak and a rotation, the old value shouldn't stay readable in history.
`POST …/secrets/{key}/purge?path= {before_version?}` (`admin`) drops the stored ciphertext of the
key's older versions (default: all but the current one), which nothing can decrypt again. The
history rows stay, marked `purged`, so history still says what changed and when. Revealing a purged
version answers 404. A rollback that would need a purged value leaves that key as it is and says so
in its `warnings`. A project can also keep old values for a while only
(`PATCH /v1/secrets/projects/{project} {history_days}`, 1–3650, `null` for good): the sweeper purges
versions older than that, never a secret's current version.

### References

A value can include other values:

| Syntax | Means |
| --- | --- |
| `${KEY}` | `KEY` in the same environment and folder |
| `${env.KEY}` | `KEY` at the root of environment `env` |
| `${env.folder.sub.KEY}` | `KEY` in folder `/folder/sub` of environment `env` |
| `$${` | a literal `${` |

- References are resolved on the server when values are read with `resolve` (the default for
  `values`, `export` and `roll run`). A reference to another environment needs `reveal` access to
  that environment too.
- Resolution follows references up to 8 deep, and a cycle is reported with its path:
  `cycle: DB_URL → DATABASE_URL → DB_URL`. A missing key or an unreadable environment leaves the
  reference unresolved. Reads report it in `problems`, and `roll run` fails on it unless
  `--allow-unresolved` is passed.
- A write that adds a reference to a key that doesn't exist, or that makes a cycle, succeeds and
  returns the problem in `warnings`. The dashboard shows it before saving.

### Imports

A folder can import other folders (`[{environment, path}]`, in order): their secrets show up in the
folder as if they were its own, read-only. For each key, the folder's own secret wins, then the
imports in the order listed, and then the base environment (for an ephemeral environment). Imports
are followed up to 8 deep, and cycles are ignored and reported. Imported keys are marked with where
they come from.

#### Sharing across projects

An organisation-wide value (an error tracker's DSN, a shared API key) lives in one project and is
shared with the others, with both sides' consent:

1. The owning project's admin **exports** a folder to a project: `PUT …/environments/{env}/exports?path=`
   `[{project}]` (admin on that folder; audited `exports.set`). `roll secrets exports add <project>`.
2. That project's admin **imports** it like any folder, naming the project:
   `{"project": "<id>", "environment": "production", "path": "/shared"}` (admin on the importing
   folder, since every reader of it will read the shared values; refused with 403 unless the folder
   is shared with this project). `GET …/shared` lists what other projects share with this one,
   ready to import. `roll secrets imports add production:/shared --from-project <id>`.

Readers of the importing folder then see the shared values as part of their project, a token
scoped to it too: the export stands in for access to the other project, the one exception to
"nothing reaches an environment its caller can't read", made explicit by the other project's admin.
Values stay encrypted with their own project's data key and are opened with it. The export is
checked on every read, so removing it cuts every import of it off at once. Changes to a shared
value move the importing folder's digest (`roll run --watch` and syncs follow it).

What crosses is stored values only: plain values and managed credentials, never personal values,
the other folder's own imports, a value with a reference, or a live or dynamic link (those would
resolve against the other project's environments and resources).

In the dashboard the imports panel has "From another project" and, under it, "Shared with other
projects" for the folder's own exports; an imported key names the project it comes from.

A protected environment's shared folder never reaches an untrusted consumer (a check, sandbox or
preview), as its own never does. Promotion copies only the folder's own keys (a shared one is the
target's to import, or not). A shared value's fingerprint is its own project's, so compare and the
grid show it apart from a copy of the same value here, and its cell has no fingerprint here.

### Required keys

A project can list keys every standard environment must define, each in a folder
(`PUT …/required [{key, path?, comment?}]`). The overview grid shows a required key as a row even
before any environment sets it (`required: true`). Reading an environment's values reports each
required key it lacks as a problem (`required, but staging doesn't set it`), so `roll run`, checks
and resolves refuse to start without it, as they do for an unresolved reference. An ephemeral
environment counts what its base defines.

**Typed keys.** A required key can also say what its value must be: a `type` (`url`, `int`,
`port`, `bool`, `json`, `email`) and a `pattern` (a regular expression the whole value matches;
at most 500 characters, checked when it's set). A write, an import, a promotion, a change request
or a new ephemeral environment's overrides with a value that doesn't fit is refused with 400
naming the key and the rule (`PORT must be a port (1–65535)`), never the value. A value with a
reference is checked once resolved: reading the environment's values reports `PORT must be a port`
as a problem, so `roll run` won't start with it. A rollback restores what was there, typed or not.
Edit them in Environments → Required keys, or `roll secrets required add PORT --type port`.
A type added after values exist doesn't change them: the overview grid marks each stored value
that doesn't fit (`misfit` on its cell, the rule it breaks), for a caller who may reveal it.
The dashboard's add and edit forms check a typed key's value as it's typed, with the same rules
and messages (`apps/dashboard/src/lib/secrets-required.ts`).

### Owners

A required key can name its `owner` (a plain name: letters, digits, spaces and `@._-/`, since it goes
into chat notices; `roll secrets required add STRIPE_KEY --owner @payments`, the
Environments view's Owner field): the person or team who supplies it. Health's findings about that
key carry it (`owner`, "ask @payments" in the view), and so does the daily notice, so whoever sees
an expired or missing key knows who decides its new value. The key's rule in its own folder
counts first, else its rule anywhere.

### Insights

`GET …/environments/{env}/insights?path=` shows, for each key of a folder, when it last changed
and by whom, when its value was last read and by whom, how often in the last 30 days, and whether
it's **stale**: unchanged for longer than the environment's `max_age_days`
(`PATCH …/environments/{env} {max_age_days}`, 1–3650, `null` for never). Reads come from the audit
log (reveal, export, resolve and sync pushes) over the last 90 days; nothing is decrypted. A stale
key is only flagged: it doesn't stop `roll run`, a deploy or a check.

**Daily notices.** An environment with a notify URL (its protection policy's `notify_url`) gets one
notice a day with its health findings (see Health): expired, missing, exposed, expiring and stale
keys, worst first, each with its detail (`findings`), up to 10 per kind in the text, and a link to
the Health view. Weak values aren't in it (that says something about a value, and a channel's
readers needn't be ones who may reveal it), nor unread ones. Stale needs a `max_age_days`; live
and dynamic links never count as stale (they change on their own), managed ones do. The same
outbox and retries as approval notices carry it; two nodes never both send it, since the day is
claimed in the same write that queues the notice, and an environment with nothing to say is
looked at once a day too.

**Expiry dates.** Some values stop working on a date roll can't see: a vendor's API key, a TLS
certificate, a trial token. A secret's optional `expires_at` (ms; set in any write's `set`
operation, `null` clears it, leaving it out keeps it) records that date. It's versioned like the
comment, so history, rollback, promote, change requests and backups carry it, and a new value
keeps the date until someone changes it, except a certificate: a new value holding PEM
certificates is dated by the earliest `notAfter` among them, unless the write gives a date. Insights add `expires_at` and `expiring` (within 14 days,
or past); the grid and the environment's list show a clock, with the days left inside two weeks
and "expired" in red after. The daily notice above lists expiring and expired keys too (names and
dates only), for any environment with a notify URL, limit or not. Like
staleness, it's only flagged: nothing stops working in roll. `roll secrets set KEY=v --expires
2027-03-31`, `roll secrets expire KEY --on 90d` (or `never`) and the editor's date field set it.

**Protected values set elsewhere** (1Password Watchtower's reuse check). In a protected
environment, each key's `exposed_in` names the unprotected environments that hold the very same
value: production's database password pasted into staging, where more people, previews and less
vetted code can read it. Values are compared by fingerprint (per project; nothing is decrypted),
plain values only: links and references are expected to match, and the empty value is skipped.
Only environments the caller may read are named. The environment's list marks the key "also in
staging", and `roll secrets insights` prints `ALSO IN staging`.

**Read alerts** (Doppler's and 1Password's activity alerts). The environment's notify URL also
hears of reads that look unlike the reader, from the audit log, within a sweep of happening:
- a token or workload identity reading from an address it hasn't read from before (its first
  address isn't news: that's where it runs);
- one principal revealing 30 or more of the environment's values within an hour (someone copying
  it out by hand).

An alert names the principal, the address or the count, the environment and a link to its
activity: never a value. Each is also an audit row (`secrets.alert`, by `roll:alerts`), told or
not: the environment's Activity has an Alerts filter, and audit sinks carry them to a SIEM. The sweep walks the audit log after a cursor; advancing the cursor,
recording the addresses seen and queueing the notices are one write, so two nodes never both
alert, and history from before the first sweep isn't alerted on.

**Weak values** (1Password Watchtower). A key whose name says it's a credential (`PASSWORD`,
`SECRET`, `TOKEN`, `API_KEY`, `PRIVATE_KEY`…, but not `…_URL`, `…_HOST`, `…_ID` and the like) is
judged when it's written, from the plaintext then: a common password, its own key's name, under
12 characters, too few different characters, or under 64 bits of randomness (as if each character
were drawn from its classes). Only the reason is kept (`weak`, never anything of the value), on
`Secret` and in insights, and only a caller who may reveal the value sees it: that it's weak says
something about it. Values with references aren't judged; a value written before this is judged at
its next write. The environment's list marks it "weak", and `roll secrets insights` prints `WEAK`. A change request's values (`…/requests/{id}/values`, for
reviewers who may reveal them) carry `proposed_weak` too, so a guessable credential is caught before
it's approved. The add and edit forms' "Generate" fills in 48 random
letters and digits made in the browser (rejection-sampled, like the CLI's `--generate`), so the fix
is one click.

### Health

`GET …/health` (`roll secrets health`, the dashboard's Health view) is every finding above in
every standard environment and folder at once, worst first, so none waits on someone opening the
right folder: `expired`, `rotation_failing` (a managed key's scheduled rotation fails: the
database, vendor, product or rotator refused or didn't answer; the detail says why, the old
credential keeps working, and the sweeper tries again; cleared by the next rotation that works),
`revoke_failing` (a credential roll retired, or a lease that ended, that it couldn't revoke yet: it
still works until roll can; cleared once it does; `GET …/rotations`, `roll secrets rotations` and
the Health view's "Managed credentials" list every managed credential with its schedule and last
failure; `roll secrets rotations --rotate-due 7` rotates every one due within a week, or failing,
in one run, for an operator's maintenance window, and fails if one does), `missing` (a required key an environment doesn't define, through its
imports and base), `exposed` (a protected value also in an unprotected environment), `weak`,
`expiring`, `certificate_expiring` (a certificate a project CA signed for a CSR that ends
within two weeks: links' certificates are replaced by roll, a CSR's only by whoever asked; `key` is
its names, `path` its CA), `stale`, then `unread` (nobody revealed, exported, resolved or synced it in 90 days,
and it's older than that). Each `Finding` names the environment, folder and key where it's set,
with a detail (the other environments, the reason, the date): never a value, and nothing is
decrypted. Only what the caller may read is listed, and `weak` only where it may reveal.

**Keys no code reads.** With `?code=1` (`roll secrets health --code`, the view's "Check the code"),
health also lists `unreferenced` keys: ones no repository on the project's canvas mentions on its
default branch, looked up in roll code's search index (rolld's `CodeRefs`; roll secrets reads no
files). A mention anywhere counts, as a case-sensitive substring, so a key that might be read is
never flagged. It's a search per key and repository (up to 500 keys), so only on request, and any
repository that can't be searched here (the index not ready, roll code off) makes the check say
nothing at all rather than guess. Whether a name appears in a repository says something about its contents,
so the check is only for a caller who may read every repository on the canvas (`Grants::reads_code`:
with roll-access, the caller's Grant for `code` read on that repository; until then, the operator):
anyone else gets a 403 that doesn't name it (that would tell a caller who can't see the canvas
the repository is there). In the
dashboard each finding fixes where it's listed: a weak key's "New value" (48 random characters
made in the browser) and an expired or expiring key's new date, each one change, or a change
request where the environment is protected. "New values for all N in Staging" does every weak key
of one environment at once: one change (or one change request) with a random value for each. The
Health tab and the sidebar's Secrets entry carry
the count of findings to fix now (expired, missing, exposed), and Home lists them across every
project (`GET /v1/secrets/findings`, `roll secrets health --all`).

Health, `POST …/leaks`, `GET /v1/secrets/search` and `GET /v1/secrets/findings` filter each
finding by the caller's own access: roll secrets asks the caller's roll-access Grant, so
roll-access lets a token limited to some environments or folders call them (health and leaks are
`filters_itself` in its catalogue, search and findings take any grant) and its answer just leaves
the rest out. Keep the filter in each: without it, the mark would let such a token see the whole
project.

### Compare and promote

- `GET …/compare?from=development&to=staging&path=/` lists every key in either environment with a
  status: `same`, `different`, `only_from` or `only_to`. Fingerprints decide; no value is revealed.
  A key in both that the caller can't compare (it may not reveal one side, or one is sensitive) is
  `hidden`: equality would test guesses against a value it can't read.
- `POST …/environments/{to}/promote?from={from}` copies the selected keys (values, comments, tags and
  links) from one environment to another as one change in the target. On a protected target, it
  opens a change request instead.

## Links

A **link** makes a secret's value come from a resource in the same project. The resource must be on
the project's canvas (a workspace resource). Each resource kind has a **link provider**: a small
trait in `crates/roll-secrets/src/links.rs`, implemented in `crates/rolld/src/secrets.rs` by calling
the product's own HTTP API in process. `roll-secrets` has no list of kinds: a product makes its
resources linkable by registering a provider. That is how roll search, roll apps and future
products join without changes here.

```rust
pub trait LinkProvider: Send + Sync {
    fn kind(&self) -> &str;                                   // "postgres", "search", …
    fn fields(&self) -> &[Field];                              // url, host, password, api_key…
    fn read(&self, r#ref: &str, field: &str, opts: &Opts) -> Result<Value>;          // live
    fn issue(&self, r#ref: &str, opts: &Opts) -> Result<Issued>;                    // managed/dynamic
    fn revoke(&self, r#ref: &str, handle: &Handle) -> Result<()>;
}
```

A link is `{kind, ref, field, mode, options}`:

- **`live`** (a connection URL, a host, a bucket's endpoint): the value is read from the product
  when it is resolved, so it is always current. If the product rotates the password, the next read
  has the new one. History records the link, not the value.
- **`managed`** (an access key pair, a stream API key): roll secrets asks the product to issue a
  credential, stores it as the secret's value (encrypted, versioned), and can **rotate** it, on
  demand or every `rotate_days`. Rotation issues a new credential, makes it the current version, and
  revokes the previous one after `grace_minutes` (default 60), so running services have time to
  pick up the new value.
- **`dynamic`** (short-lived credentials): each resolve that asks for it (`roll run`, an app's
  start) issues a fresh credential with a TTL (`ttl_minutes`, default 60, at least 15 minutes, at most 7 days), recorded
  as a **lease**. Leases are revoked when they expire, or at once with `DELETE …/leases/{id}`. A
  long-running consumer renews instead of restarting: `POST …/leases/{id}/renew` pushes the expiry
  out by the link's TTL (at most 30 days from issue), and the provider follows where the product
  enforces its own expiry (a Postgres role's `VALID UNTIL`). `roll run --watch` renews its leases
  every 10 minutes. `Values.dynamic` names the keys that hold one, for a consumer that can't renew
  (a service that scales to zero refuses them). Only
  the lease's metadata is stored, never the credential.
- **When a secret, an environment or the project goes**, what it issued is revoked first, while
  the project's key and registries (rotators…) can still reach it. A revocation that fails (the
  product or the customer's endpoint is down) stays as a lease that's already due, and the sweeper
  retries it; a deleted project keeps its key and registries (`projects.forgotten_at`) until its
  last lease is revoked, then they go too. Nothing it issued outlives it unrevoked.

**Suggested links.** `GET …/suggestions` lists each resource on the project's canvas with the
standard environments that neither link it nor have its usual keys at `/`, and those keys: the
provider's usual fields (a credential of the app's own, managed, wherever the product issues one:
a Postgres role, a bucket key, a Kafka user; never the shared password) under their usual names
(`DATABASE_URL`, `AWS_*`, `KAFKA_*`), prefixed with the resource's name when the canvas has two of a
kind. The overview offers each as one click to link it in all of them (a protected environment gets
a change request). Only roll can: it knows the canvas.

Which fields and modes each kind offers is in `GET …/links` (the catalogue of the project's
resources and their fields). The providers rolld registers are listed in `crates/rolld/src/secrets.rs`
and in the table below.

| Kind | Live fields | Issued (managed or dynamic) | Rotate the resource |
| --- | --- | --- | --- |
| `postgres` | `url`, `host`, `port`, `user`, `password`, `database` (option `branch`, default `main`) | a role of its own, credential `role`: `role_url`, `role_user`, `role_password` (`CREATE ROLE … VALID UNTIL` through the console's query route; option `permission` `read` or `write`) | no |
| `redis` | `url`, `host`, `port`, `password`, `rest_url`, `rest_token` | no (ACL users don't survive an unload) | yes: a new password |
| `stream` | `bootstrap`, `security_protocol` | an API key, credential `key`: `username`, `password`, `properties` | no |
| `bucket` | `bucket`, `endpoint`, `region` | an access key limited to the bucket, credential `key`: `access_key_id`, `secret_access_key` (option `permission` `read`, `write` or `admin`) | no |
| `nosql` | `cql`, `username`, `password`, `http_token`, `alternator` | no | yes: a new password |

Secrets of one environment linked to the same resource and credential share one issue: linking
`AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` gives one access key, and rotating either rotates
both, in one change. A Postgres role made for a branch also exists in branches made from it later
(roles live in the branch's data); it expires with `VALID UNTIL` either way. The role gets a SCRAM-SHA-256
verifier, never the password itself, so a failing statement can't put it in the compute's log.
Revoking a role ends
its sessions and gives what it made (a migration's tables) to `roll` before dropping it, so revoking
a credential never drops data.

| `search` | `url`, `host`, `port`, `username` | a credential of its own with data access only (no keys or users; `"access": "data"`), credential `key`: on an Elasticsearch cluster `api_key` (`Authorization: ApiKey …`), on an OpenSearch one an internal user, `user` (`<user>@<cluster>`, which the node routes) and `password`; a link to the field the cluster's flavor doesn't issue is refused when written, and switching a cluster's flavor carries no credential across (link the other fields again); an OpenSearch cluster's fields are offered and suggested under the names its clients read (`OPENSEARCH_URL`, `_HOST`, `_PORT`, `OPENSEARCH_USERNAME`/`_PASSWORD` for its user, `OPENSEARCH_CLUSTER` for the cluster's name); a dynamic one expires by itself 5 minutes after its lease | no (never the cluster's password) |

`search` resources are linkable once roll search's workspace kind lands (`feature/roll-search`).
roll apps' services have a workspace kind (`service`, `svc_…`); they consume secrets (an app's
variables, `app:<svc>`) rather than issue credentials, so they have no link provider. Products
without a workspace kind yet (queues, tasks) join by adding their kind to the workspace and
registering a provider.

### Rotators

Some values live outside roll and still need rotating: a vendor's API key, a database roll doesn't
run. A **rotator** is a URL the project names (`POST …/rotators {name, url}`, `admin`; the answer
holds its signing secret, once), and a link kind like a product's: a key linked to it in `managed`
mode (`{kind: "rotator", ref: "rot_…", field: "value", mode: "managed"}`, from the dashboard's Link
a resource too) gets its value from the rotator and then behaves as every managed link does. It
rotates every `rotate_days` or on demand, the old value is revoked after `grace_minutes`, keys of one
environment linked to the same rotator share one value, and a protected environment gets a change
request.

Each call is a POST from rolld's guarded client (HTTPS, a public address, no redirects), signed as
every roll webhook is (`X-Roll-Signature-256`, `X-Roll-Timestamp`, `X-Roll-Delivery`), with
`X-Roll-Rotator: <id>`:
- `{"action": "issue", "rotator", "label"}`: mint a new value; answer `{"value": "…", "handle": …}`
  (at most 96 KiB). `handle` is anything the endpoint needs to revoke that value later; it's kept
  as given (not sealed, like a product's credential handle), so it names the value, never holds it.
- `{"action": "revoke", "rotator", "handle"}`: retire it; any 2xx.

The URL (which may carry a token) and the signing secret are sealed with the project's data key;
only the host is shown. An answer is read once, never logged, and never part of an error. A rotator
a key still links to can't be deleted, nor one with a retired credential still waiting out its
grace period (its revocation goes through the rotator).

### Databases outside roll

A Postgres, MySQL, Redis or MongoDB server roll doesn't run (Amazon RDS, Supabase, Neon, PlanetScale, ElastiCache, Upstash, MongoDB Atlas, a server of the customer's own)
has its logins rotated by roll itself, with no rotator to write, as Infisical's secret rotation
does, and issues a login per lease, as Vault's database engine does. A project registers it
(`POST …/outside-databases {name, url, roles?, group?, ca?}`, `admin`; `roll secrets databases add
NAME --role a --role b --group g`, the URL on stdin): `url` is an admin connection
(`postgres://admin:…@host:5432/db`, `mysql://admin:…@host:3306/db`,
`sqlserver://admin:…@host:1433/db`, `clickhouse://admin:…@host:8443/db` or
`rabbitmq://admin:…@host:15671/vhost`, a user that may alter both
roles and create roles in the group), sealed with the project's data key like a rotator's (only `host:port` is shown); `roles` are
two login roles that take turns (managed links); `group` is a role whose grants dynamic logins get;
one or both. `ca` is the server's CA certificates (PEM) when the system's don't cover it (RDS).
Registering connects once and checks the roles exist (and can log in).

It's the link kind `outside_database` with fields `username` and `password`, one credential
(`login`), `managed` or `dynamic`.

**Managed** (two roles taking turns): each issue (linking, every `rotate_days`, `…/rotate`) gives the role not
handed out last a new 40-character password and hands it out, so the role in use keeps working
until its grace period ends; then its password is replaced by one nobody knows. Each role's
generation is kept with the database and carried in the credential's handle, so a retirement that
ends after the same role was handed out again does nothing. An issue never takes a busy role: the
one a stored credential uses, one an unrevoked lease still covers (the previous login in its
grace), or one another issue or retirement is changing (each holds its role for two minutes at
most), all worked out in the write that claims it. So two rotations racing (two nodes' sweepers, a
person and the schedule) can't reset the login in use; rotating again within the grace period is
a 409 until it ends. A rotation the server refuses changes nothing, and a role whose handout is
retired (its grace ended, or the write that issued it failed) is free again.
The two roles serve one credential, so a database is linked in one environment only (409 elsewhere;
a link provider says so with `exclusive()`): give staging and production a pair of roles each.

**Dynamic** (a login per lease): each resolve creates `roll_<random>` with `CREATE ROLE … LOGIN
PASSWORD <verifier> VALID UNTIL <lease end + 5 min> IN ROLE <group>`, so it has the group's grants
and the server refuses it after the lease even if roll is down; the username and password keys share
it. Renewing the lease moves `VALID UNTIL`. Ending it (expiry, `DELETE …/leases/{id}`) ends the
login's sessions where the admin may (`pg_signal_backend`) and drops the role; a role that owns
something the app made (a table) is kept, unable to log in (`NOLOGIN`, `VALID UNTIL '-infinity'`), for
the customer to reassign. Dynamic links work in any number of environments.

rolld connects the way it fetches anything a tenant chose: it resolves the host itself, refuses a
private address (`ROLL_CODE_WEBHOOK_ALLOW_PRIVATE` allows them for local development), and connects
to the address it checked. The URL may set `sslmode` and little else (no socket paths, several
hosts or `sslrootcert` files); TLS is required (`prefer` is treated as `require`, certificates are
verified) and `sslmode=disable` works only where private addresses do. The new password goes as a
SCRAM-SHA-256 verifier (`ALTER ROLE … PASSWORD 'SCRAM-SHA-256$4096:…'`), so neither the connection
nor the server's statement log ever holds it. One project can't register the same role on one host
twice (the two would undo each other's passwords), and a database a key links to, or with a
retired password still to replace, can't be deleted.

**MySQL** works the same way through the same guards (`mysql://…`; the URL may set `ssl-mode` and
nothing else; TLS verified against the URL's host, never a socket). Accounts are `'<name>'@'%'`:
rotation is `ALTER USER … IDENTIFIED BY` (the password in the statement, over TLS; MySQL rewrites it
out of its logs), and a dynamic login is `CREATE USER`, `GRANT <group>` and `SET DEFAULT ROLE
<group>`. MySQL has no `VALID UNTIL`, so a dynamic login lasts until roll ends its lease (renewing
changes nothing on the server); ending it kills its sessions where the admin may
(`CONNECTION_ADMIN`) and drops the account.

**Redis** (`rediss://user:password@host:6379`, ACL users; `redis://` only where private addresses
are allowed; no path or settings). Passwords go as `#<sha256>` (`ACL SETUSER <role> resetpass
#…`), so the server never sees one. Redis has no roles, so `group` names a template user (off, no
password) whose rules a dynamic login copies: `ACL SETUSER roll_… reset on #… <its commands, keys
and channels>`; a template with selectors is refused. Ending the lease is `ACL DELUSER`, which also
closes the user's connections; Redis has no expiry for a user, so a dynamic login lasts until roll
ends its lease. Every change is kept across a restart the way the server keeps its users: `ACL
SAVE` with an ACL file (a failure fails the change, or a restart would bring back a retired
password), else `CONFIG REWRITE` where there's a config file.

**MongoDB** (`mongodb+srv://user:password@cluster.example.net`, Atlas's form, or
`mongodb://user:password@host1,host2/?tls=true&authSource=admin`; `tls=true` is required unless
private addresses are allowed, and the URL may carry what Atlas's strings do, nothing else). User
management runs on the primary, found with `hello` among the seed hosts only: a server naming a
primary outside them is refused, and a `mongodb+srv://` name's SRV targets must sit inside its own
domain (the spec's rule), so a server or a DNS answer can't send roll anywhere else. roll logs in with
SCRAM-SHA-256, checks the server's signature (a server that can't prove it knows the admin's
password isn't the one the URL names), and refuses an iteration count past 1,000,000. Rotation is
`updateUser` with a new `pwd` over TLS (MongoDB keeps only SCRAM credentials and redacts them from
its logs); `group` is a role, `name` in `authSource` or `name@db` (`read@shop`); a dynamic login is
`createUser` with it, and ending the lease kills its sessions where the admin may
(`killAllSessionsByPattern`) and drops it. MongoDB has no expiry for a user. Replies are read by a
small BSON reader that checks every length against the bytes present, caps nesting, and reads at
most 1 MiB. The server is the tenant's, so its replies are read by a small parser that bounds
nesting, counts and lengths by the bytes present.

**ClickHouse** too, over its HTTPS interface (`clickhouse://admin:…@host:8443/db`, ClickHouse
Cloud's port; no settings): rolld pins the address it checked, verifies the certificate against
the URL's host (the system's roots and `ca`), follows no redirect and uses no proxy, and reads at
most 64 KiB of an answer. Values go as query parameters (`{n:String}`), never into SQL. Passwords
go as `IDENTIFIED WITH sha256_hash`, so the server never sees one (its query log holds the hash):
rotation is `ALTER USER` on two users that take turns; a dynamic login is `CREATE USER`, `GRANT
<group>` and `DEFAULT ROLE <group>` (the group is a role); ending it drops the user, then stops
what it's still running (`KILL QUERY`). ClickHouse has no expiry for a user, so a dynamic login lasts until
roll ends its lease. Users made over SQL need the admin's `access_management`; on a self-managed
cluster, keep users in replicated access storage. Checked against ClickHouse 25.8: rotation (the old
password refused, the new one taken), a dynamic login that signs in with the role and can't once
dropped, an untrusted certificate and a wrong admin password refused, and no password in the
server's query log.

**RabbitMQ** too, as Vault's and Infisical's RabbitMQ engines, through its management HTTPS API
(`rabbitmq://admin:…@host:15671/vhost`; no vhost is `/`; no settings), with the same guards as
ClickHouse (the checked address pinned, the certificate verified, no redirects or proxy, answers
read up to 256 KiB). The admin needs the `administrator` tag (checked at registration, with the
vhost and the users). Passwords go as RabbitMQ's own salted SHA-256 (`password_hash`), so the
broker never sees one. Rotation sets a new password on two users that take turns, keeping their
tags; `group` names a template user whose permissions and topic permissions on the vhost a
dynamic user copies (no tags), all of them or none: an exchange without a topic permission is open
to every routing key, so a template with more than 100 is refused rather than copied in part, and
a grant that fails undoes the user. RabbitMQ has no expiry for a user: ending a lease deletes it, then
closes its connections where the broker has that route (3.11 and later). Checked against RabbitMQ
4: rotation (tags kept, the old password refused), a dynamic user with the template's permissions
that's gone once dropped, and an untrusted certificate, a wrong password and a user without the
administrator tag refused.

**SQL Server** (and Azure SQL Managed Instance) too, as Vault's MSSQL engine (`sqlserver://…`; the
URL may set `encrypt=true` and nothing else). Everything is encrypted and the certificate verified
against the URL's host (TDS 7.4 with `Encrypt=Required`; nothing in plain text, even where private
addresses are allowed; TDS 8 `strict` isn't offered: SQL Server 2022 on Linux refused it in testing),
and a redirect to another server (Azure's gateway) is refused, since roll connects only to the
address it checked. Rotation is `ALTER LOGIN [name] WITH PASSWORD` on two SQL logins that take
turns; a dynamic login is `CREATE LOGIN`, then `CREATE USER … FOR LOGIN` and `ALTER ROLE <group> ADD
MEMBER` in the URL's database (the group is a database role). It has no `VALID UNTIL` either:
ending a lease disables the login first (that works while it's connected, so nothing can log in
with it even if a lingering session makes the drop fail and the sweeper retries), kills its
sessions, then drops the user and the login. Registering checks the admin login's server
permissions: `ALTER ANY LOGIN`, and for dynamic logins `VIEW SERVER STATE` and `ALTER ANY
CONNECTION`. roll's passwords mix upper- and lowercase letters and digits, which SQL Server's
password policy requires (a lowercase-and-digits one is refused, as checked live). A conversation reads at most 4 MiB from the server, on top of the
driver's own bounds. Checked against SQL Server 2022 in Docker with a certificate from a private CA:
rotation (the rotated login signs in), a dynamic login that signs in with the group's rights and
can't once dropped, and an untrusted certificate refused. With TLS 1.3 enabled on the server as
well (`tlsprotocols = 1.2,1.3`), SQL Server 2022 on Linux refused TDS 7.4 logins, Microsoft's own
`sqlcmd` included: such a server needs TLS 1.2 allowed.
### GCP and Azure
Short-lived GCP and Azure access tokens for each lease, as Vault's GCP and Azure engines give, with
no key of the customer's stored anywhere: **roll is an OpenID Connect issuer**, and the customer's
cloud trusts roll's token for one project the way it trusts GitHub Actions' tokens.
- **The issuer** is `<rolld's public URL>/v1/secrets/oidc` (`ROLL_CODE_PUBLIC_URL`; the clouds must
  reach it over HTTPS). Its discovery document and keys (`…/.well-known/openid-configuration`,
  `…/jwks`) are public. Tokens are RS256 (Azure takes nothing else), live five minutes, and carry
  `sub = roll-secrets:<project>` (and `project`). The signing key (RSA 2048) is made at startup,
  sealed with the deployment key, and shared by every node; every published key verifies, and the
  public routes read only public halves. After a deployment-key rotation it's resealed like
  projects' keys (health's `resealed` counts it).
- **GCP**: a workload identity pool provider for roll's issuer whose attribute condition pins
  `assertion.sub == "roll-secrets:<project>"`, and a service account that lets that principal
  impersonate it (`roles/iam.workloadIdentityUser`). Register it with `POST …/cloud-accounts {name,
  cloud: "gcp", audience: "//iam.googleapis.com/projects/<number>/locations/global/workloadIdentityPools/<pool>/providers/<provider>",
  service_account}`. Each lease: STS token exchange, then `generateAccessToken` for the service
  account, for the lease's TTL plus five minutes (an hour at most).
- **Azure**: an Entra app with a federated credential for roll's issuer, subject
  `roll-secrets:<project>`, audience `api://AzureADTokenExchange`. Register it with `{name, cloud:
  "azure", tenant, client_id, scope?}` (`https://management.azure.com/.default` by default). Each
  lease: client credentials with roll's token as the assertion.
- **The trust must name this project.** Registering an account (admin) first exchanges a token for a
  made-up project: one the cloud takes is refused, naming the condition to add, so no tenant can
  register (and use) another customer's account that trusts roll. Only a definite refusal
  (`invalid_grant`, `PERMISSION_DENIED`, `AADSTS70021`…) passes; a timeout or an outage fails the
  registration. Then this project's token must work.
- A key links to an account's `access_token` in `dynamic` mode, with a lease of 55 minutes at most
  (so it ends before its token; a longer one is refused when it's linked). A token can't be ended
  early or extended: a lease that ends leaves it valid until it expires, an hour at most; resolve again for a
  new one. An account a key links to can't be deleted. `roll secrets clouds ls` prints the issuer and
  subject to trust; `roll secrets clouds add-gcp` / `add-azure` register one.
### Certificates
A project's own certificate authority, as Vault's PKI engine and Infisical's PKI give: short-lived
TLS certificates for services (mTLS between them), with no CA to run.
- `POST …/cas {name, domains, allow_ip?, max_days?}` (admin; `roll secrets cas add NAME --domain
  svc.internal`, the Integrations tab) makes an ECDSA P-256 key and a self-signed certificate for ten
  years. The key is sealed with the project's data key, bound to the CA, and never leaves roll; the
  certificate (`GET …/cas`, `roll secrets cas cert NAME`) is what clients trust. It carries name
  constraints: only its domains, and IP addresses only where allowed (otherwise excluded), so a
  client that trusts it trusts nothing else.
- The link kind `pki` has `certificate`, `private_key` and `ca` (one credential, PEM), with the
  certificate's names in `options.names` (a list, or comma-separated; the dashboard's Link dialog
  asks): DNS names under the CA's domains, a wildcard one level under one, or IP addresses where
  allowed; anything else is refused, naming the domains. `dynamic` gives each lease its own
  certificate for the lease's TTL plus five minutes; `managed` stores one and rotates it every
  `rotate_days` (required, and at most half the CA's `max_days`; the Link dialog asks), valid for
  twice that, so a rotation always comes first. Both are at most `max_days` (90 by default, 397 at
  most) and never past the CA's own end. A link's names and rotation are checked when it's written.
  Certificates are for servers and clients both (`serverAuth`, `clientAuth`; the key only signs),
  with a random 128-bit serial.
- `POST …/cas/{id}/sign {csr (PEM), days? | ttl_minutes?}` (`write`, audited; `roll secrets cas
  sign NAME req.csr [--days N] [--chain]`) signs a CSR, for a device or a service mesh that keeps
  its own private key. The request's signature is checked, then only its public key and its
  subjectAltName names (DNS names and IP addresses, each within the policy; the subject's CN isn't
  read) are taken: usages, lifetime (30 days, or the CA's `max_days` if shorter, by default; 5
  minutes to `max_days`) and everything else are the CA's own, whatever the CSR asks (being a CA
  itself, signing code). The CSR's key must match its signature (a P-384 key signed with SHA-384,
  not SHA-256). It answers `{certificate, ca, serial, names, not_after}`. Checked with
  OpenSSL's CSRs (P-256 and RSA): `openssl verify -x509_strict` passes against the CA.
- Every certificate a CA signs is listed until it expires (`GET …/cas/{id}/certificates`: serial,
  names, `csr` or `link`, its end, who asked, when it was revoked; `roll secrets cas certs NAME`).
  `…/cas/{id}/revoke {serial}` (`write`, audited; `roll secrets cas revoke NAME SERIAL`, the
  dashboard's Revoke) takes one back, a lost device's, say; roll revokes a lease's certificate
  when the lease ends and a managed one's when its grace runs out. The CA signs a CRL of the
  revoked certificates that haven't expired, good for an hour and signed at most once a minute per
  node (a revocation shows in that node's next one at once, another node's within a minute):
  `GET /v1/secrets/crl/{id}` (DER, `application/pkix-crl`; `?format=pem`), without a token, as any
  CRL is fetched. Every certificate names it in its CRL distribution point (rolld's
  `ROLL_CODE_PUBLIC_URL`). Checked with OpenSSL: `openssl verify -crl_check` refuses a revoked one
  (`certificate revoked`) and passes the rest. **OCSP** too, beside the
  CRL (RFC 6960), for clients that ask about one certificate rather than fetch lists:
  `POST /v1/secrets/ocsp/{id}` (the request's DER as the body) or `GET /v1/secrets/ocsp/{id}/<base64
  of the request>` (cacheable), without a token. A certificate is `good` while the CA lists it
  unrevoked, `revoked` with its time once revoked, `unknown` when the CA didn't sign it, it has
  expired or the request names another issuer; the CA signs each answer itself. The route is
  public, so its cost is bounded: nonces aren't echoed (RFC 8954 allows it, as large public CAs
  do), answers are kept a minute per node by the certificates asked about (a revocation drops the
  CA's), and a CA signs at most 50 fresh answers a second per node (`tryLater` beyond). An answer is
  good for ten minutes (`nextUpdate`; GET answers `max-age=60`, and `no-store` on anything but a signed answer): how long a revocation can take to
  reach a client that keeps answers. Every
  certificate names the responder (Authority Information Access). Checked with `openssl ocsp`:
  `good` and `revoked`, `Response verify OK`, over POST and over GET. Clients that
  don't check CRLs or OCSP still trust a revoked
  certificate until it ends, so lifetimes stay short. A CA a key links to can't be deleted, nor one
  with a revoked certificate still within its lifetime (its CRL must keep answering; a deleted
  project's CAs keep serving theirs until then too); deleting one leaves what it signed valid until
  it expires. One CA keeps at most 10,000 certificates from CSRs valid at once. Serials are
  random, positive and never start with a zero byte, so the serial a client shows is the one to
  revoke.

### SSH certificates

A project's own SSH CA, as Vault's SSH engine signs them: short-lived user certificates instead of
long-lived public keys on every server. Servers trust one CA key (`TrustedUserCAKeys`).

- `POST …/ssh-cas {name, principals, max_hours?, host_domains?, max_host_days?}` (admin; `roll secrets ssh-cas add NAME --principal
  deploy`, the Integrations tab) makes an Ed25519 key, sealed with the project's data key and bound to
  the CA. Its public key (`ssh-ed25519 …`, `roll secrets ssh-cas key NAME`) is what servers trust.
  `principals` are the logins it signs for; `max_hours` (24 by default, a week at most) is the longest
  a certificate lives.
- The link kind `ssh` has `certificate` (the `…-cert.pub` line) and `private_key` (an unencrypted
  OpenSSH key), one credential, with the logins in `options.principals` (the Link dialog asks); a
  login the CA doesn't allow is refused when it's linked. Each lease gets a new Ed25519 key and a
  user certificate (`permit-pty`, `permit-port-forwarding`; the key id names the lease) for its TTL
  plus five minutes, at most `max_hours`. Links are `dynamic` only: a certificate lives hours, and a
  managed rotation counts in days.
- The certificate and key formats are OpenSSH's (`PROTOCOL.certkeys`, `PROTOCOL.key`), written by roll
  and checked against OpenSSH 10: `ssh-keygen -L` reads them, and `sshd` with `TrustedUserCAKeys`
  takes the certificate for its principal and nobody else's. A certificate can't be taken back, so
  lifetimes stay short; an SSH CA a key links to can't be deleted.
- **`roll ssh -e production -- deploy@host`** (as `vault ssh` and `tsh ssh`): resolves the
  environment's `SSH_CERT` and `SSH_KEY` (`--cert`, `--identity` for other keys; a dynamic link gets
  a pair for this session), writes them to a private temporary directory (0700, the key 0600), runs
  `ssh -i … -o CertificateFile=… -o IdentitiesOnly=yes` with the rest of the arguments, deletes both
  when it exits, and exits with ssh's code. A person never handles the key. Hosts the project's
  SSH CAs sign are trusted by their CA (their `@cert-authority` lines are added after the user's own
  `known_hosts`, which keeps any host the user accepts), so there's no "trust this host?" on first
  use. `--key` and `sign-user` read only a public key line: given a private key's path they read
  `<path>.pub`, and anything else is refused before it leaves the machine.
- **A key you already have** (in an agent, on a hardware token): `POST …/ssh-cas/{id}/sign-user
  {public_key, principals, ttl_minutes?}` (`reveal`, as resolving an SSH link is, since a certificate
  is a login, and `write`, since otherwise a principal comes only from a link a writer made;
  audited, and refused if the audit row can't be written; `roll secrets ssh-cas sign-user NAME ~/.ssh/id_ed25519.pub --principal
  deploy`) certifies its public key for logins the CA allows, 60 minutes by default and the CA's
  `max_hours` at most; only the public key comes to roll. `roll ssh --key ~/.ssh/id_ed25519.pub
  --ca ops --principal deploy -- deploy@host` does it per session (the private file next to it,
  or else the agent's matching key). Checked with OpenSSH 10: an RSA key held only in `ssh-agent`
  logged in to a real `sshd` as `deploy` with roll's certificate, and not as `root`.
- **Host certificates**, as Vault's SSH engine signs host keys: an SSH CA with `host_domains`
  (`--host-domain svc.internal`, a domain of the project's own: a top-level or common public
suffix such as `com`, `co.uk` or `github.io` is refused (from a built-in list of the common ones, not the whole Public Suffix List: signing hosts is an admin's, and a CA's `@cert-authority` line only reaches clients that add it); `max_host_days`, 90 by default, 397 at most) signs a server's own
  public key for its names. `POST …/ssh-cas/{id}/sign-host {public_key, hosts, days?}` (admin, since
  a host certificate lets a server stand in for those names; audited; `roll secrets ssh-cas
  sign-host NAME /etc/ssh/ssh_host_ed25519_key.pub --host db1.svc.internal`) takes the key line
  (Ed25519, ECDSA P-256/384/521, or RSA of 2048–16384 bits, parsed with bounds), checks each name is
  under a host domain (a `*.` one level under one too), and answers the `…-cert-v01@openssh.com`
  line for sshd's `HostCertificate` and the `known_hosts` line clients add: `@cert-authority
  *.svc.internal,svc.internal ssh-ed25519 …` (`roll secrets ssh-cas known-hosts NAME`, the CA's row
  in the Integrations tab). The server's key never leaves it. A CA may sign users, hosts or both.
  Checked against OpenSSH 10: `ssh-keygen -L` reads all three key types' certificates as host
  certificates, and `ssh -o StrictHostKeyChecking=yes` with only the `@cert-authority` line accepts
  a real `sshd`'s Ed25519, ECDSA and RSA host certificates and refuses another name (`Host key
  verification failed`).
### Sensitive keys
A key tagged `sensitive` (`roll secrets set KEY --tag sensitive`, or its tags in the dashboard) can be
written but never read back by a person or a token, as Vercel's sensitive environment variables and
Doppler's restricted secrets: only roll apps' builds and instances get its value (`ForApps`
resolves as a runtime), where the service's code uses it.
- Everything else is a person or acts for one, so it's withheld there. The operator is no exception.
  - `…/values` and `roll run` leave it out and name it in `withheld`, so `roll run` still starts,
    saying so. `…/secrets/{key}/value` answers 403, and so does every version of a key that's
    sensitive now: tagging a key keeps its value, so an older version holds it too.
  - A reference doesn't resolve to it (`${KEY}` can't carry it into another key). A change
    request's values hide it, when it's sensitive now or the request makes it so (a promote, a new
    key). A backup carries its name, not its value.
  - Checks, sandboxes (a terminal), previews, syncs (a target its maker chose and may read back:
    the key is "not synced" and the target keeps what it had) and CI's workload identities don't
    get it.
- Nothing tests a guess against it either: no fingerprint, weakness or typed-key misfit shows for
  it, comparing environments says `hidden`, a local file's diff skips it, and the leak scan
  (`POST …/leaks`) doesn't match it. roll code's own scans of pushed code still do: a sensitive
  value in code leaked like any other.
- Sensitive is for good: a write that takes the tag off is refused (delete the key and set it again
  for a value people can read).
- In-process callers say what they are (`Caller::runtime`): `Caller::consumer` is one, and
  `.not_runtime()` makes it not (checks and sandboxes). A runtime's read names the keys whose value
  holds a sensitive one, directly or through a reference, in `sensitive`, so it uses them and never
  shows them (roll apps puts them in a container's environment and masks them in its variables, as
  its own sealed ones).
### Encryption keys
Encryption as a service, as Vault's Transit engine and Infisical's KMS give: an app sends data and
gets ciphertext back under a named key that never leaves roll, so it can keep encrypted fields (card
numbers, tokens, notes) in its own database without holding a key.
- `POST …/transit {name}` (admin; `roll secrets transit add NAME`) makes a key. Each version is 32
  random bytes sealed with the project's data key, bound to the key and version, so a project's
  customer-managed key covers it too.
- `POST …/transit/{name}/encrypt {plaintext (base64), context?}` (`write`) answers `{ciphertext}`:
  `roll:v<version>:<base64url(nonce ‖ AES-256-GCM)>`. `context` (a row's id, say) is bound in: the
  same ciphertext under another context won't open. `{items: […]}` encrypts a batch (1,000 items,
  4 MiB at most; 64 KiB each) and answers `{items: [{ciphertext}]}`.
- `…/decrypt {ciphertext, context?}` (`reveal`, audited) answers `{plaintext}` (or a batch);
  `…/datakey {context?}` (`reveal`) a fresh 256-bit data key and its ciphertext, for encrypting large
  data locally (envelope encryption).
- `…/rotate` (admin) adds a version new encryptions use; older ones keep decrypting.
  `…/rewrap` (`write`) re-encrypts under the newest without the plaintext leaving roll, and
  `PATCH …/transit/{name} {min_decrypt_version}` retires older versions. `DELETE …/transit/{name}?confirm=<name>`
  deletes a key for good: what it encrypted can't be decrypted again.
- Every key also signs and computes HMACs, each with its own key derived from the version's
  bytes (HKDF-SHA256, one label per use). `…/sign {input (base64)}` (`write`) answers
  `{signature}`: `roll:v<version>:<base64url>`, Ed25519 over the input exactly as given, so a
  service holding only the public key checks it with any Ed25519 library (webhooks, tokens an app
  issues). `…/verify {input, signature}` (`read`) answers `{valid}`; `GET …/public` (`read`) is
  every version's public key as raw base64, PEM (SPKI) and a JWK (`kid` `<name>:v<version>`).
  `…/hmac {input}` (`write`) answers `{hmac}`, HMAC-SHA256, stable for one input: a lookup column
  over encrypted fields (a blind index). `…/hmac/verify {input, hmac}` answers `{valid}` and is
  `write`, not `read`: it answers guesses about the input. All take batches, as encrypt does; a
  signature or HMAC by a retired (`min_decrypt_version`) or unknown version is `valid: false`, a
  malformed one a 400. Signing is `write` on the project, as encrypting is: a grant can't yet be
  narrowed to one key, so anyone who may write the project's secrets may sign with its keys.
- A key can be brought from outside, as Vault's and Infisical's KMS imports: `POST …/transit {name,
  key, vault_version?}` or `…/rotate {key, vault_version?}` (admin; `roll secrets transit import NAME
  [--vault-version N]` with the key on stdin) takes 32 bytes (base64) as the first or newest version.
  From Vault's transit (an exportable `aes256-gcm96` key, `vault read
  transit/export/encryption-key/NAME/<n>`), name each version's Vault number: Vault's own
  ciphertexts, `vault:v<n>:…`, then decrypt under it (Vault's AAD is empty, so no context; derived
  keys aren't brought over) and `…/rewrap` moves them to `roll:v…`, so data encrypted before roll
  comes over without its plaintext leaving. Checked against Vault 1.20: its exported key brought in,
  its `vault:v1:` and `vault:v2:` ciphertexts decrypted and rewrapped. The listing marks brought
  versions (`imported`, `vault_version`); backups keep the Vault numbers.
- A failed decrypt or rewrap is audited too (probing contexts leaves a trace). A backup carries every
  key's versions, in the archive sealed to its taker's key, so a restore (here or on another server)
  still opens what they encrypted; ciphertexts are bound to their context, not to the key's id.
- Nonces are random (96-bit): rotate a key well before 2³² encryptions under one version, the bound
  AES-GCM sets for random nonces (`…/rotate`, then `…/rewrap` what matters).

## Consumption

### `roll secrets` and `roll run`

```
roll secrets ls      [-p project] [-e env] [--path /]          # keys, masked
roll secrets get     KEY [--raw]
roll secrets set     KEY=VALUE… | KEY (value from stdin) [--comment] [--tag]
roll secrets rm      KEY…
roll secrets import  .env [--overwrite]                         # one change (.env or JSON)
roll secrets edit                                               # the folder in $EDITOR, one change
roll secrets set     KEY --generate 48                          # a random value made locally
roll secrets health                                            # every finding, every environment, worst first
roll secrets doctor                                            # this machine's setup, checked
roll secrets check   [.env.example] [--strict]                # keys a template names that the environment lacks
roll secrets scan    [PATH…] | --staged                       # this project's values in files: where, never what
roll secrets render  config.tpl [--out config.yml [--watch --exec CMD]]   # ${KEY} filled in
roll secrets export  [--format dotenv|json|shell|yaml|docker|kubernetes] [--name N --namespace NS]
roll secrets envs    ls | add NAME [--base ENV --ephemeral] | rename OLD NEW | rm NAME | order A B C
roll secrets history [KEY]   roll secrets rollback --to SEQ | --at TIME
roll secrets diff    --from development --to staging | --file .env   # two environments, or a local copy
roll secrets promote --from development --to staging [KEY…]
roll secrets audit   [--key KEY] [--format csv|jsonl]
roll secrets syncs   ls | add KIND --target name=value… | run ID | rm ID
roll secrets sinks   ls | add URL [--from-start] | rm ID
roll secrets purge   KEY [--before N]        roll secrets retention 365|forever
roll secrets insights [--unused]             roll secrets shares [--revoke ID]
roll secrets leases  [--renew ID | --revoke ID]
roll secrets databases ls | add NAME [--role A --role B] [--group G] [--ca FILE] (admin URL on stdin) | rm ID
roll secrets aws-users ls | add NAME USER --role ROLE | rm ID
roll secrets clouds  ls | add-gcp NAME --audience A --service-account SA | add-azure NAME --tenant T --client-id C [--scope S] | rm ID
roll secrets cas     ls | add NAME --domain D… [--allow-ip] [--max-days N] | cert ID | sign ID [CSR|-] [--days N] [--chain]
                     | certs ID | revoke ID SERIAL | rm ID
roll secrets ssh-cas ls | add NAME [--principal P…] [--max-hours N] [--host-domain D…] [--max-host-days N] | key ID
                     | sign-host ID [KEY.pub|-] --host H… [--days N] | known-hosts ID
                     | sign-user ID KEY.pub --principal P… [--ttl-minutes N] | rm ID
roll secrets transit ls | add NAME | rotate NAME | encrypt NAME [--context C] | decrypt NAME [--context C] | rm NAME --confirm NAME
roll secrets transit sign NAME | verify NAME SIGNATURE | hmac NAME | public NAME   # stdin is the input
roll secrets transit import NAME [--vault-version N]                                # the key on stdin
roll secrets kubernetes [--name N] [--namespace NS] [--refresh 1h] [--url URL]   # ESO manifests
roll run [-p project] [-e production] [--path /] [--allow-unresolved] [--watch] -- <command…>
roll ssh [-p project] [-e production] [--cert SSH_CERT] [--identity SSH_KEY] -- <ssh arguments…>
roll run [-p project] [-e production] [--path /] [--env-file .env]… [--allow-unresolved] [--watch] -- <command…>
roll run [-p project] [-e production] [--path /] [--allow-unresolved] [--watch] [--mask | --no-mask] -- <command…>
```

- The project is `-p`/`ROLL_PROJECT` (a name or id), the environment `-e`/`ROLL_ENV`, the path
  `--path`/`ROLL_SECRETS_PATH`. Without them, `roll secrets setup -p shop -e staging [--path /api]`
  remembers them for the current directory and the ones under it, in the user's own
  `~/.config/roll/secrets.json` (never the repo), so `roll run -- npm start` needs no flags there
  (`--show`, `--clear`). Then `development` and `/`. A scoped token (`ROLL_TOKEN`) implies its
  project.
- `roll secrets doctor` checks that setup in one go, each line ok or FAIL: who the token is (and
  the project it's bound to), the directory's project and environment and how many keys it can
  read there, anything there that needs fixing now (expired, missing or exposed keys, from health),
  the keys a `.env.example` names that it lacks, whether a local `.env` still matches (the diff
  above, by fingerprint), and whether staged files hold any of
  the project's values (the scan, names only). It fails when a check does: a first step when
  `roll run` doesn't see what it should.
- `roll run --watch [--interval 5]` keeps the command as a child and restarts it (SIGTERM, then
  SIGKILL after 10 s) when the environment changes. It polls `…/digest`, a hash of the folder's
  effective keys, versions, fingerprints and links, and of what their references reach (in this
  environment or another), that decrypts nothing, issues nothing and isn't audited
  (`Secrets::digest` in process). It compares from the digest each `…/values?digest=1` answer
  carries, which the server takes just before that read (asked for, since it costs about as much
  as the read): a write that lands while the command's values are
  being read (or re-read, or newly referenced) is still a change, never folded into what it
  compares against. The message names the folders that moved (`production:/ changed`). A change
  that doesn't resolve keeps the running command. A live link's resource rotating doesn't change
  the digest.
- `roll secrets edit` opens the folder's own keys as a `.env` in `$EDITOR` (a 0600 file under
  `$XDG_RUNTIME_DIR` when there is one, removed after) and applies what changed as one change
  (`import` with `replace`: a removed line deletes its key; linked and personal keys aren't in the
  file and never deleted). `set KEY --generate N` makes a random value of N letters and digits.
- `roll run` fetches the resolved values (`…/values`), adds them to the child's environment and
  execs the command. It writes no file, and it fails before running anything if a reference is
  unresolved. Variables already set in the shell keep their value unless `--override` is passed.
- What the command prints is masked in logs, as 1Password's `op run` does and GitHub Actions does
  for its own secrets: roll keeps the command as a child, reads the stream, and writes
  `<concealed by roll>` in place of any value it added, so a debug print doesn't put a key in a
  build log. By default stderr when it isn't a terminal, or in CI (`CI` set, where a PTY is a log
  too): it's diagnostics, never data. stdout is data as often as a log (`pg_dump > dump.sql`,
  `| gzip`, a dump over ssh), so it's masked only in CI and never into a file: nothing written as
  data is ever changed unasked. `--mask`, or `ROLL_MASK=1` in a pod's or a unit's environment
  where stdout is the log, masks both streams wherever they go; `--no-mask` neither. **Jenkins**
  writes a step's output to a log file, which roll can't tell from `> dump.sql`: set `ROLL_MASK=1`
  in the job's environment (`environment { ROLL_MASK = '1' }`), or stdout reaches its log unmasked
  while stderr is masked. A value is
  matched as it is, its JSON and URL encodings, each line of a multi-line one that has no spaces (a
  PEM key's lines, not a JSON file's `"type": …` lines), and, in a JSON value (a cloud key file,
  minified or not), the fields named as secrets (`private_key`, `client_secret`, not `token_uri`);
  values under 8 characters (`true`, a port) aren't masked. Output passes through as it comes, except a tail that
  could start a value, held until more comes or the stream ends (never on a timer: a
  block-buffered program can pause in the middle of a value).
- Masking is a guard against accidents, not a boundary. The command can print a value some other
  way (reversed, split, base64, Python's `ensure_ascii` JSON, `%2f`, `+` for spaces), and where
  two values overlap in the output only the first is concealed whole.
- A masked command runs as roll's child. The exit is the command's: its code, or the same SIGINT,
  SIGTERM or SIGHUP re-raised, so a shell loop stops on Ctrl-C. SIGTERM and SIGHUP (`docker stop`,
  systemd) are passed on, with or without `--watch`, and SIGINT unless roll's terminal sent it to
  the command too (so a `kill -INT` aimed at roll while it's in the terminal's foreground isn't
  passed on: send SIGTERM, or signal the command). A process the command leaves in the background gets 1 s of output after the
  command exits (then roll exits and its pipe closes): run daemons with `--no-mask`
  (`roll-cli/src/mask.rs`).
  Measured (release build, 2026-11-14, 400 MB through `cat`): with 300 values of 32 characters
  and a 3 KB key, about 330 MB/s on real logs and docs and 240 MB/s on random base64 (the
  worst case: every byte can start a value); about 1.2 GB/s with a handful of values. Unmasked,
  the same `cat` runs at about 3.5 GB/s. No command prints anywhere near that, so masking costs a
  real command nothing it notices; memory stays under 30 MB.

### Docker builds and Compose

- **A build that needs a secret** (a private registry's token): BuildKit's secret mount, from
  `roll run`'s environment. The value is there for that one `RUN` step and in no layer; a build
  argument (`--build-arg`) would stay in the image's history.

  ```dockerfile
  # syntax=docker/dockerfile:1
  RUN --mount=type=secret,id=npm,env=NPM_TOKEN npm ci
  ```

  ```sh
  roll run -e production -- docker build --secret id=npm,env=NPM_TOKEN .
  ```

- **Compose**: list the variables without values and Compose passes `roll run`'s through, so
  nothing is written to a file:

  ```yaml
  services:
    api:
      environment: [DATABASE_URL, STRIPE_KEY]
  ```

  ```sh
  roll run -e development -- docker compose up
  ```

  `docker compose config` would print them: pass `--no-interpolate` or don't share its output.
- Checked on 2026-11-14 (Docker 29.9, Compose 5.6): the build step saw the value, `docker history
  --no-trunc` and the image's `Config.Env` held none of it, a Compose service saw it, and
  `docker compose config` printed it unless `--no-interpolate`.
### Running through an outage
`roll run --fallback FILE` (or `ROLL_FALLBACK`) also writes every cleanly resolved fetch to `FILE`,
as Doppler's CLI does. When rolld can't be reached, or answers with a 5xx, `roll run` starts from
the file, saying how old it is. A refusal (a revoked token, a 403, an environment that doesn't
exist) never falls back, and neither does a file older than `--fallback-max-age` (default 7 days).
- The file is AES-256-GCM, with a key derived (HKDF-SHA256, the file's own salt, info
  `roll-fallback-v1`) from `ROLL_FALLBACK_KEY` (at least 32 characters: a copied file can be tried
  offline), else from the token. A workload identity's tokens change at every login, so it needs
  `ROLL_FALLBACK_KEY`, and a login that gets no answer goes on to the file.
- "Can't be reached" is a network error or a 5xx, from the login, the project's lookup or the
  values; a 4xx (a revoked token, a customer's key that refused: 423) or an answer that isn't
  roll's never falls back. With `--fallback`, rolld gets 5 s to connect and 30 s to answer.
- The project as asked for (`-p`), the project, environment, folder and fetch time are associated
  data: a file can't be swapped for another project's or environment's, or edited to look fresher.
- It's written 0600 through a new file renamed over the old one. Only what resolved cleanly is
  kept. A dynamic credential in it may have ended with its lease.
- When the project can only be found by asking rolld (a name, or the token's own), the file's own
  project is used, and checked again when it's opened. `--watch` started from a fallback reloads
  the command once rolld answers.

### Runtime resolve for services (roll apps)

roll apps (docs/apps/) deploys a service into an environment and injects that environment's
secrets when the service starts. rolld's `ForApps` (`crates/rolld/src/secrets.rs`) is apps'
`SecretSource`: the service's own project's environment of the same name, resolved as
`app:<svc>`, only where the project stands for the service (`Workspace::trusts`). A project or
environment roll secrets doesn't have is no variables (a service needn't use secrets); an
unresolved reference or a dynamic credential fails the deploy, saying which. It uses one call:

- **In process**: `Secrets::values(project, env, path, true, Some("app:<service>"), &caller)`
  returns `Values`: a map of `Value`s (redacted in `Debug`, zeroized on drop), `problems` (fail a
  deploy on any), `dynamic` (keys holding a short-lived credential: refuse them for a service that
  scales to zero) and `linked` (keys from links, whose addresses a container network may need
  rewritten). The caller is `Caller::consumer("app:<service>")`, or `Secrets::untrusted(project,
  &caller)` for code nobody vetted. Dynamic links issue leases owned by the consumer, and the call
  is audited as `resolve` by `app:<service>`. `Secrets::digest(project, env, path, &caller)` (no
  decryption, unaudited) moves whenever the values would: compare it to decide a redeploy.
- **Preview environments are untrusted themselves.** An ephemeral environment that wasn't made
  `trusted` never reads a protected environment, whoever resolves it: an app deployed into a
  preview's environment as `app:<service>` gets its overrides and the unprotected rest, never
  production's values. Its digest leaves the protected base out the same way.
- **Over HTTP** (another node or process): `POST …/environments/{env}/resolve`
  `{"path": "/", "consumer": "app:<service>", "seal_to": "<X25519 public key>"}`. With `seal_to`
  the answer is one `roll-seal-v1` box (the same box as docs/code/agents.md, "Endpoints"; associated
  data `roll-secrets:<project>:<env>:<consumer>`), so the values are never in the clear outside the
  process that holds the private key. Without `seal_to` it is plain JSON over the internal API.

### Checks, sandboxes and previews

- **Checks**: `secrets = "development"` (or `"staging:/api"`) on a `[checks.<name>]` table in
  `roll.toml` gives the check that environment's values, under the table's own `env`. The repo must be
  on a workspace project's canvas. A protected environment never reaches a check, and neither does
  one with an unresolved reference (the check fails and says which). The consumer is
  `check:<owner/repo>#<check>`. A check's output becomes its summary, so every secret value of 6
  characters or more is replaced by `***` there. A value cut in half by the summary's length limit
  can still show in part.
- **Containers never carry values on a command line**: `docker create --env NAME` takes the value
  from the docker CLI's own environment (owner-readable only), and names docker reads itself
  (`DOCKER_*`, `PATH`, proxies…) go in a private env file. On Fargate the env travels in the run's
  bundle, as before.
- **Sandboxes**: `POST /v1/code/agents/{owner}/sandboxes` takes `secrets: {project, environment}`.
  The project must have one of the owner's repos on its canvas. The values are sealed with the rest
  of the sandbox's env until the backend has them. The consumer is `sandbox:<id>`.
- **Previews**: roll previews (docs/previews.md) makes an ephemeral environment per change (see
  "Ephemeral environments") and resolves it as `preview:<slug>`. A preview runs a change's code, so
  it resolves with `"untrusted": true`: an ephemeral environment based on a protected one gets its
  overrides and links, never the base's values.
- **Untrusted consumers.** Anything that runs code nobody vetted resolves untrusted
  (`Secrets::untrusted` in process, `"untrusted": true` over HTTP): no protected environment is
  read, asked for or reached through a reference, an import or a base, whatever the caller may
  otherwise do. Checks and sandboxes always are.
- **roll tasks** runs functions in the user's own app, so it has no environment to inject. The app
  itself uses `roll run` or the resolve call.

`code_control::SecretEnv` is the one interface roll code uses, and `crates/rolld/src/secrets.rs`
(`ForCode`) implements it.

#### Leaked secrets don't land

A change that adds the value of a secret of its repo's project gets a `secret_leak` blocker, so it
can't be queued or landed: `adds the value of STRIPE_KEY (production): rotate it and remove it from
the change`. The blocker names keys and environments, never the value or the line.

- **What is checked.** The lines the change adds since its merge base (its own diff, so the check is
  O(change)). Each added line gives candidates: every run of `[A-Za-z0-9_\-+/=.:@~]`, the right-hand side
  of `KEY=VALUE` and `key: value` (quotes and a trailing `,` or `;` stripped), and the whole trimmed
  line, kept only if it looks like a credential (`credential_like`): at least 12 characters, two
  kinds of character with a digit or a capital among them, 3 bits of entropy a character, and never
  a URL without a password. Configuration kept as an environment variable (`https://shop.example`,
  `production`, `staging-database`) never blocks a landing; a low-entropy password can slip
  through. Binary files, files over 1 MiB and lines over 4 KiB are
  skipped, and at most 20,000 candidates are checked per change.
- **How it matches.** `Secrets::leaks` (`crates/roll-secrets/src/leaks.rs`) fingerprints each
  candidate with the project's data key and compares it with the stored `fp` of every secret, in
  every environment, that holds a value (plain values and managed links; live and dynamic links
  store none). Nothing is decrypted. Candidates shorter than 8 characters never match, so values
  like `true` or `8080` don't block. Every project that has the repo on its canvas is checked.
- **When.** The answer is computed before a change is queued or landed and when a single change is
  read, and cached per head with the merge base it was found from. A secret set after a head was
  checked is caught when the head or its merge base moves. A repo on no project's canvas, or a
  server without roll secrets, gets no blocker; if roll secrets can't answer, the change isn't
  blocked and the check runs again next time.
- **Values already in a repo.** A change is checked for what it adds, so a value that landed
  before (before the repo was on the canvas, or before the secret was set) is found by a scan:
  `GET /v1/code/repos/{owner}/{name}/leaks` (the repo's managers; in the Secrets surface, Access,
  "Values in code") reads the default branch's head tree with the same candidates and limits
  (text files up to 1 MiB, lines up to 4 KiB, 20,000 files and 200,000 candidates, then
  `truncated`) and answers each match as `{key, environment, folder, path, line}`, never the value. With
  `?history=N` (at most 1,000) it also reads what each of the last N first-parent commits added
  and lists the commits that added a value, `{key, environment, folder, changeset, path}`: a value removed
  since is still in the history anyone with the repo can read, so it's out and needs rotating.
- **Bounds.** A scan reads at most 256 MiB (files for the head, patches for history) and keeps at
  most 32 MiB of candidates, then says `truncated`; two scans run at once per server. It names
  keys and environments of the projects with the repo on their canvas to the repo's managers, as
  a blocker does on a change. A project's check also covers the values other projects share with
  it, each fingerprinted with its own project's key and named by that project
  (`platform/production`).
- **What it tells.** A blocker says a value is a secret's, so a writer of the repo could confirm a
  guess at a value they may not reveal by pushing it. That's accepted: they confirm a guess they
  already hold (never learn a value), only credential-like guesses count, every guess is a pushed
  change in the repo's history, and the alternative, landing a live credential, is worse. A
  project whose secrets its repo's writers must not even confirm keeps the repo off its canvas.
- **Way around.** Rotate the secret and remove the value from the change. For a false positive, an
  admin lands with `bypass` (on a queue repo, the queue bypass; on a direct-land repo, the bypass of
  review rules, which now covers `secret_leak` too), audited as `secret_leak_bypass`.

#### Keys a change starts reading

The same pass over the lines a change adds also picks out the environment variables they read, by
their usual spellings (`process.env.X`, `process.env["X"]`, `import.meta.env.X`, `os.environ["X"]`,
`os.getenv("X")`, `env::var("X")`, `os.Getenv("X")`, `ENV["X"]`, `System.getenv("X")`; upper-case
names, and not the ones platforms set themselves such as `PATH`, `PORT` or `GITHUB_*`). Each one a
standard environment of the repository's project doesn't define (set in any folder, or reaching its
root through an import or its base) becomes a `secret_missing` notice on the change: "reads
SENTRY_DSN, which isn't set in every roll secrets environment". It names the key only, never the
environments, and only on a change of the repo's own (`trusted`) in a repo that isn't public: whoever
can read a change could otherwise guess names in a diff to learn what roll secrets holds. A notice
never blocks a landing, since
a variable can come from elsewhere: it's there so the key is added before the code that needs it
ships. Both carry the `key` (a `secret_leak` blocker its `environments` too, which its author could only
know by holding the value), and the change page links straight to that key in the project that has the repo
on its canvas ("Rotate STRIPE_KEY in roll secrets", "Add SENTRY_DSN in roll secrets").

#### Before a commit: `roll secrets scan`

The same check runs on a laptop, before anything is pushed: `roll secrets scan [PATH…]` walks
files (skipping `.git`, `node_modules`, `target`, binaries and files over 1 MiB) and `--staged`
reads exactly what `git commit` would record. The CLI takes each line's candidates (runs of token
characters, the trimmed line, the right-hand side of `KEY=VALUE`), and `POST …/leaks
{candidates}` (at most 20,000 a call) answers which are the project's values, by fingerprint: the
key, environment and folder, never the value. It prints `file:line KEY (environment)` and exits
non-zero when it found one, so it works as a hook:

```
# .git/hooks/pre-commit
exec roll secrets scan --staged
```

A scan only names values its caller may reveal, and among personal values only the caller's own:
otherwise it would confirm guesses at values the caller can't read. Each scan is audited (`secrets.scan`, the keys it found).

### A local copy against the environment

`roll secrets diff --file .env` (`POST …/environments/{env}/diff`) compares a local `.env` or JSON
copy with the folder: the keys the file adds, the ones whose value differs, the same ones, and the
ones it lacks, by fingerprint on the server, never printing a value; it fails when they differ, so
a stale copy shows before it misleads. Values with references and links' values can't be compared
with a copy and are listed apart. Comparing is a way to test a guess, so it needs `reveal` and is
audited (`secrets.compare`).

### Moving from another manager

`roll secrets import` takes a `.env` file or JSON: an object of `KEY: value` (numbers and booleans
as their text) or an array of `{key, value, comment?}`. Export from the old manager, then import
into each environment:

Or straight from its API, with your own token in its usual variable, read on your machine (never
through roll's server) and imported as one change:

```
DOPPLER_TOKEN=dp.st…  roll secrets import --from doppler --source shop/prd -e production
VAULT_ADDR=https://vault.example VAULT_TOKEN=hvs…  roll secrets import --from vault --source secret/shop -e production
INFISICAL_TOKEN=…  roll secrets import --from infisical --source <workspace id>/prod/api -e production
VAULT_ADDR=… VAULT_TOKEN=…  roll secrets import --from vault --source secret/shop --recursive -e production
roll secrets import --from aws --source prod/shop/ -e production      # your aws CLI's profile or SSO
```

- `--recursive` (Vault) walks a KV v2 tree: each secret goes into its own folder under `--path`
  (`secret/shop/db` → `/db`, the secret at the source itself → `--path`), one change per folder;
  a Vault name that can't be a roll folder (letters, digits, `_`, `-`) stops it, naming it, and
  so do more than 500 secrets, 8 levels or 2,000 paths (import in parts). A secret the token can't
  read stops it too, naming it (only a 404 is "nothing there"), so nothing is left out unsaid;
  secrets are read and folders listed only as Vault lists them, so a token with read on `app/*`
  but not on `app` itself works. Answers are read up to 4 MiB.
- `--from aws` reads Secrets Manager through the user's own `aws` CLI (its profiles, SSO and
  roles; `ROLL_AWS_CLI` names another binary): every secret whose name starts with `--source`
  (500 at most). A JSON object's fields come in as keys; any other string as one key named after
  the secret's last segment (`prod/shop/sendgrid-key` → `SENDGRID_KEY`); binary secrets are left
  out, named; JSON `null` fields too. Two secrets that give one key stop it, naming both (import
  them into different folders), and so do more than 500 under the prefix. Values come back on the
  CLI's output, never on a command line.
- Checked against Vault 1.20 (a three-level tree into three folders) and Secrets Manager on
  LocalStack (a JSON secret, a plain one, and one outside the prefix left alone).

`--map dev=development,stg=staging,prd=production` does a whole project: each `<source>/<name>` (a
Doppler config, a Vault path, an Infisical environment) into its roll environment, one change
each, reported per environment. The dashboard's import dialog shows these commands, since a browser
can't call the other manager's API. Doppler's own `DOPPLER_*` keys are left out; numbers and booleans come in as their text. Hosts
come from the managers' own settings (`DOPPLER_API_HOST`, `VAULT_ADDR`, `INFISICAL_API_URL`).

```
infisical export --env=prod --format=json > prod.json   &&  roll secrets import prod.json -e production
doppler secrets download --no-file --format json > prod.json  &&  roll secrets import prod.json -e production
vercel env pull .env.production --environment=production  &&  roll secrets import .env.production -e production
```

### VS Code

`apps/vscode` is roll secrets for VS Code (as Doppler's, Infisical's and 1Password's extensions):

- **Where the code reads a variable** (`process.env.X`, `import.meta.env.X`, `os.environ["X"]`,
  `os.getenv`, `os.Getenv`, `env::var`, `ENV["X"]`, `getenv`, `System.getenv`,
  `Environment.GetEnvironmentVariable`), a hover says which environments set it in the folder, and
  which this token can read don't. A warning marks a key roll sets somewhere but not in the
  workspace's environment (`STRIPE_KEY isn't set in staging (set in development)`); names roll
  doesn't know at all (`PATH`, `NODE_ENV`) are left alone. Names are completed where a name goes.
- **In a `.env` file**, a line with a plain value offers "Replace with `roll://ENV/KEY`" when the
  environment has the key, and "Move to roll": after a confirmation that names the project,
  environment and folder (and says when it replaces roll's value), the value is set in roll as one
  change and the entry becomes its reference, which `roll run --env-file` resolves. The file is
  read as the CLI reads it (a double-quoted value over several lines, a comment after its quote,
  the last entry of a key set twice; nothing is offered on a file the CLI would refuse), and the
  whole entry is replaced, every line of a multi-line key; if the entry changed while roll
  answered, it's left alone and the message says so. A protected environment's 409 says to open a
  change request in the dashboard.
- **What it reads**: the folder's grid (`GET …/grid`: names and which environments set them, never
  a value; `read` is enough), cached a minute. It writes only for "Move to roll". A value never
  goes into a message, a log or the extension's state.
- **Setup**: what `roll secrets setup` remembers for the folder (`~/.config/roll/secrets.json`),
  or the `roll.project`, `roll.environment` and `roll.path` settings. All of them, and
  `roll.apiUrl`, come from your own settings only, never a repository's `.vscode/settings.json`
  (which could send your token elsewhere, or point "Move to roll" at another project or
  environment), and a confirmation names the project it writes to. The status bar's
  `roll: shop · staging` chooses another environment for the workspace. The token is a project
  token kept in VS Code's secret storage (`roll secrets: Sign in`), or `ROLL_TOKEN`.
- Checked in code-server 4.96 (VS Code 1.96) against a live rolld: the hover, the warning, and a
  `.env` value moved into staging and replaced by `roll://staging/STRIPE_KEY`.

### Backup and restore

`roll secrets keygen` makes an X25519 key pair (the secret key in a 0600 file, the public key
printed); `roll secrets backup --to <public key> --out shop.backup` writes the whole project:
every environment (with its base, labels, expiry, trust, protection, approvals and age limit), its
folders, its own keys with their values, comments, tags and links, its imports and the project's
required keys. The archive is sealed to that key (`roll-seal-v1`, associated data
`roll-secrets-backup:<project>`), so the file is safe at rest anywhere and only the key's holder
opens it; `POST …/backup {recipient}` (admin, audited `project.backup`) is the call. Linked keys
carry their link and no value: a restore issues its own credentials. Personal values, history,
leases, tokens and identities aren't in it.

Environments → Backup downloads the same file from the dashboard, sealed to a public key pasted
there: the browser never holds the secret key.

`roll secrets restore shop.backup --key roll-secrets-backup.key` opens it on the CLI's machine and
loads it into the selected project, which must have no secrets yet (`POST …/restore` with the
opened archive; admin, audited `project.restore`), on this server or another. Values are written
before any environment is protected, so a restore isn't a change request; what can't be restored
(a link to a resource that project doesn't have, an import of another project's folder) comes back
as a warning, by name. Disaster recovery, and moving a project between roll servers.

### Checking against a template

`roll secrets check .env.example` compares the keys a `.env`-style template names (`KEY=…`,
`export KEY=…`, a bare `KEY`; not comments) with the keys the environment has, its imports and base
included, by name only: it reads no value, so `read` is enough. It lists each `missing` key and each
`extra` one, and fails when one is missing (with `--strict`, also when there's an extra), so a CI
step catches a variable the code started reading before an environment has it.

### References in files

As 1Password's `op://`: a file committed to the repository holds `roll://` references where values
would go, so it carries the shape of the configuration and no value.

- `roll://ENV/KEY` is a key in this project's environment, `roll://ENV/folder/sub/KEY` one in a
  folder, `roll://PROJECT@ENV/KEY` one in another project (its name, or its `prj_…` id with a
  scoped token; a name that more than one project has, under different owners, is refused: give
  the id). Segments are letters, digits, `-`, `_` and `.` (never `.` or `..`).
- In the dashboard, a key's menu has **Copy reference**: the reference to the folder it's shown
  in, so it resolves to what that folder reads (its own value, an import, an inherited one). On a
  page that isn't https the browser has no clipboard, and the note shows the reference to copy.
  (The desktop app's deep links also start `roll://`, as `roll://agents/thread/…`; the two never
  meet: references are read from files and templates, deep links from the OS.)
- `roll run --env-file .env` (repeatable) adds a dotenv file's entries to the environment's:
  plain values as written, references resolved, and the file's entries win over the environment's
  (the shell's own variables still win over both unless `--override`). A file such as
  `DB_URL=roll://production/DATABASE_URL` and `PORT=8080` runs with production's value and 8080.
- `roll secrets render` resolves references in a template where they stand (a reference runs to
  the first space, quote, bracket or comma) and fills `${KEY}` around them; neither kind of value is
  read again as a template.
- Each environment and folder is one read through the same audited `values` route, under the
  caller's own access: a reference to an environment the token can't read fails, naming it, and so
  does one to a key that isn't set or doesn't resolve. Nothing new runs on the server.
- `roll run --watch` and `roll secrets render --watch` follow every environment a file references
  too (their digests), and renew each dynamic credential's lease at its own project.
- A dotenv file is one `KEY=value` per line (quoted values on one line; a quote that runs past its
  line is refused), and its errors name the line, never its text.

### Templates

For a tool that reads a config file rather than its environment, `roll secrets render
config.tpl --out config.yml` fills each `${KEY}` from the environment (resolved, so references,
imports and links count) and writes the file 0600; without `--out` it prints it. A key with no
value fails the whole render, naming every such key, so a half-filled file is never written.
`\${KEY}` stays a literal `${KEY}`, and anything else (`$KEY`, `${KEY:-default}`) is left as
written, so shell snippets in the file survive. The values are read through the same audited
`values` route `roll run` uses; nothing new runs on the server.

With `--watch` it keeps running, like `roll run --watch`: it checks the environment's digest every
`--interval` seconds (default 5), renders again when it changes, and replaces the file whole (a
file beside it, then a rename, so a reader never sees half of one) only when the text changed;
then `--exec "systemctl reload nginx"` runs through `sh -c`. A render that fails (a key deleted)
leaves the last good file in place and says why. Dynamic credentials' leases are renewed every 10
minutes.

### Finding a key

`roll secrets find STRIPE_KEY` (or `GET /v1/secrets/search?key=`) lists every project, environment
and folder that holds a key, among those the caller may read (a token, its own project's), with its
version, when it changed and whether it's linked: never its value. `STRIPE_*` finds every key that
starts with `STRIPE_`. For rotating an organisation-wide key: where is it, so nothing is missed.
In the dashboard, "Set everywhere…" under an exact key's places then gives each place one new value:
one change per environment, a change request where it's protected, and a linked place rotated by
its provider instead; each place's outcome is listed, so a refusal in one doesn't hide the rest.

### Sharing a value once

`roll secrets share STRIPE_KEY [--views 1] [--ttl 24h] [--note …]` (or a value on stdin) prints a
link that opens once, or `--views` times, before it expires (5 minutes to 7 days), for handing a
credential to someone outside roll. `roll secrets open <link>` and the dashboard's share page
open it.

- With `ROLL_WEB_URL` (the dashboard's address) set on rolld, `POST …/shares` answers `link`
  (`<dashboard>/share/<id>`), and `roll secrets share` prints `link#key`.
- **The server never sees the value.** The client encrypts it with a random 256-bit key
  (AES-256-GCM, `roll-share-v1` as associated data) and puts the key in the link's `#fragment`,
  which browsers never send. The server keeps the ciphertext, and drops it at the last open, at
  expiry or when it's revoked.
- **Opening is a `POST`** (`/v1/secrets/shares/{id}/open`), so a chat's link preview, which only
  `GET`s, can't use it up. `GET /v1/secrets/shares/{id}` shows the note and the opens left. Both
  routes are public; the id is 120 random bits.
- Creating, opening (with the opener's address) and revoking are in the project's audit log.
  `GET …/shares` lists a project's shares (never their ciphertext); `DELETE …/shares/{id}` revokes
  one. A project has at most 200 open at once.

## Syncs

A **sync** pushes an environment's resolved values to a platform outside roll, one way, whenever
they change: GitHub Actions secrets, GitLab CI/CD variables, Vercel, Netlify, Render, Fly.io,
Railway, Heroku, DigitalOcean App Platform, CircleCI, Bitbucket Pipelines, Terraform Cloud, Cloudflare Workers, AWS Parameter Store and Secrets Manager, Google Secret Manager, Azure Key Vault, a Kubernetes Secret, an endpoint
of the team's own, or a deploy hook that only hears that something changed. Infisical's "secret
syncs" are the reference. roll secrets stays the source of truth: a sync never reads from its
target, and a value edited there is overwritten at the next
change of that key. Before one is added, `POST …/syncs/preview` (`roll secrets syncs add … --preview`, the form's
Preview) says what it would push, names only, without calling the platform: its names after `keys`
and `prefix`, the credential keys it reads (never pushed), and any credential or listed key the
folder doesn't have.

roll-secrets knows no platform. Each kind is a `SyncTarget` (`crates/roll-secrets/src/syncs.rs`)
that rolld registers (`crates/rolld/src/secrets/syncs.rs`), as link providers are:

```rust
pub trait SyncTarget: Send + Sync {
    fn kind(&self) -> &str;                                   // "github", "webhook", "notify"
    fn label(&self) -> &str;
    fn fields(&self) -> &[SyncField];                         // what the dashboard asks for
    fn values(&self) -> bool { true }                         // false: pushes carry key names only
    fn signed(&self) -> bool { false }                        // a signing secret, shown once
    fn credentials(&self, target: &Json) -> Vec<String> { vec![] } // keys holding its token
    // On create and before every push; `resources`: the project's canvas, as (kind, ref).
    fn check(&self, project: &str, resources: &Resources, target: &Json) -> Result<()>;
    // `push`: the values by target name, which names changed, which were removed, the digest,
    // the signing secret, the resolved credentials.
    fn push(&self, target: &Json, push: &Push) -> Result<()>;
}
```

A sync is `{id: "syn_…", environment, path, kind, target, keys, prefix, status, last_digest,
last_synced_at, error}`:

- **What it pushes.** The folder's effective values (own, imported, inherited from a base),
  resolved, as `roll run` gets them. `keys` limits it to some keys (default: all); `prefix` is put
  in front of every name at the target (`prefix: "APP_"` pushes `DB_URL` as `APP_DB_URL`). Personal
  values never sync, and neither do dynamic links (a short-lived credential would expire at the
  target): a dynamic key, or a reference that can't resolve, is left out and named in `error`.
- **When.** The sweeper (every node, every 30 s) compares each sync's environment `digest` (the
  hash `roll run --watch` polls) with the sync's `last_digest`. A sync whose digest moved is
  claimed with a compare-and-swap on its `running_until` (5 minutes), so two nodes never push one
  sync at once. A new sync first pushes at the next sweep. `POST …/syncs/{id}/run` pushes now, every key, whatever the digest says (also after
  a live link's resource rotated, which moves no digest).
- **Only what changed.** A sync remembers a stamp per name it pushed (the secret's id, version,
  fingerprint and link: no value). A push carries the values of every synced key and says which
  names `changed` since the last push, and which were `removed`: synced before, gone now (deleted,
  or left out by `keys`). Targets that set one value per call (GitHub) send only the changed ones
  and delete the removed ones at the target.
- **As whom.** A sync resolves with the access of whoever created it, looked up at every push: the
  operator, or the scoped token that made it (a revoked or expired token stops the sync with an
  error). So a reference to an environment its creator can't reveal stays unresolved. Every push is
  audited as `sync.push` by `sync:<id>`, with the keys it pushed or removed (roll's names, without
  the prefix) and never a value; a push that fails is audited too.
- **Errors** name keys and the target, never a value: `github owner/app: 404 Not Found`,
  `DB_URL: ${staging.X} doesn't exist`. A failed push keeps the previous `last_digest` and is tried
  again after 5 minutes. Deleting a sync leaves what it pushed at the target. Deleting its
  environment or project deletes the sync.
- **Who.** Creating, deleting and running a sync need `admin` on its environment: it sends values
  out of roll. Listing needs `read` and shows the syncs of environments the caller can read.

| Kind | Target | Pushes |
| --- | --- | --- |
| `github` | `{repo: "owner/name", environment?: "production"}` | Repository Actions secrets, or a GitHub environment's secrets, through roll's GitHub App |
| `webhook` | `{url, recipient}` (a base64url X25519 public key) | Every synced value, sealed to `recipient`, POSTed to `url` |
| `notify` | `{url}` | No values: that the environment changed, and which keys, POSTed to `url` (deploy hooks) |
| `vercel` | `{project: "<id or name>", team?: "<team id>", target: ["production" \| "preview" \| "development", …], token_key: "VERCEL_TOKEN"}` | A Vercel project's environment variables (`type: "encrypted"`) for those environments |
| `cloudflare` | `{account: "<account id>", script: "<worker>", token_key: "CLOUDFLARE_API_TOKEN"}` | A Worker's secrets (`secret_text`) |
| `aws-ssm` | `{region, prefix: "/shop/production/", access_key_id_key, secret_access_key_key, kms_key_id?}` | Parameter Store `SecureString`s named `<prefix><name>` |
| `google-secret-manager` | `{project, prefix?, service_account_key}` | one Secret Manager secret per key, `<prefix><KEY>`, each change a new version |
| `azure-key-vault` | `{vault, tenant_id, client_id, client_secret_key}` | one Key Vault secret per key (`_` becomes `-`) |
| `aws-secrets-manager` | `{region, secret_name: "shop/production", access_key_id_key, secret_access_key_key, kms_key_id?}` | one Secrets Manager secret: the environment's values as a JSON object |

### `github`

- The repo must be the GitHub repo of a roll repo on the project's canvas (connected through the
  App), checked when the sync is made and before every push, so a project can't write into a repo
  of another tenant that the App happens to reach. Without the App (`ROLL_GITHUB_APP_ID`…),
  creating one answers 400 and says the App isn't configured.
- Each value is encrypted for GitHub with the repo's (or the environment's) public key as a
  libsodium sealed box (`crypto_box_seal`: X25519, XSalsa20-Poly1305, nonce BLAKE2b-24(epk ‖ pk);
  `code_github::seal_secret`), then `PUT …/actions/secrets/{name}` or
  `PUT …/environments/{environment}/secrets/{name}`. Removed names are deleted (a 404 counts as
  done).
- GitHub's names are `[A-Za-z_][A-Za-z0-9_]*` and may not start with `GITHUB_`; a key that would is
  an error naming it (rename it or use `keys`/`prefix`).

### Platform credentials (`vercel`, `cloudflare`, `aws-ssm`, `aws-secrets-manager`, `google-secret-manager`, `azure-key-vault`)

A platform's own credential is never in the sync. The target names **keys of the sync's own
environment and folder** that hold it (`token_key`; for AWS `access_key_id_key` and
`secret_access_key_key`, an IAM user's key the team stores, not a roll bucket's managed key).
They are resolved at each push like any value (references work), never pushed themselves (they're
left out of the synced keys, and `keys` can't list them), and never stored, logged or put in an
error. A credential key that isn't set, is empty or doesn't resolve fails the push with an error
naming it. The calls go to the platforms' fixed hosts (`api.vercel.com`, `api.cloudflare.com`,
`ssm.<region>.amazonaws.com`; `region` is `[a-z0-9-]`), with timeouts, no redirects and at most
1 MiB of an answer read; an error carries the platform's status and error code, never its message.

- **`vercel`**: changed names are upserted in batches of 50 (`POST /v10/projects/{project}/env?upsert=true`,
  `type: "encrypted"`, the target's environments). A removed name is looked up by id
  (`GET …/env`) and deleted (`DELETE /v9/projects/{project}/env/{id}`) only where every
  environment it serves is one of the sync's: a variable that also serves another environment is
  the team's own. `target` may also be a comma-separated string (the dashboard's form). Values are
  at most 64 KB.
- **`cloudflare`**: `PUT /client/v4/accounts/{account}/workers/scripts/{script}/secrets` with
  `{name, text, type: "secret_text"}` per changed name, `DELETE …/secrets/{name}` per removed one
  (a 404 counts as done). Values are at most 5 KB.
- **`aws-ssm`**: `PutParameter` (`SecureString`, standard tier, `Overwrite`, `KeyId` when
  `kms_key_id` is set) and `DeleteParameter` (`ParameterNotFound` counts as done) over AWS JSON 1.1,
  signed with SigV4 by hand from s3-proto's primitives (no AWS SDK in rolld). `prefix` is a path
  that starts and ends with `/`, not under `/aws` or `/ssm`. A value over 4 KB is an error naming
  it.
- **`google-secret-manager`**: `service_account_key` names the key holding a service account's JSON
  key file. rolld signs a JWT with its private key (RS256, `ring`) and trades it at Google's token
  endpoint (`oauth2.googleapis.com`, never the `token_uri` the file names) for an access token, then
  per changed key: `GET` the secret, create it when missing (labelled `managed-by=roll-secrets`,
  automatic replication), and `addVersion` with the value; per removed key, delete the secret only
  when it carries that label. A secret that exists without the label is someone else's: roll never
  writes to it. Errors carry Google's `error.status`, never an answer.
- **`azure-key-vault`**: rolld signs in as the app registration (client credentials at
  `login.microsoftonline.com`, scope `vault.azure.net`), with its client secret from the key
  `client_secret_key` names, then per changed key: `GET` the secret (a name's `_` becomes `-`, since
  Key Vault allows only letters, digits and `-`, and roll's keys never hold a `-`), and `PUT` the
  value with the tag `managed-by=roll-secrets`; per removed key, delete it only when it carries the
  tag. A secret roll deleted earlier is kept by the vault's soft delete: setting it again recovers it
  first, once the deleted secret's tag shows it was roll's. Whether a secret is roll's must be known
  before anything is written or deleted: a refused read stops the push. One that exists without the tag is someone else's, never written. Errors carry Key Vault's
  `error.code`, never an answer.
- **`aws-secrets-manager`**: one secret holding the environment's values as a JSON object (what an
  app reads with one `GetSecretValue`), sent whole on each push, so a removed key is simply gone
  from it. `DescribeSecret`, then `PutSecretValue` on a secret roll created (tagged
  `managed-by=roll-secrets`) or `CreateSecret` (with that tag, and `KmsKeyId` when set); a secret
  that exists without the tag is someone else's, and roll never replaces it. Deleting the sync
  leaves the secret. Same SigV4 by hand (service `secretsmanager`); over 64 KB of JSON is an error.

### `render`, `netlify`, `fly`, `railway`, `heroku`, `digitalocean`, `circleci`, `bitbucket` and `terraform-cloud`

- **`render`**, `{service, token_key}`: a Render service's own environment variables (not a linked
  environment group's). A changed key is set (`PUT /v1/services/{id}/env-vars/{key}`), a removed one
  deleted; the key holding an API key is a secret of this environment and folder.
- **`netlify`**, `{account, site, context?, token_key}`: a Netlify site's variables, one deploy
  context's values (`all` by default; `production`, `deploy-preview`, `branch-deploy`, `dev`). A
  changed key sets that context's value (or creates the variable with it), a removed key deletes
  the variable.

- **`fly`**, `{app, token_key}`: a Fly.io app's secrets through Fly's GraphQL API, as flyctl sends
  them (`setSecrets` for changed keys, `unsetSecrets` for removed ones; a macaroon deploy token goes
  as `FlyV1 …`, anything else as `Bearer …`). Setting secrets makes a release, as `fly secrets set`
  does.
- **`railway`**, `{project, environment, service?, token_key, token_kind?}`: a Railway service's
  variables in one environment (the environment's shared variables without `service`), through its
  public GraphQL API (`variableCollectionUpsert`, `variableDelete`). The token is a project token
  (`Project-Access-Token`) by default, or an account token with `token_kind: "account"`.
- **`circleci`**, `{project: "gh/acme/web", token_key}`: a CircleCI project's environment variables
  (`POST …/project/{slug}/envvar` per changed name, `DELETE` per removed one, `Circle-Token`).
  CircleCI keeps no tag on a variable, so only names this sync pushed before are ever removed.
- **`bitbucket`**, `{workspace, repo, environment?, token_key}`: a repository's Pipelines variables,
  or one deployment environment's (its uuid), all secured. The sync lists them for their uuids
  (following `next` only on Bitbucket's API host), then `PUT`s an existing one, `POST`s a new one and
  `DELETE`s a removed one.
- **`terraform-cloud`**, `{workspace_id, category?, token_key}`: a Terraform Cloud workspace's
  variables (`category` `env`, the default, or `terraform`), all `sensitive`, over its JSON:API
  (`…/workspaces/{id}/vars`). Terraform Cloud keeps no tag on a variable, so roll describes its own
  as `managed by roll secrets` and never writes to or deletes one without that description.
- **`digitalocean`**, `{app, component?, token_key}`: an App Platform app's environment variables,
  app-wide or one component's (a service, worker, job, function or static site, by name). The sync
  reads the app spec, replaces only the keys it pushes (as encrypted `SECRET`s, run and build time),
  drops the keys it removes, leaves every other variable as it came (already-encrypted values go
  back unchanged), and puts the spec back. Updating the spec redeploys the app.
- **`heroku`**, `{app, token_key}`: a Heroku app's config vars, in one `PATCH /apps/{app}/config-vars`
  per push (changed names with their values, removed ones as `null`). A failure names Heroku's error
  `id`, never the answer, which echoes the vars.

Like Vercel's, they use the platforms' fixed API hosts, and a GraphQL answer's `errors` fail the
push by code, never with a value.

### `gitlab`

A GitLab project's CI/CD variables in one environment scope, `{project, environment_scope?,
token_key, base_url?}`: the project's id or `group/project`, the scope (`*` by default), the key of
this environment and folder holding an access token with the `api` scope, and a self-managed
instance's public URL (gitlab.com by default; fetched like a webhook, `PublicOnly`). A changed key
is updated in that scope, or created; a removed one is deleted. Values are raw (no `$` expansion),
and masked when GitLab can mask them (8 characters or more, no spaces), so they stay out of job logs.

### `kubernetes`

One Secret in a cluster, `{server, namespace, secret, token_key, ca_key?}`: the API server's
public `https://` URL (fetched like a webhook: `PublicOnly`, no redirects), the namespace and the
Secret's name, and the keys of this environment and folder holding a service account token (with
`create`, `get` and `update` on that Secret) and, optionally, the cluster's CA in PEM. Each push
writes the Secret's `data` as exactly the synced keys (removed ones are gone), labelled
`app.kubernetes.io/managed-by=roll-secrets` and annotated with the project, environment and
folder. roll never replaces a Secret without that label, so it can't clobber one of the team's own.
The synced values must fit a Secret's 1 MiB. Pods read it as usual (`envFrom.secretRef`); a pod
restart or a reloader picks up a change.

#### Pulling instead: External Secrets Operator

A cluster whose API server isn't public (a private EKS endpoint, a cluster on a private network)
can pull instead, with [External Secrets Operator](https://external-secrets.io)'s webhook
provider: the cluster calls roll, and roll never calls the cluster.
`roll secrets -e production [--path /api] kubernetes [--namespace shop] [--url https://…] | kubectl
apply -f -` makes a token for the environment and folder and prints all three objects below; the
dashboard's Export menu has the same ("External Secrets Operator…", the manifests shown once). One
`SecretStore` per project serves a whole environment (`dataFrom`) and single keys (`data` with
`property`). For a tokenless setup, roll's own operator (#36) pulls with the cluster's
identity instead; this is for clusters that already run ESO for their other managers:

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: roll-token
  labels:
    external-secrets.io/type: webhook   # ESO reads only labelled Secrets
stringData:
  token: rlst_…                         # roll secrets -p shop tokens create k8s --for-env production
---
apiVersion: external-secrets.io/v1
kind: SecretStore
metadata:
  name: roll-shop
spec:
  provider:
    webhook:
      url: "https://roll.example/v1/secrets/projects/prj_…/environments/{{ .remoteRef.key | urlquery }}/values?dynamic=false&strict=1"
      headers:
        Authorization: "Bearer {{ .auth.token }}"
      result:
        jsonPath: "$.values{{ with .remoteRef.property }}.{{ . }}{{ end }}"
      secrets:
        - name: auth
          secretRef:
            name: roll-token
---
apiVersion: external-secrets.io/v1
kind: ExternalSecret
metadata:
  name: shop-production
spec:
  refreshInterval: 1h
  secretStoreRef: { name: roll-shop, kind: SecretStore }
  target: { name: shop-production }
  dataFrom:
    - extract: { key: production }        # every key; or data: [{secretKey: url, remoteRef: {key: production, property: DATABASE_URL}}]
```

- The token is a project token for the environment (`--for-env`); each refresh is one audited read
  by it (`secrets.export`), so a short `refreshInterval` fills the audit log.
- A folder is `&path=/api` on the store's URL (a store per folder).
- `dynamic=false`: ESO can't renew a lease, so a dynamic key would be a new credential on every
  refresh that lapses on its own. With it, a dynamic key is left out (named in `dynamic`); link it
  as `managed` (one credential roll rotates) or `live` instead, which ESO's next refresh follows.
- `strict=1`: a key that doesn't resolve (a reference to a key that's gone, a live link whose
  resource doesn't answer) fails the whole read with 409 `unresolved`, naming the keys. ESO keeps
  the Secret it has and marks the ExternalSecret not synced, instead of writing an empty value
  over a working one. Dynamic credentials such a read issued are revoked with it.
- `urlquery` on the key: whoever may make an ExternalSecret can't add their own query
  (`staging/values?dynamic=true&x=`) to the store's URL.
- Checked against ESO 2.15 on k3s 1.33: `dataFrom` and `data` from one store, and a change in roll
  reaching the Secret on the next refresh.

### `webhook` and `notify`

Both POST JSON to a public HTTPS `url` the creator chose: fetched through the `PublicOnly` resolver,
a literal private host refused, no redirects, 15 s timeout, at most 64 KiB of the answer read (and
ignored). Any 2xx is success. `ROLL_CODE_WEBHOOK_ALLOW_PRIVATE` (local development only) allows
`http` and private addresses.

Each sync of these kinds has a **signing secret** (`whsec_…`), made when the sync is created, shown
once in the answer, and stored sealed with the project's data key. Every request carries:

```
X-Roll-Sync: syn_…
X-Roll-Timestamp: <unix seconds>
X-Roll-Signature-256: sha256=<hex HMAC-SHA256(secret, "<timestamp>.<body>")>
X-Roll-Delivery: dlv_<32 hex>   (the same on every retry of this delivery)
```

It's the scheme of every roll webhook (roll code's, roll-access's audit sinks), so one receiver
verifies them all.

- `webhook` body: `{sync, project, environment, path, digest, at, changed, removed, context,
  sealed}`. `sealed` is a `roll-seal-v1` box (see "Runtime resolve") to `recipient` of the JSON
  object of every synced name and value; `context` is `roll-secrets-sync:<sync id>`. Only the
  holder of the recipient's secret key reads the values, so a team can write a sync for any
  platform without roll knowing it.
- `notify` body: `{sync, project, environment, path, digest, changed_keys, removed_keys, at}`. No
  value, sealed or not, and nothing is resolved or revealed for it: a deploy hook ("redeploy when
  production's secrets change").

## Governance

### Access

The roll-wide permission model (docs/permissions.md, `crates/roll-access`, branch
`feature/roll-mcp`) decides who may do what: rolld's gate classifies each request by its route
pattern and method into the product `secrets`, a resource (`{project, environments, paths}`) and one
action:

| Action | Routes |
| --- | --- |
| `read` | names, metadata, versions, history, compare, folders, imports, exports (listing), what's shared with the project, requests, links, leases, required keys, listing syncs, insights, the audit log |
| `reveal` | `…/values`, `…/secrets/{key}/value`, `…/versions/{v}/value`, `…/requests/{id}/values`, `…/resolve`, renewing a lease (whoever could read the credential), creating a one-time share (how a value leaves roll) |
| `write` | set, batch changes, rotate, rollback, promote, folders, imports (a project's own), create an environment, open or apply a request |
| `delete` | a secret, an environment, ephemeral environments by label, a lease |
| `admin` | rename or reorder environments, protection and approval policy, reviewing a request, syncs (create, delete, run), tokens and identities (listing too), audit sinks, revoking another's one-time share, exports (sharing a folder with another project), imports from another project, purge and retention, required keys (setting), making a trusted preview |

`POST /v1/secrets/login` (workload identities) and the one-time share links (`/v1/secrets/shares/…`)
are public: the OIDC token, or the key in the link, is the credential.

Values are returned only by the `reveal` routes, so the gate can classify a request without reading
its body. A route that touches a second resource (the source of a promote, a reference into another
environment, a path inside a batch) checks it in process through the same evaluator. The rules
that follow from it:

- **Nothing reaches an environment its caller can't read.** A reference, an import and an ephemeral
  environment's base each show through only where the caller may read (for a listing) or reveal
  (for values) the other environment. Making an ephemeral environment needs `reveal` on its base.
- **Links are an admin's choice.** A link reaches a resource's credentials as the operator, so
  setting one (or promoting one) needs `admin` in that environment, and so does rotating a
  resource's own credential (`live` links: it affects every environment that links it). A link
  works only while its resource is on the project's canvas, checked on every read, issue and
  rotation.
- **Protected environments.** Changing their imports needs `admin` (it changes what they resolve to
  without a change). Checks and sandboxes resolve as a consumer that may not touch any protected
  environment, so none reaches them through a reference, an import or a base either.
- **Folders.** A caller limited to some folders sees only their changes, change items, requests,
  audit rows and the leases of their secrets.
- **Fingerprints** are shown only to a caller who may reveal the value: a writer could otherwise
  test guesses against values it can't read. The grid's and compare's statuses still say whether
  two environments match, to a caller who can read both.
- Token scopes name environments by slug: a token for `pr-117` reaches a new `pr-117` made after the
  old one was deleted (it needs `reveal` on the new one's base to make it).

rolld's gate decides each request from the route (roll-access's catalogue). The checks it can't
make from the request alone (a second environment, a path inside a batch, each finding of a
filtered answer) go through one trait in `crates/roll-secrets/src/access.rs`, which every caller
carries:

```rust
pub trait Grants: Send + Sync + Debug {
    fn allows(&self, action: Action, at: &Scope<'_>) -> bool;     // may they, here
    fn project(&self) -> Option<&str>;                            // the one project they're limited to
    fn untrusted(&self) -> bool;                                  // code nobody vetted
    fn reads_code(&self, project: &str, repo: &str) -> bool;      // health's code check
}
```

It is the operator (`Everything`), a roll-access grant (`GrantGrants`, for `rlt_…` tokens and
`rlo_…` OAuth connections: `ask` inside the product is a no), or one of the project's own tokens.

Secrets accepts the operator (`roll_auth::Operator`, or any request when rolld has no operator
token), signed-in people on their own projects (below), roll-access's grants, and its own **scoped tokens** (`rlst_…`, created under `…/tokens`) for one project, a list of
environments (or all), a list of path prefixes (or all), the actions above and an optional expiry.
A token can be pinned to client addresses (`allowed_ips`: up to 20 addresses or CIDRs, IPv4 or
IPv6): from anywhere else it's refused (403), checked on every request against the connection's
peer address (or `X-Forwarded-For` where rolld trusts it), as Infisical's trusted IPs and Doppler's
IP restrictions do. An identity can be pinned the same way: a login from elsewhere is refused, and
the token it gets carries the same list. `roll secrets tokens create … --allow-ip 203.0.113.0/24`.
Tokens are stored as SHA-256 hashes, looked up on every request and never cached. roll-access's
scoped tokens (`rlt_…`) and OAuth connections (`rlo_…`) reach secrets too: rolld's gate classifies
the request, and secrets asks the same `Grant` for every second check (`GrantGrants`). The `rlst_`
tokens are to become roll-access's (Next).

#### People (signed-in accounts)

Someone signed in to roll (docs/accounts.md) who isn't an admin manages the secrets of the
workspace projects they own (an admin's session is the operator):
rolld's gate lets a session through `/v1/secrets` (`Access::UserOrCredential`: a session, or a
request carrying a credential secrets checks), with `X-Roll-CSRF` on writes. Their caller is
`OwnProjects`: everything on their own projects as the workspace has them on this request, nothing
on anyone else's, which answers 404 as the workspace does (sharing a folder with a project they
can't see is refused the same way as with one that doesn't exist). Search, findings and
`health --all` keep only their projects. The audit log names the person by user id (`usr_…`), and
their personal overrides are theirs. The deployment key's state (`GET /v1/secrets/health`) is the
operator's.

What a person made acts later only while they can sign in and still reach its project (an admin
then, or its owner now): their tokens, any token one of those made, their identities' logins and
their syncs. rolld asks roll-accounts on every use (`People`); a lookup that fails is an error, not
a disabled account. A sync a person made pushes as them, as they are then.

Anyone may put any resource on their own canvas, so a project reaches a resource only where the
workspace trusts it to (`Workspace::trusts`, docs/workspace.md "What a canvas stands for"): the
operator's and an admin's projects their whole canvas, a person's only repos under their own roll
code owner (`ada/…`).

- **Links, audit sinks and syncs to roll's products** (`members`) take only what the project is
  trusted for: a link reaches a resource's credentials as the operator. A person's project can link
  its own rotators. This lifts per product once each says who owns a resource.
- **roll code and roll previews** (checks, previews and sandboxes getting values, leak blockers,
  missing-key notices) count a project only for repositories it's trusted for: otherwise anyone
  could hand a stranger's previews their variables (say, a `DATABASE_URL` pointing at their own
  server), or watch a stranger's landings for their values.

### Workload identities (OIDC)

A CI job or a cluster logs in with its own short-lived OpenID Connect token instead of holding a
`rlst_` token, as Infisical's OIDC auth, Doppler's identities and Vault's JWT auth do: nothing
long-lived sits in a CI system's settings, and a leaked log or fork can't reuse a login past its
minutes.

- **An identity** (`…/identities`, admin) names an `issuer` (the token's `iss`), an `audience`
  (`aud`, default this server's URL from the CLI and the dashboard, so a token minted for one roll
  server isn't good at another), a `subject` (`sub`, or a pattern of it where `*` stands for
  any run of characters), up to ten other `claims` that must match (`ref`, `environment`,
  `repository_owner_id`…), and the scope of what a login gets: environments, folders and actions
  like a token's (never more than its creator has) for `ttl_minutes`, 5–60 (default 15).
- **Shared issuers.** GitHub Actions, GitLab.com, Terraform Cloud, Buildkite and Google sign for
  every tenant they host, so an identity of theirs must name one tenant literally in its subject
  (`repo:acme/web:…`, `project_path:group/project:…`, `organization:acme:…`, a Google service
  account's numeric id exactly): `repo:*` would let anyone's job in. Narrow further with claims
  (`ref=refs/heads/main`, or GitHub's `environment`). A token that lives longer than a day is
  refused. Key fetches are one at a time per issuer, and a failed one isn't retried for 30 s.
  Logins are capped at 120 a minute per identity and 240 per client address (429 `rate_limited`),
  counted per node before any work.
- **Pinned keys, for an issuer roll can't reach.** A cluster signs its service accounts' tokens as
  `https://kubernetes.default.svc.cluster.local` (k3s, kind, kubeadm, a private EKS endpoint), an
  issuer roll can't and shouldn't fetch. An identity can carry the issuer's public keys instead
  (`jwks`, a JWKS document up to 64 KiB with at least one RS256 or ES256 key; as Vault's
  `jwt_validation_pubkeys`): the issuer is then only compared with the token's `iss`, never
  fetched, and only those keys verify a login (`pinned_keys` counts them). When the cluster's keys
  change, replace them (`PUT …/identities/{id}/jwks`, `roll secrets identities keys ID --jwks
  FILE`): the id, and every pod's `ROLL_IDENTITY`, stays. During a rotation, give the old and the
  new keys in one JWKS. Replacing them is making the identity: it then acts only while whoever
  replaced them can sign in and reach the project, and the tokens its logins got so far are revoked
  (workloads log in again; with #56, `roll run --watch` does it by itself on the 401).

  ```sh
  kubectl get --raw /openid/v1/jwks > jwks.json
  roll secrets identities add pods --jwks jwks.json --audience roll --for-env production \
    --issuer "$(kubectl get --raw /.well-known/openid-configuration | jq -r .issuer)" \
    --subject system:serviceaccount:shop:web
  ```

  Checked on k3s 1.33: a pod running as `shop/web` with a projected token for audience `roll`
  (`ROLL_OIDC_TOKEN_FILE`) ran `roll run` with production's values, and the audit log names
  `system:serviceaccount:shop:web`.
- **Who makes them.** Identities are made by people (or the operator), never by a token or a
  login; a login can't create tokens either, so nothing it makes outlives it.
- **Login.** `POST /v1/secrets/login {identity, token}` (public: the token is the credential)
  checks the signature against the issuer's keys (RS256 or ES256; its discovery document's
  `jwks_uri`, fetched over the guarded client: HTTPS, public addresses, no redirects, bounded reads;
  cached an hour, fetched again early for a new key id, kept a day if the issuer is down), `iss`,
  `aud`, `exp` and `nbf` (60 s leeway), the subject and the claims, then mints an `rlst_` token
  with the identity's scope for its TTL. The token is looked up on every request like any other;
  its principal is `identity:<id>`, so the audit log names the identity, and the login itself is
  `identity.login` with the token's `sub`. Deleting the identity revokes every token it gave out.
  An OIDC token can be exchanged more than once while it lives (as with Vault): each login is
  audited.
- **The CLI.** With `ROLL_IDENTITY=idn_…` and no `ROLL_TOKEN`, `roll secrets …` and `roll run` log
  in by themselves, with the token from `ROLL_OIDC_TOKEN`, `ROLL_OIDC_TOKEN_FILE` (a Kubernetes
  projected service account token) or GitHub Actions' own endpoint (`permissions: id-token: write`;
  audience `ROLL_OIDC_AUDIENCE`, default the server's URL):

  ```sh
  roll secrets identities add deploy --github acme/web --claim ref=refs/heads/main --for-env production
  ```

  ```yaml
  permissions: { id-token: write, contents: read }
  env: { ROLL_API_URL: https://roll.example.com, ROLL_IDENTITY: idn_… }
  steps:
    - run: roll run --env production -- ./deploy.sh
  ```

  A login lasts its TTL (5 to 60 minutes). A long-running process (`roll run --watch` in a pod,
  `render --watch`) logs in again by itself a quarter of the way before its login ends (at least
  30 s before), reading `ROLL_OIDC_TOKEN_FILE` again, since Kubernetes rotates a projected token's
  file. That login runs in the background while requests go on with the current token; one that
  fails or takes over 30 s leaves the current token in use while it lasts (tried again every 15 s;
  its lifetime is measured on the process's own clock). A token that has ended, or that the server
  refuses (401), logs in again before the next request: a pod needs no stored secret, as with Vault's Kubernetes auth. The cluster's service
  account issuer has to be one roll can fetch keys from (EKS's and GKE's are public URLs):

  ```yaml
  # the pod: a projected token for roll's audience, and the identity to log in as
  env:
    - { name: ROLL_API_URL, value: https://roll.example.com }
    - { name: ROLL_IDENTITY, value: idn_… }
    - { name: ROLL_OIDC_TOKEN_FILE, value: /var/run/secrets/roll/token }
  volumeMounts: [{ name: roll-token, mountPath: /var/run/secrets/roll }]
  # volumes: [{ name: roll-token, projected: { sources: [{ serviceAccountToken: { audience: roll, path: token, expirationSeconds: 3600 } }] } }]
  ```
- **A CI job, end to end.** No stored secret: the job logs in with its own OIDC token as an
  identity limited to staging, then checks before it tests. Each step only sees staging, since
  check, scan and health answer what the caller may read:

  ```sh
  roll secrets identities add ci --github acme/web --claim ref=refs/heads/* --for-env staging
  ```

  ```yaml
  jobs:
    test:
      runs-on: ubuntu-latest
      permissions: { id-token: write, contents: read }
      env: { ROLL_API_URL: https://roll.example.com, ROLL_IDENTITY: idn_…, ROLL_ENV: staging }
      steps:
        - uses: actions/checkout@v4
        # roll on the runner's PATH; then:
        - run: roll secrets check .env.example   # every key the code reads is set in staging
        - run: roll secrets scan .               # no stored value was committed
        - run: roll secrets health               # nothing expired, missing or exposed
        - run: roll run -- npm test              # the tests get staging's values, never on disk
  ```
- When roll-access takes over tokens, identities mint its scoped tokens instead; the routes stay.

### Protected environments and approvals

An environment can be **protected** with a policy: `{"approvals": 1, "approvers": [principal…],
"notify_url"?}`. With `notify_url` (a Slack or Discord incoming webhook, or any HTTPS URL; kept
sealed, the environment shows only its host as `notify`), each request opened on it is announced:
the notice is written in the request's own transaction and delivered by the sweeper with retries,
as `{text, content, project, environment, request: {id, by, message, keys}, link}` (key names,
never values; Slack markup in `text` is escaped and Discord mentions are off, since the message is
the requester's). Notices aren't signed: an incoming webhook's URL is its own credential, and the
notice carries nothing a receiver should act on without opening the request in roll.
A write to a protected environment doesn't apply. It answers 409 `approval_required`, and the
client opens a **change request** with the same operations (`POST …/requests`). A request:

- holds its operations with values encrypted, and shows a diff without values; an approver who may
  reveal them sees each value now beside the one proposed (`…/requests/{id}/values`, audited);
- needs `approvals` approvals from approvers other than its author (an empty approver list means any
  principal with `admin`);
- applies as one change when approved (`…/apply`), and only if no key it touches changed since it
  was opened (otherwise it is `stale` and must be refreshed);
- can be applied without approvals by a caller with `admin` passing `{"bypass": true}`. The bypass
  is recorded in the audit log.

Rotation of a managed link and dynamic leases on a protected environment don't need approval: they
don't change what the secret is, only its credential.

**Change policies per folder**, as Infisical's approval policies on secret paths: a folder of an
environment that isn't protected as a whole can still take changes only through approved requests.
`PUT …/environments/{env}/folder-policies?path=/payments {approvals?, approvers?}` (admin;
`approvals` 1–10, 1 by default; 200 per environment, never on an ephemeral one; `roll secrets envs
protect staging --folder /payments [--approver …]`, `unprotect --folder`; the folder bar's "Require
approval" in the dashboard) covers the folder
and every folder under it; `GET` lists them and `DELETE ?path=` removes one. A write that touches a
covered key answers 409 `approval_required`, naming the folder, and the rest of the environment
takes writes as before. A request's **gates** are the environment's policy (when it's protected)
and every folder policy covering a key it changes; each needs its own approvals from its own
approvers (`gates: [{path?, approvals, approvers?, approved}]` on the request), a reviewer must be
an approver of at least one of them, and applying checks the gates as they are then (a policy added
after the approvals still has to be met). Every way to change a covered key goes through a request:
writes, deletes (deleting a covered folder, or one holding one, is refused), a promotion (it opens
a request), and setting a covered folder's imports needs `admin`, as on a protected environment. A
reviewer needs `admin` on each folder the request changes (a folder's own admin reviews its
folder's requests), and a review counts only for the gates whose scope its reviewer is admin of:
an admin of `/payments` approves for `/payments`' gate, never for a protected environment's own
(`gates` on each review); applying checks the gates in the applying transaction, and a bypass of gates
that aren't met is audited. Backups carry folder policies, and a restore sets them last. Only who
may change what is per folder: reading (untrusted readers, exposure in health) stays the
environment's.

### Audit log

Every read of values and every write is recorded: time, principal (`operator`, `token:tok_…`,
`app:…`), action, environment, path, keys, a summary and the client address (the peer's, or the
first `X-Forwarded-For` one with `ROLL_CODE_TRUST_FORWARDED_FOR`). A value never is.
Listing metadata isn't audited. Revealing, exporting, resolving and every write are.
`GET …/audit?env=&key=&actor=&action=&before=&limit=` reads it newest first. Rows are kept for 400
days.

- **Export**: `GET …/audit?format=csv` or `format=jsonl` (with the same filters, `since` too)
  answers up to 10,000 rows as a file.
- **Streaming to a SIEM**: an audit sink (`POST …/audit/sinks {url, from_start?}`, `admin`) is a
  public HTTPS URL that receives the project's new rows in order, in batches of up to 200:
  `{sink, project, events: AuditEntry[]}`, signed like syncs and every roll webhook (`X-Roll-Sink`, `X-Roll-Timestamp`,
  `X-Roll-Signature-256: sha256=HMAC(secret, "ts.body")`, `X-Roll-Delivery`; the `whsec_…` secret is shown once and kept
  sealed). Each sink keeps a cursor; the sweeper claims a sink in a write before delivering, so no
  two nodes send one batch, and a failed batch is sent again 5 minutes later. Deliveries go through
  the same `PublicOnly` client as syncs. At most 10 sinks a project.
- **Into a roll bucket**: a sink whose URL is `bucket:<name>`, a bucket on the project's own canvas
  (any other is refused, and a bucket moved off the canvas later gets nothing more: it's checked at
every delivery), writes each batch as an object `roll-secrets/audit/<sink>/<delivery>.json`
  there, through roll storage in process: a compliance archive with the bucket's own lifecycle,
  versioning and Object Lock. A retried batch is the same delivery, so it writes the same object
  again, never a second one.
- **Into a roll stream**: a sink `stream:<stream id>/<topic>` (a stream on the canvas, checked at
  every delivery like a bucket) produces each audit event to that topic as one record, keyed by its
  audit `id`, through roll streams in process: consumers react as it's written. A retried batch can
  produce an event again, so a consumer dedupes by key.


## HTTP API

Bodies are JSON and every error is `{"error": string, "code"?: string}`. Paths below are under
`/v1/secrets/projects/{project}`. `{env}` is an environment slug, `{key}` a secret key, and the folder
is the `path` query parameter (default `/`). `{project}` is a workspace project id; a project that
isn't in the workspace is 404.

| Route | Body | Answer |
| --- | --- | --- |
| `GET /v1/secrets/health` | | `{ok, key: "env"\|"kms"\|"file"\|"missing", resealed: "all"\|number\|null, links: string[], syncs: string[]}` (`links`, `syncs`: the kinds registered) |
| `GET` | | `{project, environments: Environment[], previews: number, history_days: number \| null}` |
| `PATCH` | `{history_days}` | `{history_days}` (`admin`) |
| `GET /grid?path=` | | `Grid` |
| `GET /environments` | | `Environment[]` in order |
| `POST /environments` | `NewEnvironment` | 201 `Environment` |
| `DELETE /environments?label=k=v` | | `{deleted: string[]}` (ephemeral only) |
| `PUT /environments/order` | `{order: slug[]}` | `Environment[]` |
| `GET /environments/{env}` | | `Environment` |
| `PUT /environments/{env}` | `Ensure` | 201 or 200 `{environment, created, change}` (409 `mismatch`) |
| `PATCH /environments/{env}` | `{slug?, name?, expires_at?, max_age_days?}` | `Environment` |
| `DELETE /environments/{env}?confirm=` | | 204 |
| `PUT /environments/{env}/policy` | `{protected, approvals?, approvers?, notify_url?}` | `Environment` |
| `GET /environments/{env}/folder-policies` | | `FolderPolicy[]` |
| `PUT /environments/{env}/folder-policies?path=` | `{approvals?, approvers?}` | `FolderPolicy` (`admin`) |
| `DELETE /environments/{env}/folder-policies?path=` | | 204; 404 when the folder has none (`admin`) |
| `GET /environments/{env}/folders?path=` | | `{path, folders: Folder[]}` |
| `POST /environments/{env}/folders` | `{path}` | 201 `Folder` |
| `DELETE /environments/{env}/folders?path=` | | `{change: Change \| null}` |
| `GET /environments/{env}/imports?path=` | | `Import[]` |
| `PUT /environments/{env}/imports?path=` | `Import[]` | `Import[]` |
| `GET /environments/{env}/exports?path=` | | `Export[]` |
| `POST /backup` | `{recipient}` | `{sealed, context}`: the whole project, sealed to `recipient` (admin) |
| `POST /restore` | `Archive` (an opened backup) | `{environments, secrets, warnings}` (admin; an empty project) |
| `GET /suggestions` | | `Suggestion[]`: canvas resources linked nowhere yet, with their usual keys and where they'd go |
| `GET /shared` | | `Import[]`: the folders other projects share with this one |
| `PUT /environments/{env}/exports?path=` | `{project}[]` | `Export[]` (admin) |
| `GET /environments/{env}/secrets?path=` | | `Secret[]` (own, imported, inherited; no values) |
| `GET /environments/{env}/secrets/{key}?path=&type=` | | `Secret` |
| `PUT /environments/{env}/secrets/{key}?path=` | `SetSecret` | `Written`; 409 `stale` when `if_version` isn't the key's version |
| `DELETE /environments/{env}/secrets/{key}?path=&type=&if_version=` | | `Written`; 409 `stale` when `if_version` isn't the key's version |
| `GET /environments/{env}/secrets/{key}/value?path=&resolve=` | | `{value, problems}` |
| `GET /environments/{env}/secrets/{key}/versions?path=&type=` | | `Version[]` newest first |
| `GET /environments/{env}/secrets/{key}/versions/{v}/value?path=&type=` | | `{value}` |
| `POST /environments/{env}/secrets/{key}/rotate?path=` | | `Applied` |
| `POST /environments/{env}/secrets/{key}/purge?path=` | `{before_version?}` | `{purged}` (`admin`) |
| `GET /environments/{env}/values?path=&format=&resolve=&dynamic=&digest=&name=&namespace=&seq=&at=` | | `Values` (with `seq`: as stored after that change, `seq` in the answer), or text with `format`: `dotenv`, `shell`, `json`, `yaml`, `docker` (`--env-file`; a value with a line break is a 400 naming the key) or `kubernetes` (a Secret named `name`, default the environment, in `namespace`) |
| `GET /environments/{env}/values?path=&format=&resolve=&dynamic=&strict=&name=&namespace=&seq=&at=` | | `Values` (`dynamic=false`: a dynamic key is left out, named in `dynamic`, instead of a fresh credential; `strict=1`: any problem is a 409 `unresolved` naming the keys, so a reader keeps what it had; with `seq`: as stored after that change, `seq` in the answer), or text with `format`: `dotenv`, `shell`, `json`, `yaml`, `docker` (`--env-file`; a value with a line break is a 400 naming the key) or `kubernetes` (a Secret named `name`, default the environment, in `namespace`) |
| `GET /environments/{env}/insights?path=` | | `Insights` (read) |
| `GET /environments/{env}/digest?path=` | | `{digest}` (read: changes when the values would) |
| `POST /environments/{env}/resolve` | `{path?, consumer, seal_to?, untrusted?}` | `Values` or `{sealed, context, problems, leases, dynamic, linked}` |
| `GET /environments/{env}/changes?before=&limit=` | | `Change[]` newest first |
| `POST /environments/{env}/changes` | `Batch` | 201 `Applied` |
| `POST /environments/{env}/diff` | `{dotenv} \| {json}, path?` | `{added, changed, same, missing, references, linked}`: a local copy against the folder by fingerprint, names only (`reveal`, audited) |
| `POST /environments/{env}/import` | `{dotenv \| json, path?, overwrite?, replace?, message?, request?, if_seq?}` | 201 `Applied & {skipped}` or `{request, skipped}` |
| `GET /environments/{env}/changes/{seq}?state=` | | `ChangeDetail` |
| `POST /environments/{env}/rollback` | `{to?: seq, at?: ms, message?, bypass?}` | 201 `Applied` |
| `GET /compare?from=&to=&path=` | | `Compare` |
| `POST /environments/{env}/promote?from=` | `{path?, keys?, message?}` | 201 `Applied` |
| `GET /environments/{env}/requests?state=` | | `Request[]` |
| `POST /environments/{env}/requests` | `Batch` | 201 `Request` |
| `GET /environments/{env}/requests/{id}` | | `Request` |
| `GET /environments/{env}/requests/{id}/values` | | `RequestValue[]` (`reveal`; audited: the value now and the one proposed) |
| `POST /environments/{env}/requests/{id}/review` | `{verdict: "approve"\|"reject", comment?}` | `Request` |
| `POST /environments/{env}/requests/{id}/apply` | `{bypass?}` | `Request` |
| `POST /environments/{env}/requests/{id}/close` | | `Request` |
| `GET /links` | | `LinkTarget[]` |
| `GET /leases` | | `Lease[]` |
| `DELETE /leases/{id}` | | 204 |
| `POST /leases/{id}/renew` | `{ttl_minutes?}` | `Lease` (`reveal` on its environment) |
| `GET /tokens` | | `Token[]` |
| `POST /tokens` | `NewToken` | 201 `Token & {token}` (shown once) |
| `DELETE /tokens/{id}` | | 204 |
| `GET /identities` | | `Identity[]` (admin) |
| `POST /identities` | `NewIdentity` | 201 `Identity` |
| `DELETE /identities/{id}` | | 204 (its tokens revoked) |
| `PUT /identities/{id}/jwks` | `{jwks}` | `Identity` (admin; a person's): its pinned keys replaced, its id kept |
| `POST /v1/secrets/login` | `{identity, token}` | `{token, expires_at, project, identity}` (public) |
| `GET /audit?env=&key=&actor=&action=&before=&since=&limit=&format=` | | `AuditEntry[]` newest first, or CSV/JSONL with `format` |
| `GET /required` | | `Required[]` |
| `PUT /required` | `Required[]` | `Required[]` (replaces the list; `admin`) |
| `GET /shares` | | `Share[]` newest first |
| `POST /shares` | `{ciphertext, note?, views?, ttl_minutes?}` | 201 `Share` |
| `DELETE /shares/{id}` | | 204 |
| `GET /syncs` | | `{syncs: Sync[], kinds: SyncKind[]}` |
| `POST /syncs` | `NewSync` | 201 `Sync & {signing_secret?}` (shown once; `admin`) |
| `POST /syncs/preview` | `NewSync` | `{names, credentials, missing_credentials, unknown_keys}`: what it would push, before it exists (`admin`; names only, the platform isn't called) |
| `DELETE /syncs/{id}` | | 204 (`admin`) |
| `POST /syncs/{id}/run` | | `Sync` after pushing every key (`admin`; 409 `running` while another push holds it) |
| `GET /v1/secrets/shares/{id}` (public) | | `{note, views_left, expires_at}` |
| `POST /v1/secrets/shares/{id}/open` (public) | | `{ciphertext}` (one open used) |
| `GET /audit/sinks` | | `Sink[]` (`admin`) |
| `POST /audit/sinks` | `{url, from_start?}` (an HTTPS endpoint, `bucket:<name>` or `stream:<id>/<topic>` on the canvas) | 201 `Sink & {secret}` (shown once) |
| `DELETE /audit/sinks/{id}` | | 204 |
| `GET /v1/secrets/findings` | | `{project, project_name, findings: Finding[]}[]`: every project's findings the caller may read, projects with none left out |
| `GET /health?code=` | | `Finding[]`, worst first: every standard environment's findings the caller may read (`read` on the project) |
| `POST /leaks` | `{candidates: string[]}` (at most 20,000) | `{index, key, environment, path}[]`: candidates that are values the caller may reveal (`read` on the project) |
| `GET /kms` | | `{key: string \| null}`: the customer-managed key wrapping the project's data key (admin) |
| `PUT /kms` | `{key}` (a KMS key's ARN) | `{key}` after encrypting and decrypting with it (admin; 400 when roll can't use it) |
| `DELETE /kms` | | `{key: null}`: the data key sealed by roll's deployment key alone again (admin) |
| `GET /aws-roles` | | `AwsRole[]` (`read` on the project) |
| `POST /aws-roles` | `{name, role_arn}` | 201 `AwsRole` after assuming it (admin; 400 when its trust policy doesn't pin `sts:ExternalId`) |
| `DELETE /aws-roles/{id}` | | 204 (admin; 409 `in_use` while a key links to it) |
| `GET /aws-users` | | `AwsUser[]` (`read` on the project) |
| `POST /aws-users` | `{name, role, user}` | 201 `AwsUser` (`admin`; lists the user's keys as the role first: 400 with AWS's code) |
| `DELETE /aws-users/{id}` | | 204; 409 while a key links to it or a retired key waits (`admin`) |
| `GET /rotations` | | `Rotation[]`: every managed credential the caller may read (environment, keys, resource, `rotate_days`, `issued_at`, `next_due`, `last_error`) |
| `GET /rotators` | | `Rotator[]` (`read` on the project) |
| `POST /rotators` | `{name, url}` | 201 `Rotator & {secret}` (`admin`; the signing secret shown once) |
| `DELETE /rotators/{id}` | | 204; 409 while a key links to it (`admin`) |
| `GET /outside-databases` | | `OutsideDatabase[]` (`read` on the project; never the URL) |
| `POST /outside-databases` | `{name, url, roles?: [a, b], group?, ca?}` | 201 `OutsideDatabase` (`admin`; connects and checks the roles first: 400 with the server's reason) |
| `DELETE /outside-databases/{id}` | | 204; 409 while a key links to it or a retired password waits (`admin`) |
| `GET /cloud-accounts` | | `CloudAccount[]` (`read` on the project) |
| `POST /cloud-accounts` | `{name, cloud: "gcp", audience, service_account}` or `{name, cloud: "azure", tenant, client_id, scope?}` | 201 `CloudAccount` (`admin`; 400 when a made-up project's token is taken, or this project's isn't) |
| `DELETE /cloud-accounts/{id}` | | 204; 409 while a key links to it (`admin`) |
| `GET /v1/secrets/oidc/.well-known/openid-configuration` | | roll's issuer's discovery document (public) |
| `GET /v1/secrets/oidc/jwks` | | roll's issuer's public keys (public) |
| `GET /cas` | | `Ca[]` (`read` on the project; certificates, never keys) |
| `POST /cas` | `{name, domains, allow_ip?, max_days?}` | 201 `Ca` (`admin`) |
| `DELETE /cas/{id}` | | 204; 409 while a key links to it (`admin`) |
| `GET /cas/{id}/certificates` | | `CaCertificate[]`: `{serial, names, source, not_after, issued_at, issued_by, revoked_at?}`, what it signed that hasn't expired, newest first (`read`) |
| `POST /cas/{id}/revoke` | `{serial}` | 204; 404 for a serial it didn't sign or that has expired (`write`, audited) |
| `GET /v1/secrets/crl/{id}` | | the CA's CRL, DER (`application/pkix-crl`) or `?format=pem`; no token |
| `POST /v1/secrets/ocsp/{id}` | an OCSP request (DER) | an OCSP response (`application/ocsp-response`); no token |
| `GET /v1/secrets/ocsp/{id}/{request}` | | the same, the request base64 and URL-encoded (RFC 6960 A.1); no token |
| `POST /cas/{id}/sign` | `{csr, days?, ttl_minutes?}` | `{certificate, ca, serial, names, not_after}` (`write`, audited); 400 for a name outside the policy or a lifetime past `max_days` |
| `GET /ssh-cas` | | `SshCa[]` (`read` on the project; public keys, never private ones) |
| `POST /ssh-cas` | `{name, principals, max_hours?, host_domains?, max_host_days?}` | 201 `SshCa` (`admin`) |
| `DELETE /ssh-cas/{id}` | | 204; 409 while a key links to it (`admin`) |
| `POST /ssh-cas/{id}/sign-user` | `{public_key, principals, ttl_minutes?}` | `{certificate, serial, principals, valid_before}` (`reveal`, audited) |
| `POST /ssh-cas/{id}/sign-host` | `{public_key, hosts, days?}` | `{certificate, known_hosts, serial, hosts, valid_before}` (`admin`, audited); 400 for a name outside `host_domains` or a weak key |
| `GET /transit` | | `TransitKey[]` (`read`; versions and dates, never key material) |
| `POST /transit` | `{name, key?, vault_version?}` | 201 `TransitKey` (`admin`; `key`: one brought, 32 bytes base64) |
| `PATCH /transit/{name}` | `{min_decrypt_version}` | `TransitKey` (`admin`) |
| `DELETE /transit/{name}?confirm=` | | 204; 400 unless `confirm` repeats the name (`admin`) |
| `POST /transit/{name}/rotate` | `{key, vault_version?}` or nothing | `TransitKey` (`admin`); 409 for a Vault version it has |
| `POST /transit/{name}/encrypt` | `{plaintext, context?}` or `{items: […]}` | `{ciphertext}` or `{items: [{ciphertext}]}` (`write`) |
| `POST /transit/{name}/rewrap` | `{ciphertext, context?}` or `{items: […]}` | `{ciphertext}` or `{items}` (`write`) |
| `POST /transit/{name}/decrypt` | `{ciphertext, context?}` or `{items: […]}` | `{plaintext}` or `{items}` (`reveal`, audited) |
| `POST /transit/{name}/datakey` | `{context?}` | `{plaintext, ciphertext}` (`reveal`, audited) |
| `POST /transit/{name}/sign` | `{input}` or `{items: […]}` | `{signature}` or `{items}` (`write`; Ed25519) |
| `POST /transit/{name}/verify` | `{input, signature}` or `{items: […]}` | `{valid}` or `{items}` (`read`) |
| `POST /transit/{name}/hmac` | `{input}` or `{items: […]}` | `{hmac}` or `{items}` (`write`; HMAC-SHA256) |
| `POST /transit/{name}/hmac/verify` | `{input, hmac}` or `{items: […]}` | `{valid}` or `{items}` (`write`: it answers guesses) |
| `GET /transit/{name}/public` | | `TransitPublicKey[]`: `{version, public_key (base64), pem, jwk}` for every version (`read`) |
| `GET /v1/secrets/whoami` | | `{principal, project?, scopes?}` |
| `GET /v1/secrets/search?key=` | | `Place[]`: every project, environment and folder holding `key` (`PREFIX*` for a prefix) that the caller may read; never values |

- 400: a bad slug, path, key, value, body or query.
- 401: no credential, or a token that doesn't exist, is revoked or expired.
- 403: the caller may not do this (`{"code": "forbidden"}`).
- 404: no such project, environment, folder, secret, version, change, request, lease or token.
- 409: a slug that is taken (`conflict`), a write to a protected environment
  (`approval_required`), a request that is `stale`, the last standard environment.
- 503: no deployment key on a node that shares the system database (`no_key`).

### Shapes

```ts
type Environment = {
  id: string; slug: string; name: string; position: number; kind: "standard" | "ephemeral";
  base: string | null; labels: Record<string, string>; expires_at: number | null;
  protected: boolean; approvals: number; approvers: string[]; created_at: number; secrets: number;
  max_age_days: number | null; notify: string | null; trusted: boolean; // trusted: an ephemeral one that may read a protected base
};
type Ensure = Omit<NewEnvironment, "slug"> & { replace?: boolean }; // PUT /environments/{env}
type NewEnvironment = {
  slug: string; name?: string; kind?: "standard" | "ephemeral"; base?: string; labels?: Record<string, string>;
  expires_at?: number; ttl_minutes?: number; secrets?: Op[]; // ephemeral: created in one call with its overrides
  trusted?: boolean; // ephemeral: may read a protected base (admin on it)
};
type Folder = { path: string; name: string; secrets: number; folders: number };
type Import = { project?: string; environment: string; path: string }; // project: another project's shared folder
type Export = { project: string; created_at: number; created_by: string };
type Link = { kind: string; ref: string; field: string; mode: "live" | "managed" | "dynamic"; options?: Record<string, unknown> };
type Secret = {
  id: string; key: string; path: string; type: "shared" | "personal"; version: number;
  comment: string; tags: string[]; link: Link | null; fp: string; references: string[];
  updated_at: number; updated_by: string;
  source: { kind: "own" } | { kind: "import"; environment: string; path: string } | { kind: "base"; environment: string };
  overridden: boolean; // a personal value of the caller replaces it
};
type SetSecret = { value?: string; comment?: string; tags?: string[]; type?: "shared" | "personal"; link?: Link | null; message?: string; expires_at?: number | null; if_version?: number };
type Op =
  | { op: "set"; key: string; value?: string; comment?: string; tags?: string[]; type?: "shared" | "personal"; link?: Link | null; if_version?: number }
  | { op: "delete"; key: string; type?: "shared" | "personal"; if_version?: number };
type Batch = { path?: string; message?: string; ops: Op[]; if_seq?: number };
type Written = { change: Change | null; secret: Secret | null; warnings: string[] };
type Applied = { change: Change | null; items: Item[]; warnings: string[]; request?: Request }; // change: null when nothing changed; request: opened instead (promote into a protected environment)
type Item = { key: string; path: string; type: "shared" | "personal"; what: string[]; from_fp: string | null; to_fp: string | null };
type Change = { seq: number; environment: string; path: string; message: string; by: string; at: number; keys: number; kind: "write" | "rollback" | "promote" | "rotate" | "request" | "rename" };
type ChangeDetail = Change & { items: Item[]; state?: { key: string; path: string; version: number; fp: string; comment: string; tags: string[]; link: Link | null; at: number }[] };
// what: added | changed | deleted | comment | tags | link | rotated
type Version = { version: number; seq: number; op: "set" | "delete"; what: string[]; purged: boolean; fp: string | null; comment: string; tags: string[]; link: Link | null; at: number; by: string };
type Values = { values: Record<string, string>; problems: { key: string; problem: string }[]; leases: string[]; dynamic: string[]; linked: string[]; withheld: string[]; sensitive: string[] }; // leases: ids of dynamic credentials issued; dynamic: the keys holding one; linked: keys from any link (addresses a consumer on another network rewrites)
type Values = { values: Record<string, string>; problems: { key: string; problem: string }[]; leases: string[]; dynamic: string[]; linked: string[]; digest?: string }; // digest (with ?digest=1, not with seq or at): the folder's `…/digest`, taken just before this read; leases: ids of dynamic credentials issued; dynamic: the keys holding one; linked: keys from any link (addresses a consumer on another network rewrites)
type Grid = { path: string; environments: string[]; rows: { key: string; cells: Record<string, Cell | null>; status: "same" | "different" | "missing"; required: boolean }[]; folders: string[]; previews: number };
type Insights = { path: string; max_age_days: number | null; reads_since: number; keys: { key: string; version: number; updated_at: number; updated_by: string; last_read_at: number | null; last_read_by: string | null; reads_30d: number; stale: boolean }[] };
type Required = { key: string; path: string; comment: string; type?: "url" | "int" | "port" | "bool" | "json" | "email"; pattern?: string; owner?: string /* who supplies it */ };
type Sink = { id: string; url: string; status: "new" | "ok" | "error"; error: string | null; cursor: number; delivered_at: number | null; created_at: number; created_by: string };
type ProjectFindings = { project: string; project_name: string; findings: Finding[] };
type Finding = { issue: "expired" | "missing" | "exposed" | "weak" | "expiring" | "certificate_expiring" | "stale" | "unread" | "unreferenced"; environment: string; path: string; key: string; detail: string };
type Finding = { issue: "expired" | "rotation_failing" | "revoke_failing" | "missing" | "exposed" | "weak" | "expiring" | "stale" | "unread" | "unreferenced"; environment: string; path: string; key: string; detail: string };
type Rotation = { environment: string; path: string; keys: string[]; kind: string; ref: string; name: string; rotate_days: number | null; grace_minutes: number; issued_at: number | null; next_due: number | null; last_error: string | null; failed_at: number | null };
type Rotator = { id: string; name: string; host: string; created_at: number; created_by: string };
type Share = { id: string; note: string; views_left: number; created_by: string; created_at: number; expires_at: number; opened_at: number | null; open: boolean };
type Cell = { fp: string; version: number; updated_at: number; link: Link | null; reference: boolean; source: "own" | "import" | "base"; comment: boolean; personal: boolean; misfit?: string };
type Compare = { from: string; to: string; path: string; rows: { key: string; status: "same" | "different" | "only_from" | "only_to" | "hidden"; from: number | null; to: number | null }[] };
type Request = { id: string; environment: string; path: string; message: string; by: string; at: number; state: "open" | "approved" | "rejected" | "applied" | "closed" | "stale"; items: Item[]; reviews: { by: string; verdict: "approve" | "reject"; comment: string; at: number }[]; approvals: number; approvals_needed: number; applied_seq: number | null };
type Compare = { from: string; to: string; path: string; rows: { key: string; status: "same" | "different" | "only_from" | "only_to"; from: number | null; to: number | null }[] };
type Request = { id: string; environment: string; path: string; message: string; by: string; at: number; state: "open" | "approved" | "rejected" | "applied" | "closed" | "stale"; items: Item[]; reviews: { by: string; verdict: "approve" | "reject"; comment: string; at: number; gates?: (string | null)[] }[]; approvals: number; approvals_needed: number; gates: { path?: string; approvals: number; approvers?: string[]; approved: number }[]; applied_seq: number | null };
type FolderPolicy = { path: string; approvals: number; approvers: string[]; created_at: number; created_by: string };
type LinkTarget = { kind: string; ref: string; name: string; fields: { field: string; label: string; modes: Link["mode"][]; secret: boolean; suggest: string; credential?: string }[] };
type Lease = { id: string; environment: string; path: string; key: string; kind: "dynamic" | "retired"; link: Link | null; consumer: string; issued_at: number; expires_at: number; revoked_at: number | null };
type Identity = { id: string; name: string; issuer: string; audience: string; subject: string; claims: Record<string, string>; environments: string[] | null; paths: string[] | null; actions: Action[]; ttl_minutes: number; created_at: number; created_by: string; last_used_at: number | null; last_subject: string | null; allowed_ips: string[] | null; pinned_keys?: number };
type NewIdentity = { name: string; issuer: string; audience: string; subject: string; claims?: Record<string, string>; environments?: string[]; paths?: string[]; actions: Action[]; ttl_minutes?: number; allowed_ips?: string[]; jwks?: string };
type Token = { id: string; name: string; environments: string[] | null; paths: string[] | null; actions: Action[]; expires_at: number | null; created_at: number; created_by: string; last_used_at: number | null; prefix: string; allowed_ips: string[] | null };
type NewToken = { name: string; environments?: string[]; paths?: string[]; actions: Action[]; expires_at?: number; ttl_days?: number; allowed_ips?: string[] };
type Action = "read" | "reveal" | "write" | "delete" | "admin";
type Sync = {
  id: string; environment: string; path: string; kind: string; target: Record<string, unknown>;
  keys: string[] | null; prefix: string; status: "pending" | "ok" | "error"; error: string | null;
  last_digest: string | null; last_synced_at: number | null; synced: number; // names at the target
  signed: boolean; created_at: number; created_by: string;
};
type NewSync = { environment: string; path?: string; kind: string; target: Record<string, unknown>; keys?: string[]; prefix?: string };
type SyncKind = { kind: string; label: string; values: boolean; signed: boolean; fields: { name: string; label: string; required: boolean; placeholder: string }[] };
type AuditEntry = { id: number; at: number; actor: string; action: string; environment: string | null; path: string | null; keys: string[]; summary: string; ip: string | null };
```

## Tables

```sql
projects(project_id PK, dek TEXT, seq INTEGER, created_at)          -- dek: the sealed data key
environments(id PK, project_id, slug, name, position, kind, base_id, labels, expires_at,
             protected, approvals, approvers, created_at, UNIQUE(project_id, slug))
folders(env_id, path, created_at, PRIMARY KEY (env_id, path))
imports(env_id, path, position, from_env_id, from_path, PRIMARY KEY (env_id, path, position))
secrets(id PK, project_id, env_id, path, key, owner, version, value BLOB, fp, comment, tags, link,
        link_state, refs, updated_at, updated_by, UNIQUE(env_id, path, key, owner))  -- refs: the references' names, so listings decrypt nothing
versions(secret_id, version, seq, env_id, path, key, owner, op, value BLOB, fp, comment, tags, link,
         at, by, PRIMARY KEY (secret_id, version))
changes(project_id, seq, env_id, path, kind, message, by, at, keys, PRIMARY KEY (project_id, seq))
requests(id PK, project_id, env_id, path, message, by, at, state, ops BLOB, items, reviews, applied_seq)
leases(id PK, project_id, env_id, key, path, kind, link, handle, consumer, issued_at, expires_at,
       revoked_at)
tokens(id PK, project_id, name, hash UNIQUE, prefix, environments, paths, actions, expires_at,
       created_at, created_by, last_used_at, revoked_at, identity)  -- identity: a login's
identities(id PK, project_id, name, issuer, audience, subject, claims, environments, paths,
       actions, ttl_minutes, created_at, created_by, last_used_at, last_subject)
audit(id INTEGER PK, project_id, at, actor, action, env_id, path, keys, summary, ip)
syncs(id PK, project_id, env_id, path, kind, target, keys, prefix, secret BLOB, status, error,
      last_digest, state, last_synced_at, running_until, retry_at, checked_at, created_at, created_by)
      -- secret: the sealed signing secret; state: {name: stamp} of what was pushed (no values)
```

`owner` is `''` for a shared secret and the principal for a personal one. A deleted secret keeps its
row with `op = 'delete'` in `versions` and is removed from `secrets`, so `(env, path, key, owner)` is
free again and its history stays readable by key.

## Performance

`cargo run -p roll-secrets --release --example bench -- 9000` times the hot paths on one
environment of 9,000 keys plus 900 references, on SQLite or, with `ROLL_CODE_TEST_DATABASE`, on
Postgres. Measured 2026-11-13 on ai-server under a load average of about 27 (second run, warm):

| | SQLite | Postgres |
| --- | --- | --- |
| one change of 9,900 keys | 313 ms | |
| one key's change | 5 ms | 6 ms |
| grid (every standard environment) | 38 ms | 48 ms |
| list | 26 ms | 46 ms |
| values, resolved | 74 ms | 149 ms |
| digest | 90 ms | 143 ms |
| insights | 27 ms | 40 ms |
| compare | 38 ms | 62 ms |
| health (the whole project) | 53 ms* | 58 ms* |

\* Health was measured later the same day under a load average of about 39, with the grid at 77 ms
(SQLite) and 65 ms (Postgres) in the same runs: it stays under the grid, which the sidebar's count
and Home rely on.

Every lookup by key inside a request is a hash map (a grid of ~11k keys over 5 environments took
3.07 s before that, 0.43 s after, on a debug build).

## Next

Done and in PR #20:
- the store and its crypto (the deployment key from the environment, a file, or AWS KMS);
- rolld's mount and link providers (Postgres, Redis, streams, buckets, NoSQL, search: checked
  live against a real roll search cluster), and
  rotators for values outside roll;
- `roll secrets`, `roll secrets setup` and `roll run [--watch]`;
- checks, sandboxes and previews, resolving untrusted; preview environments untrusted themselves;
- leaked secrets don't land, plus scans of a repo's default branch and history, and `roll secrets
  scan [--staged]` before a commit;
- required and typed keys; insights, a daily notice of each environment's health, and expiry dates for values that expire
  outside roll; weak credentials; values as they were after any change; a protected environment's values that are also set in an unprotected one; read
  alerts (a machine's new address, many reveals in an hour);
- one-time sharing, and sharing across projects;
- syncs: GitHub, GitLab CI, CircleCI, Bitbucket Pipelines, Terraform Cloud, Vercel, Netlify, Render, Fly.io, Railway,
  Heroku, DigitalOcean App Platform, Cloudflare Workers, AWS
  Parameter Store and Secrets Manager, Google Secret Manager, Azure Key Vault, Kubernetes, sealed webhooks and deploy notifications;
- audit sinks, purge and retention, workload identities (OIDC logins for CI), tokens and
  identities pinned to client addresses; encrypted project backup and restore;
- the dashboard's Secrets surface, with the deployment key's source and a rotation's progress,
  suggested links for the canvas's resources, and one key set in several environments from the
  overview (one change each, a request where protected), and each environment's activity (its
  changes, reads, requests and syncs' pushes as one timeline, by key);
- finding a key across every project the caller may read (`roll secrets find`, the dashboard's
  "Find a key"), for rotating an organisation-wide key;
- exports as a Docker `--env-file` and a Kubernetes Secret manifest, next to dotenv, shell, JSON and
  YAML; `roll secrets render [--watch --exec]` fills `${KEY}` in a config file and keeps it current;
  `roll secrets check` compares an environment with a `.env.example`;
- the fixes from three security reviews and a phone-width pass;
- every route checked to be in the HTTP table (`every_route_is_in_the_http_contract`), which
  roll-access's catalogue is checked against.

Then change policies per folder (#53), as Infisical's approval policies on secret paths.

A ninth independent review (2026-11-14, of `roll run`'s masking and the eighth review's fixes)
found no high-severity issue; it found a held tail flushed on a timer (a block-buffered program
pausing mid-value leaked it in halves), masking on by default for data written to files and pipes,
background processes holding roll open, `--watch` dropping SIGTERM, a SIGINT judged by stdin (#54),
a folder's admin counting toward a protected environment's own gate and an import guard outside its
transaction (#53), a Vault refusal told by an error's text (#52), and a write during a watch's
re-read going unnoticed (#49): all fixed and checked live, tested where a test guards behaviour.
Since then a values answer carries the folder's digest, taken just before its read, and `--watch`
compares from those answers, which closes the last window (#49).
Every stacked pull request's API tests (#49, #50, #51, #53, #55, #57) also pass on Postgres, and
#49 adds **Copy reference** to a key's menu (`roll://ENV[/folder]/KEY`, as 1Password's). Every
stacked pull request is mergeable, and #20 is level with `main`.
Then roll secrets for VS Code (#58): hovers and warnings where code reads a variable, names
completed, and `.env` values moved into roll as `roll://` references (checked in code-server).
Then `roll completions` and direnv documented (#59).
Docker builds (BuildKit secrets) and Compose are documented and checked. Then a server's
credential under a name frameworks put in the browser bundle (`NEXT_PUBLIC_`, `VITE_`…) is
flagged on write and in health (#60). Measured again (9,900 keys, alternating with #20): health
is unchanged by #60 and stays under the grid; a values answer takes its digest only when a watch
asks (`digest=1`, #49), since that costs about a read.
Then reach (#61): for a folder or key, the tokens and identities that reach it, whether untrusted
code can, where it flows (syncs, shares, imports), and who read it in 30 days.

A tenth independent review (2026-11-14, of ESO, renewing logins, pinned keys and the ninth review's
fixes) found no high-severity issue; it found ESO writing an unresolved key as an empty value and
taking a query through its key (#55: `strict=1`, `urlquery`, checked live on k3s), stdout unmasked in
Jenkins, Buildkite, pods and units and a JSON value's secret fields (#54), a login with no timeout
that dropped a working token and ignored 401s (#56), and no way to rotate pinned keys in place
(#57): all fixed. roll MCP has `secrets_reach` and `secrets_folder_policies` tools for #61 and #53.

An eleventh independent review (2026-11-14, of VS Code, completions, browser-bundle names, reach and
the tenth review's fixes) found two high-severity issues in #58, both fixed: "Move to roll" misread
multi-line and commented `.env` entries (it now reads them as the CLI does and replaces the whole
entry), and a repository's settings could send the token elsewhere (`roll.apiUrl` and
`roll.project` come from your own settings only). It also found masking that changed data (#54:
stdout is masked only in CI and never into a file; `ROLL_MASK=1` for pods and units), re-keyed
identities outliving their re-keyer (#57), readers missed through imports and previews (#61), a
kind of credential shown to read-only callers and references not judged (#60), a stalled renewal
(#56) and a refused strict read keeping its leases (#55): all fixed.

A twelfth independent review (2026-11-14, of the eleventh review's fixes) found no high-severity
issue; it found a login racing a key replacement could still get a token (#57: the login's write
checks the keys it verified), Jenkins' file log unmasked (#54: documented, `ROLL_MASK=1`, since a
file can't be told from a dump), late logins and late 401s overwriting fresh tokens (#56), reach
looking one step away and at unshared importers (#61), a write's warning naming a credential's
kind without reveal (#60), and `.env` corner cases and a repository choosing the environment (#58):
all fixed.

A thirteenth review (2026-11-14, narrow: the twelfth review's fixes) found reach naming another
project's internal environments and walking past where reads go (#61: one hop into a shared
project, named only as a project, capped at 256 folders), reference warnings shown without reveal
(#20), `.env` files over roll's limits (#58) and a background login outliving a 401 (#56): all
fixed. The pinned-keys, relogin and masking fixes it checked were correct.
Reach measured at its cap (300 importing folders): 5–7 ms on SQLite and 120–150 ms on Postgres, once
the audit is read per project rather than per folder (#61).

Then (2026-11-14, latest): `roll run` masks values in what the command prints when it isn't a
terminal (#54), as 1Password's `op run` and GitHub Actions do; pulling into Kubernetes with External
Secrets Operator (#55: its webhook provider, checked against ESO 2.15 on k3s) and
`values?dynamic=false` for readers that can't renew a lease. #50, #51 and #53 merge cleanly
together, and the merged tree passes roll-secrets', rolld's auth and the dashboard's tests. #52's
import dialog names Vault trees and AWS (screenshots on #52). `roll secrets kubernetes` and the
Export menu print ESO's manifests with a fresh token (#55), and a workload identity's login renews
itself (#56), so a pod running `roll run --watch` needs no stored token, as with Vault's Kubernetes
auth; and identities pin their issuer's keys (#57), so clusters roll can't reach (k3s, kind,
private EKS) log pods in too (checked on k3s, with #56: a 5-minute login renewed by itself).
`roll run`'s masking was measured and made 5x faster (#54: 330 MB/s with 300 values and a 3 KB
key).

An eighth independent review (2026-11-14, of moving in, change policies per folder and the
seventh review's fixes) found no high-severity issue; it found folder gates a reviewer, a
promotion, an import or a folder delete could get around, apply checking the gates outside its
transaction, backups without folder policies (#53), Vault walks that took any error for an empty
folder and had no bound, colliding AWS keys (#52), and a `--watch` that restarted on every poll
after its references changed (#49): all fixed, the Vault and AWS ones checked live. Screenshots
of folder policies and request gates are on #53.

A seventh independent review (2026-11-14, of RabbitMQ, the OpenSearch names and the sixth
review's fixes) found no high-severity issue; it found a RabbitMQ template copied in part (an
exchange left open), `render --watch` rendering twice, throttled OCSP answers marked cacheable,
repeated flavor lookups, and the short public-suffix list: fixed, or documented for the last.
Moving in from Vault trees and AWS Secrets Manager is #52 (checked against Vault 1.20 and
LocalStack).

Then RabbitMQ outside roll (#39, merged up into #41 and #42; checked live against RabbitMQ 4) and
OpenSearch clusters' fields named as their clients read them (#51).

A sixth independent review (2026-11-14, of `roll://` references, check-and-set, OCSP, OpenSearch
links and the grant fix) found no high-severity issue; it found an OCSP route an anonymous caller
could make sign and cache per request, references whose leases were renewed at the wrong project,
check-and-set dropped on protected environments, and smaller ones (a provider asked before the
project check, a project name two owners share, render --watch, env-file errors, restore names,
public suffixes): all fixed, OCSP re-checked live. The project's tokens now read rules naming it by
id and show key-scoped ones (#30).

Later: the grid sends check-and-set too (#50), OCSP for project CAs (#44: checked with `openssl
ocsp`), search links on OpenSearch clusters (#51, agreed with the search thread), and a fix from roll
MCP's review on #30 (a grant naming the project by id reaches it inside roll secrets too).

Then (2026-11-14, latest): `roll://` references in files (#49: `roll run --env-file`, `render`) and
check-and-set writes (#50: `if_version`, the dashboard's editor), each its own pull request on #20.

A fifth independent review (2026-11-14, of host and user SSH certificates, ClickHouse,
certificates ending, `--rotate-due` and the fourth review's fixes) found no high-severity issue; it
found a CLI that would upload a private key given its path, SSH CAs that could claim a top-level
domain, best-effort signing audits, a stale-CRL race, ClickHouse's kill-then-drop order, a
known_hosts order and `--rotate-due --json` that didn't rotate: all fixed. Screenshots of the new
surfaces are on #44, #45 and #48.

Later still: SSH user certificates for keys people keep (#45: `…/sign-user`, `roll ssh --key`,
checked with an agent-held key against a real `sshd`) and `roll secrets rotations --rotate-due`
(#48).

Then (2026-11-14, latest): certificates ending as health findings (#44: `certificate_expiring`)
and ClickHouse outside roll (#39, merged up into #41 and #42: its HTTPS interface, `sha256_hash`
passwords, checked live against ClickHouse 25.8).

A fourth independent review (2026-11-14, of the rotations overview, transit signing, HMAC and
brought keys, CSR signing, revocation and the CRL, SQL Server) found no high-severity issue; it
found an ended SQL Server lease whose login could stay usable, generated passwords SQL Server's
policy refuses, deleted CAs undoing their revocations, a public CRL signed per request, P-384 CSRs
mislabelled, unrevocable serials, a batch failed by one retired version and a restore that forgot
brought keys: all fixed, tested where a test guards behaviour, and checked live (SQL Server 2022,
OpenSSL). Then SSH host certificates (#45, checked against a real `sshd`).

Built on 2026-11-14 (later), again without a merge: revoking certificates (#44: a list of what a
CA signed, `…/revoke`, revocation when a lease ends, a public CRL every certificate names, checked
with `openssl verify -crl_check`); SQL Server outside roll (#39, merged up into #41 and #42;
checked live against SQL Server 2022); bringing an encryption key (#47: Vault's `vault:v<n>:`
ciphertexts decrypt and rewrap, checked against Vault 1.20).

Built on 2026-11-14 from the last ranking, without waiting on a merge: a rotations overview (#48:
`GET …/rotations`, Health's "Managed credentials", `roll secrets rotations`); signing and HMAC
with encryption keys (#47: Ed25519 a stock library checks with the exported public key, HMAC
blind indexes, `transit(name)` in `@roll/secrets`); CSR signing (#44: `…/cas/{id}/sign`, checked
with OpenSSL's P-256 and RSA CSRs under `openssl verify -x509_strict`).

A third independent review (of #46 and #47) found four ways a sensitive value still reached a person
(older versions, change requests that add the tag, syncs and sandboxes counted as runtimes) and
guessing oracles, plus backups leaving out encryption keys: all fixed and tested. roll apps (#26)
now seals sensitive variables and redeploys when an environment's digest moves (#31 implements
it). An independent security review of #43–#45 found no cross-tenant path; it found managed
certificates that could lapse, an issuer key that a deployment-key rotation would have broken, a
fail-open decoy check and leases outliving cloud tokens, all fixed and tested. Measured again on 2026-11-14 (load about 20): values at 9,900 keys took 89–122 ms on SQLite and
155 ms on Postgres, within the noise of the Performance table; nothing to fix. Reviewed again on
2026-11-14: an independent security review of #39–#41 found racing rotations
could reset the login in use (now a claim never takes a busy role), a `#` in a password shown as
the host, Postgres connections outliving their timeout, and deleted projects leaving credentials
unrevoked (now revoked first, the rest retried by the sweeper); all fixed and tested. Earlier,
2026-11-13 (later): the Link dialog crashed on a kind that isn't a canvas
resource (a rotator, an AWS role): `KIND_ICON` only knows the workspace's. It names roll secrets'
own kinds now and falls back to a plain link icon for any it doesn't know. Earlier, 2026-11-13 (late): #39's own review found two ways a rotation could reset the
role in use (a refused `ALTER ROLE`, two environments sharing the pair); both fixed and tested.
The other threads' heads need nothing from roll secrets (MCP lists outside databases; previews
waits on #31). Earlier, on 2026-11-13 (night): the core and the stacked pull requests are level with
Doppler, Infisical, Vault and 1Password (clients, IaC, CI, Kubernetes, BYOK). The gap that's left
is Vault's signature: dynamic credentials for clouds and databases roll doesn't run. CI stays
under a minute (the new `sdk` and `terraform` jobs take 39 s and 22 s).

Ranked: first what can be built now, without a merge; then what waits on one (merges are the
user's call).

1. **A landing branch** (`feature/roll-secrets-landing`): every stacked pull request merged on #20
   in order, conflicts resolved, the whole suite passing, so the stack can land in one step. A
   rehearsal (2026-11-14) merged each onto the others in order: twelve merge cleanly; twenty
   conflict with an earlier sibling, nearly all additively (routes in `api.rs`, rows in
   `catalog.rs`, types in `types.ts`/`secrets-api.ts`, sections of this doc, `rolld/src/secrets.rs`'s
   providers), and #30 (#20 with roll MCP's #25) touches the most.
2. **When #56 and #57 meet**: the identities section keeps both (renewal and pinned keys); and when
   #49 and #54 meet, `watch` keeps both (answered digests, streams and signals).
3. *Waits on the registries reaching #20:* Terraform resources for CAs, SSH CAs, encryption keys
   and outside databases (#33), and transit in the Go and Python clients (#32).
4. *Waits on merges:* transit keys scoped by name (#24/#25 meeting #47; the MCP thread's builder and
   tools are ready), a live check of OpenSearch links (#29 and #51 in one build), and splitting the
   CLI's `secrets.rs` (2,700 lines) by command once the stacked pull requests that touch it have
   landed.
5. *Waits on #31 and #46:* `ForApps` sets `SecretVar.sensitive` from `Values.sensitive` (apps
   already seals such variables). One line once both are on #20.
6. *Waits on #38 and #43:* AWS through roll's issuer (`AssumeRoleWithWebIdentity` with roll's own
   token; the trust policy pins `sub`, as GCP's and Azure's do).
7. *Waits on #30 and #31:* screenshots of the Access tab's tokens, a person's project and the deploy
   notice.
8. *Waits on #30 meeting #24/#25:* the Access tab shows roll-access tokens waiting for an admin's
   approval (`pending`) and rotations that keep the old secret working (`keep_old_for_s`), and
   reach (#61) lists roll-access grants too.
9. *Waits on linking chat users to roll accounts:* approving change requests from Slack. Chat ops
   installs list Slack user ids and act as the owner's automation; a review must count as the
   person who pressed the button (their permissions, the request's gates), never as the app.
10. *Waits on products serving signed-in people:* products say who owns a resource, so a person's
   project can link their own databases, buckets and streams.
11. *Waits on projects with members:* access requests (time-boxed `reveal` on a protected
   environment).
1. **Screenshots for the PR**: the Access tab's tokens (roll-access's, on #30), a person's project,
   and the deploy notice when apps reads secrets (#31), refreshed once those land.
2. **Products say who owns a resource**, so a person's project can link their own databases,
   buckets and streams: an `Owners` answer from each product's records (`Workspace::trusts`).
   Waits on products serving signed-in people (today they're the operator's).
3. **Access requests** (time-boxed `reveal` on a protected environment, approved for minutes): once
   a project has members besides its owner. Tokens already ask through roll-access (`ask` rules,
   approvals for a while); a project's one owner reaches all of it.

Open pull requests after this one:

- **#30, one permission system** (after #20 and #25): a project's tokens are roll-access grants,
  identities' logins get `workload` tokens, `rlst_` is gone.
- **#31, roll apps' deploys get their project's secrets** (after #20 and #26): `ForApps`, linked
  keys marked for apps to rewrite, `may_reveal` by secrets' own rules (`Secrets::caller_of`).
  #30 and #31 both change `caller()`: whichever lands second keeps #30's (no `rlst_`) with
  #31's `caller_of` split.
- **#32, Go and Python clients** (after #20): `sdk/go/secrets`, `sdk/python/roll_secrets`.
- **#33, a Terraform provider** (after #20): environments, keys (a value or a link), identities,
  syncs, tokens and values.
- **#34, a GitHub Action** (after #20): a job's environment through its own OIDC token, masked;
  `scan: true` fails a pull request that adds a stored value.
- **#35, `roll run --fallback`** (after #20): an encrypted copy of the last values, used through an
  outage.
- **#36, a Kubernetes operator** (after #32): RollSecrets pulled with the cluster's own identity,
  and its Helm chart.
- **#37, customer-managed keys** (after #20): a project's data key wrapped by the customer's KMS key,
  set from the project's settings.
- **#38, dynamic AWS credentials** (after #20): a session per lease from a role the project registers.
- **#40, AWS IAM users' access keys** (after #38): two key slots take turns, the old key deleted
  after its grace; a user with two keys already is refused, never pruned.
- **#55, pulling into Kubernetes with External Secrets Operator** (after #20): its webhook
  provider (one store for whole environments and single keys) and `values?dynamic=false`.
- **#61, reach** (after #20): who and what can read a folder or key; `GET …/reach`, `roll secrets
  reach`, a key's "Who can read it…".
- **#60, server credentials under browser-bundle names** (after #20): `NEXT_PUBLIC_`, `VITE_`,
  `PUBLIC_`… holding a server-only value; the write warns, health's `public` finding.
- **#59, `roll completions` and direnv** (after #20): completion scripts for every command; a
  documented `.envrc`.
- **#58, roll secrets for VS Code** (after #20): hovers, warnings and completion where code reads
  a variable; `.env` values replaced by `roll://` references or moved into roll first.
- **#57, workload identities pin their issuer's keys** (after #20): `jwks` on an identity, for a
  cluster's own issuer; `identities add --jwks`, the dashboard's Other OIDC form.
- **#56, a workload identity's login renews itself** (after #20): long-running `roll run
  --watch` in a pod logs in again before its login ends, reading the rotated token file.
- **#54, `roll run` masks values in the command's output** (after #20): stdout and stderr that
  aren't a terminal go through roll, which writes `<concealed by roll>` for each value it added
  (as is, by line, JSON- and URL-encoded); `--mask`, `--no-mask`.
- **#53, change policies per folder** (after #20): a folder of an unprotected environment takes
  changes only through approved requests, with its own approvals and approvers.
- **#52, moving in from Vault trees and AWS Secrets Manager** (after #20): `import --from vault
  --recursive` into folders, `--from aws` through the user's aws CLI.
- **#51, search links on OpenSearch clusters** (after #20): an internal user (`user`,
  `password`) where the cluster has no API keys; the wrong field for a flavor is refused when linked.
- **#50, check-and-set writes** (after #20): `if_version` on a set or delete, refused as `stale`
  when the key moved; the dashboard's editor sends it.
- **#49, `roll://` references in files** (after #20): `roll run --env-file` and `render` resolve
  `roll://ENV/KEY` beside plain values, as 1Password's `op://`.
- **#48, failing rotations in health** (after #20): `rotation_failing` and `revoke_failing`
  findings, with the reason, until roll succeeds; a rotations overview of every managed credential.
- **#47, encryption keys** (after #20): encryption as a service, as Vault's Transit: named,
  versioned keys that never leave roll; encrypt, decrypt (audited), rewrap, data keys, Ed25519
  signatures and HMACs; keys brought from outside, Vault's ciphertexts included.
- **#46, sensitive keys** (after #20): a key tagged `sensitive` reaches only roll's runtimes, syncs
  and workload identities, never a person or a token; the tag is for good.
- **#45, SSH certificates** (after #44): a project's SSH CA signs short-lived user certificates
  for the logins it allows, and `roll ssh` uses them for one session; OpenSSH's formats, checked
  with `ssh-keygen` and a real `sshd` login.
- **#44, a project's own certificate authority** (after #20): short-lived TLS certificates for the
  names its name-constrained CA allows, dynamic or managed, and CSRs it signs; revocation and a
  public CRL; checked with a real TLS handshake and `openssl verify -crl_check`.
- **#43, dynamic GCP and Azure credentials** (after #20): roll as an OIDC issuer
  (`/v1/secrets/oidc`); GCP's workload identity federation and Azure's federated credentials trust
  its token for one project, checked with a decoy project at registration.
- **#42, MongoDB outside roll** (after #41): rotation and a user per lease for Atlas and replica
  sets; the primary only among the seeds, SRV targets inside their domain, SCRAM with the server's
  signature checked.
- **#41, Redis outside roll** (after #39): two ACL users take turns (`#<sha256>` passwords), or a
  user per lease copying a template user's rules; the tenant's replies read by a bounded parser.
- **#39, Postgres, MySQL, SQL Server, ClickHouse and RabbitMQ outside roll** (after #20): two login roles take turns (rotation), or a
  login per lease in a group role (dynamic), over verified TLS to a public address rolld resolved
  itself (Postgres gets a SCRAM verifier, never the password).

Done since the last review: roll search's merge (#22: its links checked live, the conflicts additive); signed-in people manage the secrets of their own projects (People,
under Access), with the canvas guards. main (roll MCP #19 and accounts #18) merged in. roll secrets asks the
caller's roll-access Grant (`GrantGrants`): a grant's request is checked inside the product too,
health and leaks filter themselves, search and findings take any grant, ephemeral environments are
roll-access's `preview` class, and a sync made by a grant pushes only while the grant may.
