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
right folder: `expired`, `missing` (a required key an environment doesn't define, through its
imports and base), `exposed` (a protected value also in an unprotected environment), `weak`,
`expiring`, `stale`, then `unread` (nobody revealed, exported, resolved or synced it in 90 days,
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

| `search` | `url`, `host`, `port`, `username` | an API key of its own with data access only (no keys or users; `"access": "data"`), credential `key`: `api_key` (`Authorization: ApiKey …`; a dynamic one expires by itself 5 minutes after its lease) | no (never the cluster's password) |

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
roll run [-p project] [-e production] [--path /] [--allow-unresolved] [--watch] -- <command…>
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
```

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

  A login lasts its TTL: enough for a job. A long-running `roll run --watch` uses a token.
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
| `PUT /environments/{env}/secrets/{key}?path=` | `SetSecret` | `Written` |
| `DELETE /environments/{env}/secrets/{key}?path=&type=` | | `Written` |
| `GET /environments/{env}/secrets/{key}/value?path=&resolve=` | | `{value, problems}` |
| `GET /environments/{env}/secrets/{key}/versions?path=&type=` | | `Version[]` newest first |
| `GET /environments/{env}/secrets/{key}/versions/{v}/value?path=&type=` | | `{value}` |
| `POST /environments/{env}/secrets/{key}/rotate?path=` | | `Applied` |
| `POST /environments/{env}/secrets/{key}/purge?path=` | `{before_version?}` | `{purged}` (`admin`) |
| `GET /environments/{env}/values?path=&format=&resolve=&dynamic=&name=&namespace=&seq=&at=` | | `Values` (with `seq`: as stored after that change, `seq` in the answer), or text with `format`: `dotenv`, `shell`, `json`, `yaml`, `docker` (`--env-file`; a value with a line break is a 400 naming the key) or `kubernetes` (a Secret named `name`, default the environment, in `namespace`) |
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
| `GET /rotators` | | `Rotator[]` (`read` on the project) |
| `POST /rotators` | `{name, url}` | 201 `Rotator & {secret}` (`admin`; the signing secret shown once) |
| `DELETE /rotators/{id}` | | 204; 409 while a key links to it (`admin`) |
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
type Values = { values: Record<string, string>; problems: { key: string; problem: string }[]; leases: string[]; dynamic: string[]; linked: string[] }; // leases: ids of dynamic credentials issued; dynamic: the keys holding one; linked: keys from any link (addresses a consumer on another network rewrites)
type Grid = { path: string; environments: string[]; rows: { key: string; cells: Record<string, Cell | null>; status: "same" | "different" | "missing"; required: boolean }[]; folders: string[]; previews: number };
type Insights = { path: string; max_age_days: number | null; reads_since: number; keys: { key: string; version: number; updated_at: number; updated_by: string; last_read_at: number | null; last_read_by: string | null; reads_30d: number; stale: boolean }[] };
type Required = { key: string; path: string; comment: string; type?: "url" | "int" | "port" | "bool" | "json" | "email"; pattern?: string; owner?: string /* who supplies it */ };
type Sink = { id: string; url: string; status: "new" | "ok" | "error"; error: string | null; cursor: number; delivered_at: number | null; created_at: number; created_by: string };
type ProjectFindings = { project: string; project_name: string; findings: Finding[] };
type Finding = { issue: "expired" | "missing" | "exposed" | "weak" | "expiring" | "stale" | "unread" | "unreferenced"; environment: string; path: string; key: string; detail: string };
type Rotator = { id: string; name: string; host: string; created_at: number; created_by: string };
type Share = { id: string; note: string; views_left: number; created_by: string; created_at: number; expires_at: number; opened_at: number | null; open: boolean };
type Cell = { fp: string; version: number; updated_at: number; link: Link | null; reference: boolean; source: "own" | "import" | "base"; comment: boolean; personal: boolean; misfit?: string };
type Compare = { from: string; to: string; path: string; rows: { key: string; status: "same" | "different" | "only_from" | "only_to"; from: number | null; to: number | null }[] };
type Request = { id: string; environment: string; path: string; message: string; by: string; at: number; state: "open" | "approved" | "rejected" | "applied" | "closed" | "stale"; items: Item[]; reviews: { by: string; verdict: "approve" | "reject"; comment: string; at: number }[]; approvals: number; approvals_needed: number; applied_seq: number | null };
type LinkTarget = { kind: string; ref: string; name: string; fields: { field: string; label: string; modes: Link["mode"][]; secret: boolean; suggest: string; credential?: string }[] };
type Lease = { id: string; environment: string; path: string; key: string; kind: "dynamic" | "retired"; link: Link | null; consumer: string; issued_at: number; expires_at: number; revoked_at: number | null };
type Identity = { id: string; name: string; issuer: string; audience: string; subject: string; claims: Record<string, string>; environments: string[] | null; paths: string[] | null; actions: Action[]; ttl_minutes: number; created_at: number; created_by: string; last_used_at: number | null; last_subject: string | null; allowed_ips: string[] | null };
type NewIdentity = { name: string; issuer: string; audience: string; subject: string; claims?: Record<string, string>; environments?: string[]; paths?: string[]; actions: Action[]; ttl_minutes?: number; allowed_ips?: string[] };
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

Reviewed again on 2026-11-13 (evening): against Doppler, Infisical, Vault and 1Password Developer,
and across the five branches. The core is level with them (environments, versions, references,
approvals, dynamic secrets and rotation, OIDC identities, syncs, scanning, audit sinks). The gaps
that matter: two permission systems; roll apps deploys without project secrets (`NoSecrets`); no
time-boxed access requests; no runtime SDK (Doppler's and Infisical's fetch secrets in process);
no Terraform provider.

Ranked (work that can start now):

1. **roll apps, the rest** (with roll apps): linked keys' addresses rewritten for a container
   (`Values.linked` → apps' `runtime.reach`; the `SecretSource` trait carries pairs only); preview
   environments ensured from their base when apps makes one; apps' `variables?reveal` needing
   `reveal` on secrets too. The wiring itself is #31.
2. **Access requests through roll-access's "ask"**: a person asks for time-boxed `reveal` on a
   protected environment, and an approver grants it for minutes (the approval names the
   environment and folder).
3. **Runtime clients beyond TypeScript.** `@roll/secrets` (packages/secrets) already fetches an
   environment's values with a token, opens sealed exports, watches the digest and renews dynamic
   leases. Go and Python clients with the same surface (and a workload identity's login built in),
   as Doppler's and Infisical's SDKs have.
4. **Products say who owns a resource**, so a person's project can link their own databases,
   buckets and streams: an `Owners` answer from each product's records (`Workspace::trusts`).
5. **A Terraform provider** (`roll_secret`, `roll_secrets_environment`, data sources for values),
   as Doppler's and Infisical's.

Open pull requests after this one:

- **#30, one permission system** (after #20 and #25): a project's tokens are roll-access grants,
  identities' logins get `workload` tokens, `rlst_` is gone.
- **#31, roll apps' deploys get their project's secrets** (after #20): rolld's `ForApps`.

Done since the last review: roll search's merge (#22: its links checked live, the conflicts additive); signed-in people manage the secrets of their own projects (People,
under Access), with the canvas guards. main (roll MCP #19 and accounts #18) merged in. roll secrets asks the
caller's roll-access Grant (`GrantGrants`): a grant's request is checked inside the product too,
health and leaks filter themselves, search and findings take any grant, ephemeral environments are
roll-access's `preview` class, and a sync made by a grant pushes only while the grant may.
