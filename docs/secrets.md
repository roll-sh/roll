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
   ```
8. **When something goes wrong**: `roll secrets history`, `roll secrets rollback --at 2h`, the
   audit log (`roll secrets audit`, or streamed to your SIEM), and `roll secrets share KEY` to hand
   one value to someone outside roll through a link that works once.

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
- One call creates the environment together with its first secrets and links, typically links to
  the preview's branched resources: the preview's Postgres branch, its Redis branch.
- It is hidden from the overview grid's columns (the grid shows a "Previews" count instead), is
  never a promotion target, and can't be protected. Its slug is any free slug (`pr-117`).

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

### Required keys

A project can list keys every standard environment must define, each in a folder
(`PUT …/required [{key, path?, comment?}]`). The overview grid shows a required key as a row even
before any environment sets it (`required: true`). Reading an environment's values reports each
required key it lacks as a problem (`required, but staging doesn't set it`), so `roll run`, checks
and resolves refuse to start without it, as they do for an unresolved reference. An ephemeral
environment counts what its base defines.

### Insights

`GET …/environments/{env}/insights?path=` shows, for each key of a folder, when it last changed
and by whom, when its value was last read and by whom, how often in the last 30 days, and whether
it's **stale**: unchanged for longer than the environment's `max_age_days`
(`PATCH …/environments/{env} {max_age_days}`, 1–3650, `null` for never). Reads come from the audit
log (reveal, export, resolve and sync pushes) over the last 90 days; nothing is decrypted. A stale
key is only flagged: it doesn't stop `roll run`, a deploy or a check.

### Compare and promote

- `GET …/compare?from=development&to=staging&path=/` lists every key in either environment with a
  status: `same`, `different`, `only_from` or `only_to`. Fingerprints decide; no value is revealed.
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
  start) issues a fresh credential with a TTL (`ttl_minutes`, default 60, at most 7 days), recorded
  as a **lease**. Leases are revoked when they expire, or at once with `DELETE …/leases/{id}`. A
  long-running consumer renews instead of restarting: `POST …/leases/{id}/renew` pushes the expiry
  out by the link's TTL (at most 30 days from issue), and the provider follows where the product
  enforces its own expiry (a Postgres role's `VALID UNTIL`). `roll run --watch` renews its leases
  every 10 minutes. `Values.dynamic` names the keys that hold one, for a consumer that can't renew
  (a service that scales to zero refuses them). Only
  the lease's metadata is stored, never the credential.

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

| `search` | `url`, `host`, `port`, `username` | an API key of its own, credential `key`: `api_key` (`Authorization: ApiKey …`; a dynamic one expires by itself 5 minutes after its lease) | no (never the cluster's password) |

`search` resources are linkable once roll search's workspace kind lands (`feature/roll-search`).
Products without a workspace kind yet (queues, tasks, roll apps) join by adding their kind to the
workspace and registering a provider.

## Consumption

### `roll secrets` and `roll run`

```
roll secrets ls      [-p project] [-e env] [--path /]          # keys, masked
roll secrets get     KEY [--plain]
roll secrets set     KEY=VALUE… | KEY (value from stdin) [--comment] [--tag]
roll secrets rm      KEY…
roll secrets import  .env [--overwrite]                         # one change (.env or JSON)
roll secrets edit                                               # the folder in $EDITOR, one change
roll secrets set     KEY --generate 48                          # a random value made locally
roll secrets export  [--format dotenv|json|shell|yaml]
roll secrets envs    ls | add NAME [--base ENV --ephemeral] | rename OLD NEW | rm NAME | order A B C
roll secrets history [KEY]   roll secrets rollback --to SEQ | --at TIME
roll secrets diff    --from development --to staging
roll secrets promote --from development --to staging [KEY…]
roll secrets audit   [--key KEY]
roll run [-p project] [-e production] [--path /] [--allow-unresolved] [--watch] -- <command…>
```

- The project is `-p`/`ROLL_PROJECT` (a name or id), the environment `-e`/`ROLL_ENV`, the path
  `--path`/`ROLL_SECRETS_PATH`. Without them, `roll secrets setup -p shop -e staging [--path /api]`
  remembers them for the current directory and the ones under it, in the user's own
  `~/.config/roll/secrets.json` (never the repo), so `roll run -- npm start` needs no flags there
  (`--show`, `--clear`). Then `development` and `/`. A scoped token (`ROLL_TOKEN`) implies its
  project.
- `roll run --watch [--interval 5]` keeps the command as a child and restarts it (SIGTERM, then
  SIGKILL after 10 s) when the environment changes. It polls `…/digest`, a hash of the folder's
  effective keys, versions, fingerprints and links, and of what their references reach (in this
  environment or another), that decrypts nothing, issues nothing and isn't audited
  (`Secrets::digest` in process). A change that doesn't resolve keeps the running command. A live
  link's resource rotating doesn't change the digest.
- `roll secrets edit` opens the folder's own keys as a `.env` in `$EDITOR` (a 0600 file under
  `$XDG_RUNTIME_DIR` when there is one, removed after) and applies what changed as one change
  (`import` with `replace`: a removed line deletes its key; linked and personal keys aren't in the
  file and never deleted). `set KEY --generate N` makes a random value of N letters and digits.
- `roll run` fetches the resolved values (`…/values`), adds them to the child's environment and
  execs the command. It writes no file, and it fails before running anything if a reference is
  unresolved. Variables already set in the shell keep their value unless `--override` is passed.

### Runtime resolve for services (roll apps)

roll apps (docs/apps/) deploys a service into an environment and injects that environment's
secrets when the service starts. It uses one call:

- **In process** (`Secrets::resolve`): `resolve(project, env, path, &Consumer::App { service })`
  returns an `Env` map of `Value`s (redacted in `Debug`, zeroized on drop). Dynamic links issue
  leases owned by the consumer, and the call is audited as `resolve` by `app:<service>`.
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
  "Ephemeral environments") and resolves it as `preview:<slug>`.
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
- **Way around.** Rotate the secret and remove the value from the change. For a false positive, an
  admin lands with `bypass` (on a queue repo, the queue bypass; on a direct-land repo, the bypass of
  review rules, which now covers `secret_leak` too), audited as `secret_leak_bypass`.

