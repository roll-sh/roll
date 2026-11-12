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
  `crates/rolld/src/secrets.rs`, which also implements the link providers.
- Client: `apps/dashboard/src/lib/secrets-api.ts`. Change it and this contract together.
- CLI: `roll secrets …` and `roll run` in `crates/roll-cli/src/secrets.rs`.
- Tests: `crates/roll-secrets/tests/api.rs` (the router in process, on SQLite or Postgres),
  `crates/roll-secrets/tests/nodes.rs` (two stores racing on one database),
  `crates/roll-cli/tests/secrets.rs` (`roll run` against a live rolld),
  `apps/dashboard/src/lib/secrets-*.test.ts`.

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
  as a **lease**. Leases are revoked when they expire, or at once with `DELETE …/leases/{id}`. Only
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
(roles live in the branch's data); it expires with `VALID UNTIL` either way. Revoking a role ends
its sessions and gives what it made (a migration's tables) to `roll` before dropping it, so revoking
a credential never drops data.

Products without a workspace kind yet (queues, tasks, roll search, roll apps) join by adding their
kind to the workspace and registering a provider.

## Consumption

### `roll secrets` and `roll run`

```
roll secrets ls      [-p project] [-e env] [--path /]          # keys, masked
roll secrets get     KEY [--plain]
roll secrets set     KEY=VALUE… | KEY (value from stdin) [--comment] [--tag]
roll secrets rm      KEY…
roll secrets import  .env [--overwrite]                         # one change
roll secrets export  [--format dotenv|json|shell|yaml]
roll secrets envs    ls | add NAME [--base ENV --ephemeral] | rename OLD NEW | rm NAME | order A B C
roll secrets history [KEY]   roll secrets rollback --to SEQ | --at TIME
roll secrets diff    --from development --to staging
roll secrets promote --from development --to staging [KEY…]
roll secrets audit   [--key KEY]
roll run [-p project] [-e production] [--path /] [--allow-unresolved] -- <command…>
```

- The project is `-p`/`ROLL_PROJECT` (a name or id), the environment `-e`/`ROLL_ENV`, the path
  `--path`/`ROLL_SECRETS_PATH`. Without them, `roll secrets setup -p shop -e staging [--path /api]`
  remembers them for the current directory and the ones under it, in the user's own
  `~/.config/roll/secrets.json` (never the repo), so `roll run -- npm start` needs no flags there
  (`--show`, `--clear`). Then `development` and `/`. A scoped token (`ROLL_TOKEN`) implies its
  project.
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

## Governance

### Access

The roll-wide permission model (docs/permissions.md, `crates/roll-access`, branch
`feature/roll-mcp`) decides who may do what: rolld's gate classifies each request by its route
pattern and method into the product `secrets`, a resource (`{project, environments, paths}`) and one
action:

| Action | Routes |
| --- | --- |
| `read` | names, metadata, versions, history, compare, folders, imports, requests, links, leases, audit, required keys |
| `reveal` | `…/values`, `…/secrets/{key}/value`, `…/versions/{v}/value`, `…/resolve` |
| `write` | set, batch changes, rotate, rollback, promote, folders, imports, create an environment, open or apply a request |
| `delete` | a secret, an environment, ephemeral environments by label, a lease |
| `admin` | rename or reorder environments, protection and approval policy, reviewing a request |

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
operator token) and **scoped tokens** (`rls_…`, created under `…/tokens`) for one project, a list of
environments (or all), a list of path prefixes (or all), the actions above and an optional expiry.
Tokens are stored as SHA-256 hashes, looked up on every request and never cached. When roll-access
lands, its evaluator replaces the built-in authority, the `rls_` tokens migrate to its scoped tokens,
and the routes stay the same.

### Protected environments and approvals

An environment can be **protected** with a policy: `{"approvals": 1, "approvers": [principal…]}`.
A write to a protected environment doesn't apply. It answers 409 `approval_required`, and the
client opens a **change request** with the same operations (`POST …/requests`). A request:

- holds its operations with values encrypted, and shows a diff without values;
- needs `approvals` approvals from approvers other than its author (an empty approver list means any
  principal with `admin`);
- applies as one change when approved (`…/apply`), and only if no key it touches changed since it
  was opened (otherwise it is `stale` and must be refreshed);
- can be applied without approvals by a caller with `admin` passing `{"bypass": true}`. The bypass
  is recorded in the audit log.

Rotation of a managed link and dynamic leases on a protected environment don't need approval: they
don't change what the secret is, only its credential.

### Audit log

Every read of values and every write is recorded: time, principal (`operator`, `token:rls_…`,
`app:…`), action, environment, path, keys, a summary and the client address (the peer's, or the
first `X-Forwarded-For` one with `ROLL_CODE_TRUST_FORWARDED_FOR`). A value never is.
Listing metadata isn't audited. Revealing, exporting, resolving and every write are.
`GET …/audit?env=&key=&actor=&action=&before=&limit=` reads it newest first. Rows are kept for 400
days.

## HTTP API

Bodies are JSON and every error is `{"error": string, "code"?: string}`. Paths below are under
`/v1/secrets/projects/{project}`. `{env}` is an environment slug, `{key}` a secret key, and the folder
is the `path` query parameter (default `/`). `{project}` is a workspace project id; a project that
isn't in the workspace is 404.

| Route | Body | Answer |
| --- | --- | --- |
| `GET /v1/secrets/health` | | `{ok, key: "env"\|"file"\|"missing", resealed: "all"\|number\|null, links: string[]}` (`links`: the kinds with a link provider) |
| `GET` | | `{project, environments: Environment[], previews: number}` |
| `GET /grid?path=` | | `Grid` |
| `GET /environments` | | `Environment[]` in order |
| `POST /environments` | `NewEnvironment` | 201 `Environment` |
| `DELETE /environments?label=k=v` | | `{deleted: string[]}` (ephemeral only) |
| `PUT /environments/order` | `{order: slug[]}` | `Environment[]` |
| `GET /environments/{env}` | | `Environment` |
| `PATCH /environments/{env}` | `{slug?, name?, expires_at?}` | `Environment` |
| `DELETE /environments/{env}?confirm=` | | 204 |
| `PUT /environments/{env}/policy` | `{protected, approvals?, approvers?}` | `Environment` |
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
| `GET /environments/{env}/values?path=&format=&resolve=&dynamic=` | | `Values` or text |
| `POST /environments/{env}/resolve` | `{path?, consumer, seal_to?}` | `Values` or `{sealed, context, problems, leases}` |
| `GET /environments/{env}/changes?before=&limit=` | | `Change[]` newest first |
| `POST /environments/{env}/changes` | `Batch` | 201 `Applied` |
| `POST /environments/{env}/import` | `{dotenv, path?, overwrite?, message?, request?}` | 201 `Applied & {skipped}` or `{request, skipped}` |
| `GET /environments/{env}/changes/{seq}?state=` | | `ChangeDetail` |
| `POST /environments/{env}/rollback` | `{to?: seq, at?: ms, message?, bypass?}` | 201 `Applied` |
| `GET /compare?from=&to=&path=` | | `Compare` |
| `POST /environments/{env}/promote?from=` | `{path?, keys?, message?}` | 201 `Applied` |
| `GET /environments/{env}/requests?state=` | | `Request[]` |
| `POST /environments/{env}/requests` | `Batch` | 201 `Request` |
| `GET /environments/{env}/requests/{id}` | | `Request` |
| `POST /environments/{env}/requests/{id}/review` | `{verdict: "approve"\|"reject", comment?}` | `Request` |
| `POST /environments/{env}/requests/{id}/apply` | `{bypass?}` | `Request` |
| `POST /environments/{env}/requests/{id}/close` | | `Request` |
| `GET /links` | | `LinkTarget[]` |
| `GET /leases` | | `Lease[]` |
| `DELETE /leases/{id}` | | 204 |
| `GET /tokens` | | `Token[]` |
| `POST /tokens` | `NewToken` | 201 `Token & {token}` (shown once) |
| `DELETE /tokens/{id}` | | 204 |
| `GET /audit?env=&key=&actor=&action=&before=&limit=` | | `AuditEntry[]` newest first |
| `GET /required` | | `Required[]` |
| `PUT /required` | `Required[]` | `Required[]` (replaces the list; `admin`) |
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
type Version = { version: number; seq: number; op: "set" | "delete"; what: string[]; fp: string | null; comment: string; tags: string[]; link: Link | null; at: number; by: string };
type Values = { values: Record<string, string>; problems: { key: string; problem: string }[]; leases: string[] }; // leases: ids of dynamic credentials issued
type Grid = { path: string; environments: string[]; rows: { key: string; cells: Record<string, Cell | null>; status: "same" | "different" | "missing"; required: boolean }[]; folders: string[]; previews: number };
type Required = { key: string; path: string; comment: string };
type Cell = { fp: string; version: number; updated_at: number; link: Link | null; reference: boolean; source: "own" | "import" | "base"; comment: boolean; personal: boolean };
type Compare = { from: string; to: string; path: string; rows: { key: string; status: "same" | "different" | "only_from" | "only_to"; from: number | null; to: number | null }[] };
type Request = { id: string; environment: string; path: string; message: string; by: string; at: number; state: "open" | "approved" | "rejected" | "applied" | "closed" | "stale"; items: Item[]; reviews: { by: string; verdict: "approve" | "reject"; comment: string; at: number }[]; approvals: number; approvals_needed: number; applied_seq: number | null };
type LinkTarget = { kind: string; ref: string; name: string; fields: { field: string; label: string; modes: Link["mode"][]; secret: boolean; suggest: string; credential?: string }[] };
type Lease = { id: string; environment: string; key: string; kind: "dynamic" | "retired"; link: Link | null; consumer: string; issued_at: number; expires_at: number; revoked_at: number | null };
type Token = { id: string; name: string; environments: string[] | null; paths: string[] | null; actions: Action[]; expires_at: number | null; created_at: number; created_by: string; last_used_at: number | null; prefix: string };
type NewToken = { name: string; environments?: string[]; paths?: string[]; actions: Action[]; expires_at?: number; ttl_days?: number };
type Action = "read" | "reveal" | "write" | "delete" | "admin";
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
        link_state, updated_at, updated_by, UNIQUE(env_id, path, key, owner))