### Moving from another manager

`roll secrets import` takes a `.env` file or JSON: an object of `KEY: value` (numbers and booleans
as their text) or an array of `{key, value, comment?}`. Export from the old manager, then import
into each environment:

```
infisical export --env=prod --format=json > prod.json   &&  roll secrets import prod.json -e production
doppler secrets download --no-file --format json > prod.json  &&  roll secrets import prod.json -e production
vercel env pull .env.production --environment=production  &&  roll secrets import .env.production -e production
```

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
they change: GitHub Actions secrets, Vercel, Cloudflare Workers, AWS Parameter Store, an endpoint
of the team's own, or a deploy hook that only hears that something changed. Infisical's "secret
syncs" are the reference. roll secrets stays the source of truth: a sync never reads from its
target, and a value edited there is overwritten at the next
change of that key.

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

### Platform credentials (`vercel`, `cloudflare`, `aws-ssm`)

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
X-Roll-Signature: sha256=<hex HMAC-SHA256(secret, "<timestamp>.<body>")>
```

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
| `read` | names, metadata, versions, history, compare, folders, imports, requests, links, leases, audit, required keys, listing syncs |
| `reveal` | `…/values`, `…/secrets/{key}/value`, `…/versions/{v}/value`, `…/requests/{id}/values`, `…/resolve` |
| `write` | set, batch changes, rotate, rollback, promote, folders, imports, create an environment, open or apply a request |
| `delete` | a secret, an environment, ephemeral environments by label, a lease |
| `admin` | rename or reorder environments, protection and approval policy, reviewing a request, syncs (create, delete, run) |

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
- **Folders.** A caller limited to some folders sees only their changes, change items, requests and
  audit rows; leases name no folder, so it sees none.
- **Fingerprints** are shown only to a caller who may reveal the value: a writer could otherwise
  test guesses against values it can't read. The grid's and compare's statuses still say whether
  two environments match, to a caller who can read both.
- Token scopes name environments by slug: a token for `pr-117` reaches a new `pr-117` made after the
  old one was deleted (it needs `reveal` on the new one's base to make it).

Until roll-access lands, the checks are one trait in `crates/roll-secrets/src/access.rs`:

```rust
pub trait Authority: Send + Sync {
    fn caller(&self, parts: &http::request::Parts) -> Result<Caller, Denied>;   // who
    fn allows(&self, caller: &Caller, action: Action, at: &Scope) -> bool;       // may they
}
```

The built-in authority accepts the operator (`roll_auth::Operator`, or any request when rolld has no
operator token) and **scoped tokens** (`rlst_…`, created under `…/tokens`) for one project, a list of
environments (or all), a list of path prefixes (or all), the actions above and an optional expiry.
Tokens are stored as SHA-256 hashes, looked up on every request and never cached. When roll-access
lands, its evaluator replaces the built-in authority, the `rlst_` tokens migrate to its scoped tokens,
and the routes stay the same.

### Protected environments and approvals

An environment can be **protected** with a policy: `{"approvals": 1, "approvers": [principal…],
"notify_url"?}`. With `notify_url` (a Slack or Discord incoming webhook, or any HTTPS URL; kept
sealed, the environment shows only its host as `notify`), each request opened on it is announced:
the notice is written in the request's own transaction and delivered by the sweeper with retries,
as `{text, content, project, environment, request: {id, by, message, keys}, link}` (key names,
never values).
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
  `{sink, project, events: AuditEntry[]}`, signed like syncs (`X-Roll-Sink`, `X-Roll-Timestamp`,
  `X-Roll-Signature: sha256=HMAC(secret, "ts.body")`; the `whsec_…` secret is shown once and kept
  sealed). Each sink keeps a cursor; the sweeper claims a sink in a write before delivering, so no
  two nodes send one batch, and a failed batch is sent again 5 minutes later. Deliveries go through
  the same `PublicOnly` client as syncs. At most 10 sinks a project.


## HTTP API

Bodies are JSON and every error is `{"error": string, "code"?: string}`. Paths below are under
`/v1/secrets/projects/{project}`. `{env}` is an environment slug, `{key}` a secret key, and the folder
is the `path` query parameter (default `/`). `{project}` is a workspace project id; a project that
isn't in the workspace is 404.

| Route | Body | Answer |
| --- | --- | --- |
| `GET /v1/secrets/health` | | `{ok, key: "env"\|"file"\|"missing", resealed: "all"\|number\|null, links: string[], syncs: string[]}` (`links`, `syncs`: the kinds registered) |
| `GET` | | `{project, environments: Environment[], previews: number, history_days: number \| null}` |
| `PATCH` | `{history_days}` | `{history_days}` (`admin`) |
| `GET /grid?path=` | | `Grid` |
| `GET /environments` | | `Environment[]` in order |
| `POST /environments` | `NewEnvironment` | 201 `Environment` |
| `DELETE /environments?label=k=v` | | `{deleted: string[]}` (ephemeral only) |
| `PUT /environments/order` | `{order: slug[]}` | `Environment[]` |
| `GET /environments/{env}` | | `Environment` |
| `PATCH /environments/{env}` | `{slug?, name?, expires_at?, max_age_days?}` | `Environment` |
| `DELETE /environments/{env}?confirm=` | | 204 |
| `PUT /environments/{env}/policy` | `{protected, approvals?, approvers?, notify_url?}` | `Environment` |
| `GET /environments/{env}/folders?path=` | | `{path, folders: Folder[]}` |
| `POST /environments/{env}/folders` | `{path}` | 201 `Folder` |
| `DELETE /environments/{env}/folders?path=` | | `{change: Change \| null}` |
| `GET /environments/{env}/imports?path=` | | `Import[]` |
| `PUT /environments/{env}/imports?path=` | `Import[]` | `Import[]` |
| `GET /environments/{env}/secrets?path=` | | `Secret[]` (own, imported, inherited; no values) |
| `GET /environments/{env}/secrets/{key}?path=&type=` | | `Secret` |
| `PUT /environments/{env}/secrets/{key}?path=` | `SetSecret` | `Written` |
| `DELETE /environments/{env}/secrets/{key}?path=&type=` | | `Written` |
| `GET /environments/{env}/secrets/{key}/value?path=&resolve=` | | `{value, problems}` |
| `GET /environments/{env}/secrets/{key}/versions?path=&type=` | | `Version[]` newest first |
| `GET /environments/{env}/secrets/{key}/versions/{v}/value?path=&type=` | | `{value}` |
| `POST /environments/{env}/secrets/{key}/rotate?path=` | | `Applied` |
| `POST /environments/{env}/secrets/{key}/purge?path=` | `{before_version?}` | `{purged}` (`admin`) |
| `GET /environments/{env}/values?path=&format=&resolve=&dynamic=` | | `Values` or text |
| `GET /environments/{env}/insights?path=` | | `Insights` (read) |
| `GET /environments/{env}/digest?path=` | | `{digest}` (read: changes when the values would) |
| `POST /environments/{env}/resolve` | `{path?, consumer, seal_to?}` | `Values` or `{sealed, context, problems, leases}` |
| `GET /environments/{env}/changes?before=&limit=` | | `Change[]` newest first |
| `POST /environments/{env}/changes` | `Batch` | 201 `Applied` |
| `POST /environments/{env}/import` | `{dotenv \| json, path?, overwrite?, replace?, message?, request?}` | 201 `Applied & {skipped}` or `{request, skipped}` |
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
| `GET /audit?env=&key=&actor=&action=&before=&since=&limit=&format=` | | `AuditEntry[]` newest first, or CSV/JSONL with `format` |
| `GET /required` | | `Required[]` |
| `PUT /required` | `Required[]` | `Required[]` (replaces the list; `admin`) |
| `GET /shares` | | `Share[]` newest first |
| `POST /shares` | `{ciphertext, note?, views?, ttl_minutes?}` | 201 `Share` |
| `DELETE /shares/{id}` | | 204 |
| `GET /syncs` | | `{syncs: Sync[], kinds: SyncKind[]}` |
| `POST /syncs` | `NewSync` | 201 `Sync & {signing_secret?}` (shown once; `admin`) |
| `DELETE /syncs/{id}` | | 204 (`admin`) |
| `POST /syncs/{id}/run` | | `Sync` after pushing every key (`admin`; 409 `running` while another push holds it) |
| `GET /v1/secrets/shares/{id}` (public) | | `{note, views_left, expires_at}` |
| `POST /v1/secrets/shares/{id}/open` (public) | | `{ciphertext}` (one open used) |
| `GET /audit/sinks` | | `Sink[]` (`admin`) |
| `POST /audit/sinks` | `{url, from_start?}` | 201 `Sink & {secret}` (shown once) |
| `DELETE /audit/sinks/{id}` | | 204 |
| `GET /v1/secrets/whoami` | | `{principal, project?, scopes?}` |

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
};
type NewEnvironment = {
  slug: string; name?: string; kind?: "standard" | "ephemeral"; base?: string; labels?: Record<string, string>;
  expires_at?: number; ttl_minutes?: number; secrets?: Op[]; // ephemeral: created in one call with its overrides
};
type Folder = { path: string; name: string; secrets: number; folders: number };
type Import = { environment: string; path: string };
type Link = { kind: string; ref: string; field: string; mode: "live" | "managed" | "dynamic"; options?: Record<string, unknown> };
type Secret = {
  id: string; key: string; path: string; type: "shared" | "personal"; version: number;
  comment: string; tags: string[]; link: Link | null; fp: string; references: string[];
  updated_at: number; updated_by: string;
  source: { kind: "own" } | { kind: "import"; environment: string; path: string } | { kind: "base"; environment: string };
  overridden: boolean; // a personal value of the caller replaces it
};
type SetSecret = { value?: string; comment?: string; tags?: string[]; type?: "shared" | "personal"; link?: Link | null; message?: string };
type Op =
  | { op: "set"; key: string; value?: string; comment?: string; tags?: string[]; type?: "shared" | "personal"; link?: Link | null }
  | { op: "delete"; key: string; type?: "shared" | "personal" };
type Batch = { path?: string; message?: string; ops: Op[]; if_seq?: number };
type Written = { change: Change | null; secret: Secret | null; warnings: string[] };
type Applied = { change: Change | null; items: Item[]; warnings: string[]; request?: Request }; // change: null when nothing changed; request: opened instead (promote into a protected environment)
type Item = { key: string; path: string; type: "shared" | "personal"; what: string[]; from_fp: string | null; to_fp: string | null };
type Change = { seq: number; environment: string; path: string; message: string; by: string; at: number; keys: number; kind: "write" | "rollback" | "promote" | "rotate" | "request" | "rename" };
type ChangeDetail = Change & { items: Item[]; state?: { key: string; path: string; version: number; fp: string; comment: string; tags: string[]; link: Link | null; at: number }[] };
// what: added | changed | deleted | comment | tags | link | rotated
type Version = { version: number; seq: number; op: "set" | "delete"; what: string[]; purged: boolean; fp: string | null; comment: string; tags: string[]; link: Link | null; at: number; by: string };
type Values = { values: Record<string, string>; problems: { key: string; problem: string }[]; leases: string[]; dynamic: string[] }; // leases: ids of dynamic credentials issued; dynamic: the keys holding one
type Grid = { path: string; environments: string[]; rows: { key: string; cells: Record<string, Cell | null>; status: "same" | "different" | "missing"; required: boolean }[]; folders: string[]; previews: number };
type Insights = { path: string; max_age_days: number | null; reads_since: number; keys: { key: string; version: number; updated_at: number; updated_by: string; last_read_at: number | null; last_read_by: string | null; reads_30d: number; stale: boolean }[] };
type Required = { key: string; path: string; comment: string };
type Sink = { id: string; url: string; status: "new" | "ok" | "error"; error: string | null; cursor: number; delivered_at: number | null; created_at: number; created_by: string };
type Share = { id: string; note: string; views_left: number; created_by: string; created_at: number; expires_at: number; opened_at: number | null; open: boolean };
type Cell = { fp: string; version: number; updated_at: number; link: Link | null; reference: boolean; source: "own" | "import" | "base"; comment: boolean; personal: boolean };
type Compare = { from: string; to: string; path: string; rows: { key: string; status: "same" | "different" | "only_from" | "only_to"; from: number | null; to: number | null }[] };
type Request = { id: string; environment: string; path: string; message: string; by: string; at: number; state: "open" | "approved" | "rejected" | "applied" | "closed" | "stale"; items: Item[]; reviews: { by: string; verdict: "approve" | "reject"; comment: string; at: number }[]; approvals: number; approvals_needed: number; applied_seq: number | null };
type LinkTarget = { kind: string; ref: string; name: string; fields: { field: string; label: string; modes: Link["mode"][]; secret: boolean; suggest: string; credential?: string }[] };
type Lease = { id: string; environment: string; key: string; kind: "dynamic" | "retired"; link: Link | null; consumer: string; issued_at: number; expires_at: number; revoked_at: number | null };
type Token = { id: string; name: string; environments: string[] | null; paths: string[] | null; actions: Action[]; expires_at: number | null; created_at: number; created_by: string; last_used_at: number | null; prefix: string };
type NewToken = { name: string; environments?: string[]; paths?: string[]; actions: Action[]; expires_at?: number; ttl_days?: number };
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
leases(id PK, project_id, env_id, secret_id, key, consumer, handle, issued_at, expires_at, revoked_at)
tokens(id PK, project_id, name, hash UNIQUE, prefix, environments, paths, actions, expires_at,
       created_at, created_by, last_used_at, revoked_at)
audit(id INTEGER PK, project_id, at, actor, action, env_id, path, keys, summary, ip)
syncs(id PK, project_id, env_id, path, kind, target, keys, prefix, secret BLOB, status, error,
      last_digest, state, last_synced_at, running_until, created_at, created_by)
      -- secret: the sealed signing secret; state: {name: stamp} of what was pushed (no values)
```

`owner` is `''` for a shared secret and the principal for a personal one. A deleted secret keeps its
row with `op = 'delete'` in `versions` and is removed from `secrets`, so `(env, path, key, owner)` is
free again and its history stays readable by key.

## Next

Done and in PR #20: the store and crypto, rolld's mount and link providers (Postgres, Redis,
streams, buckets, NoSQL, search), `roll secrets`, `roll secrets setup`, `roll run [--watch]`,
checks and sandboxes, leaked secrets don't land, required keys, one-time sharing, syncs (GitHub,
sealed webhooks, deploy notifications, Vercel, Cloudflare Workers, AWS Parameter Store), the
dashboard's Secrets surface, and the fixes from a
security review. `pnpm test:code` is green on the head. Reviewed on 2026-11-12 against Infisical,
Doppler, Vercel and 1Password Developer, and against the apps, previews, MCP and search branches.
Ranked:

1. **Swap the stopgap authority for roll-access** when `feature/roll-mcp` (PR #19) lands: done and
   tested on `wip/secrets-on-mcp` (this branch merged with roll-mcp): `Secrets::caller` reads
   `roll_auth::Grant` before the operator marker, `GrantGrants` answers second resources through
   `Grant::allows` (an `ask` is a no inside the product), whoami's project comes from the grant's
   rules, a sync made by a grant re-checks it through `roll_auth::Grants`, rolld opens secrets
   after roll-access. Merge it in when #19 is on main; then retire `rlst_` tokens for roll-access's.
   Also: roll apps' `SecretSource` wired to `Secrets::values` as `app:<svc>` (whichever of #20 and
   #21 merges second; the adapter is in the apps thread).
2. **Search links live** once roll search (#22) lands its workspace kind: check the provider
   against a real cluster. AWS Secrets Manager and session tokens for `aws-ssm`.
3. **A second security audit** of what came after the first: syncs, audit sinks, notices, one-time
   shares, purge and retention, insights, request values, lease renewal, the TypeScript client. In
   progress.
4. **CLI parity**: `roll secrets syncs|sinks ls|add|run|rm`, `purge`, `retention`, `insights`,
   `shares`, `leases renew`, so nothing needs the dashboard. In progress.
5. **The dashboard's controls** for purge (a secret's history), retention (the project's settings)
   and `notify_url` (an environment's protection).
6. **Keep `wip/secrets-on-mcp` current** with this branch (merge it again after each round here),
   so the roll-access swap is ready the day #19 lands.
7. **Performance on Postgres**: the 2k/9k-key measurements (import, grid, values, digest) against
   the system database, and indexes where they're slow.