versions(secret_id, version, seq, env_id, path, key, owner, op, value BLOB, fp, comment, tags, link,
         at, by, PRIMARY KEY (secret_id, version))
changes(project_id, seq, env_id, path, kind, message, by, at, keys, PRIMARY KEY (project_id, seq))
requests(id PK, project_id, env_id, path, message, by, at, state, ops BLOB, items, reviews, applied_seq)
leases(id PK, project_id, env_id, secret_id, key, consumer, handle, issued_at, expires_at, revoked_at)
tokens(id PK, project_id, name, hash UNIQUE, prefix, environments, paths, actions, expires_at,
       created_at, created_by, last_used_at, revoked_at)
audit(id INTEGER PK, project_id, at, actor, action, env_id, path, keys, summary, ip)
```

`owner` is `''` for a shared secret and the principal for a personal one. A deleted secret keeps its
row with `op = 'delete'` in `versions` and is removed from `secrets`, so `(env, path, key, owner)` is
free again and its history stays readable by key.

## Next

Done and in PR #20: the store and crypto, rolld's mount and link providers (a Postgres role checked
against a real compute), `roll secrets`, `roll secrets setup` and `roll run`, checks and sandboxes,
the dashboard's Secrets surface. Ranked:

1. **Postgres role passwords off the compute's log**: issue roles with a SCRAM verifier instead of
   a plaintext `PASSWORD`, so a failing statement can't log it (`log_min_error_statement`).
2. **Swap the stopgap authority for roll-access** when `origin/feature/roll-mcp` (PR #19) lands
   `roll_auth::Grant`: `Secrets::caller` reads the Grant before the `Operator` marker (whichever
   merges second does it), and `rls_` tokens migrate to roll-access tokens. Its MCP tools map onto
   the routes above.
3. **Syncs to outside**: push an environment to GitHub Actions secrets (through `code-github`),
   Vercel, AWS Parameter Store, Cloudflare Workers; one-way, versioned, audited.
4. **Change webhooks** through roll code's delivery queue: `secrets.changed` for deploy hooks.
5. **Grid without decrypting**: store whether a value has references, so the grid decrypts nothing.
