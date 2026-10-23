# roll storage: architecture

roll storage is S3-compatible object storage with branching buckets. Any S3 SDK or tool works
against it (`aws`, `rclone`, boto3, the JS v3 SDK, …).

Design goal, in order:

1. **Never lose or corrupt an acknowledged object.**
2. **S3 compatibility** for what real SDKs and tools actually send.
3. **Price parity with S3.** The margin comes from AWS, not from risky storage tricks
   (see [Pricing](#pricing)).

## Decisions

- **One customer object is one S3 object (a "blob").** No packing, no shared containers, no
  compaction rewriting live data. Every byte inherits S3's own durability (11 nines,
  3 AZs), and a bug in our code can at worst affect metadata, which is itself recoverable.
- **Blobs are immutable and never overwritten.** Each upload gets a fresh random blob id. An
  overwrite writes a new blob and repoints the key.
- **Deletes are delayed, twice.**
  - The customer sees a recovery window (`recover_days`, default 7, 0–30): deleted and
    overwritten objects can be restored from the bucket's trash.
  - Below that, a blob that nothing references is only physically deleted after an internal
    quarantine (24 h, not configurable by customers), so a refcount bug can be repaired.
- **Metadata durability equals data durability.** A write is acknowledged only after its
  metadata change is in object storage too (group commit, the tasks log-store pattern).
  The whole metadata database can be rebuilt from object storage alone.
- **Branches are copy-on-write and instant.** A branch shares its parent's blobs; nothing is
  copied when it's created.
- **Copies are free.** `CopyObject` adds a reference to the same blob (unless it changes the
  encryption; see [Server-side encryption](#server-side-encryption)).
- **Encryption is the object store's.** SSE-C and SSE-KMS parameters are passed through on the
  blob's own requests and S3 does the crypto; roll never re-encrypts bytes on an S3 backend.
- **Checksums end to end.**
  - client → gateway: `Content-MD5` and the `x-amz-checksum-*` headers and trailers are verified.
  - gateway → S3: every backend upload carries a CRC32C that S3 verifies.

## Crates

| crate | what |
|---|---|
| `s3-proto` | Wire protocol, no I/O: SigV4, SigV4a and SigV2 (header, presigned, chunked/streaming payloads with trailers, POST policies), SSE request headers, XML bodies, S3 error codes, checksums, ETags, ranges and conditional headers. |
| `s3-store` | Metadata (SQLite + durable log in object storage), branching, refcounts, trash, lifecycle, keys, metering, the blob backend (S3 via `aws-sdk-s3`, or a local directory), the GC/scrub jobs. No HTTP. |
| `s3-server` | The S3 HTTP API: routing (path and virtual-host style), auth, identities and policy evaluation, handlers, streaming to and from the backend, CORS and public buckets; the STS and IAM query APIs on the same listener. |
| `s3-select` | S3 Select, no I/O: the request XML, the SQL dialect (parser and evaluator), CSV and JSON input (GZIP, BZIP2) and output, fed the object's bytes as they stream, with bounded memory. |
| `s3-image` | Image transformations, no I/O: format sniffing, decoding (JPEG, PNG, WebP, GIF) with size and pixel limits checked before allocating, EXIF orientation, ICC to sRGB, SIMD resizing (cover, contain, fill), deterministic encoding (JPEG, PNG, WebP, AVIF). `s3-server` runs it on a bounded blocking pool and caches its output as derived blobs. |
| `s3-control` | Control-plane API `/v1/storage/...` (merged into rolld's API): accounts, buckets, branches, keys, roles, browsing/upload/download for the dashboard, presigning, trash, usage and pricing. |
| `s3-api` | The developer API: a Supabase-Storage-compatible REST API (`/storage/v1`) over the same store and evaluator, with signed URLs, TUS resumable uploads and roll's extras (see [api.md](api.md)). |

`rolld` runs all of it in one process (the S3 API on `--s3`, default `127.0.0.1:59000`; the
developer API on `--storage-api`, default `127.0.0.1:59001`), the CLI has `roll storage` (alias
`s3`), and the dashboard has `/dashboard/storage`.

## Object storage layout

Everything goes in rolld's backing bucket (`ROLL_S3_BUCKET`, `ROLL_S3_PREFIX`), or a local
directory without one:

```text
s3/blobs/<2-char shard>/<blob id>          one customer object (immutable)
s3/meta/wal/<seq, 20 digits>                metadata log entry (group commit), create-only
s3/meta/snap/<seq, 20 digits>.db.zst        metadata snapshot as of that entry
s3/meta/pruned                              log entries up to here are deleted
```

- Blob ids are 26 random lowercase base32 chars. The 2-char shard is the id's first two
  chars, which spreads load across S3 key partitions.
- Blobs are written with storage class `INTELLIGENT_TIERING` (`ROLL_S3_STORAGE_CLASS`;
  `STANDARD` for MinIO/SeaweedFS in tests). Metadata objects stay `STANDARD`.
- On S3, blobs carry the user metadata `x-amz-meta-roll-bucket` (bucket id) and
  `x-amz-meta-roll-key` (base64url of the key). These are a last-resort way to identify a
  blob if both the log and the snapshots were lost. They're not used in normal operation.
- Customer multipart uploads map 1:1 to a backend multipart upload of the blob (same part
  numbers), so a 5 TiB object is still one S3 object. `UploadPartCopy` uses the backend's
  `UploadPartCopy`, so the copy happens server-side.
- SSE-C and SSE-KMS blobs are encrypted by S3 itself. Reading an SSE-C blob (and HEADing it) needs
  the customer's key; listing and deleting don't, so the GC jobs never need keys.

## Metadata (`s3-store`)

### Schema (SQLite)

```sql
buckets (id TEXT PRIMARY KEY,           -- bkt_<12>
         name TEXT NOT NULL UNIQUE,     -- S3 bucket name rules; unique across accounts
         account_id TEXT NOT NULL,      -- the owner (migration 8)
         region TEXT NOT NULL, created_at INTEGER NOT NULL,   -- unix ms
         public INTEGER NOT NULL,       -- anonymous GET/HEAD of objects
         recover_days INTEGER NOT NULL,
         cors TEXT, lifecycle TEXT,     -- JSON (see API), NULL = none
         parent_id TEXT, parent_at INTEGER,                   -- branch of, and when (ms)
         objects INTEGER NOT NULL, bytes INTEGER NOT NULL,    -- own visible rows
         trash_bytes INTEGER NOT NULL,
         versioning TEXT,               -- NULL (never versioned) | 'Enabled' | 'Suspended'
         object_lock TEXT,              -- NULL = off; JSON { mode, days, years } (default retention)
         versions INTEGER NOT NULL, version_bytes INTEGER NOT NULL)  -- own noncurrent rows

objects (bucket_id, key TEXT, blob_id TEXT,  -- the key's current version
         size, etag, content_type, cache_control, content_disposition, content_encoding,
         content_language, expires, metadata TEXT /* JSON map */, tags TEXT /* JSON map */,
         checksum_algorithm, checksum_type /* FULL_OBJECT|COMPOSITE */, checksum,
         parts INTEGER /* 0 = single PUT */,
         part_sizes TEXT /* JSON [size, …], or { sizes, checksums } when the parts' checksums are kept */,
         modified_at INTEGER /* ms */,
         acl TEXT /* JSON grants; NULL = private */, website_redirect TEXT,
         vid TEXT,                      -- version id, sorts newest first within a key ('' for tombstones)
         is_null INTEGER,               -- the "null" version (unversioned and suspended writes)
         delete_marker INTEGER,         -- blob_id NULL + delete_marker = a delete marker;
                                        -- blob_id NULL alone = a tombstone (branches only)
         lock_mode TEXT, lock_until INTEGER /* ms */, legal_hold INTEGER /* NULL = never set */,
         owner TEXT,                    -- the developer API uploader's sub (migration 12)
         PRIMARY KEY (bucket_id, key)) WITHOUT ROWID

versions (bucket_id, key, noncurrent_at INTEGER /* ms */, <same columns as objects>,
          PRIMARY KEY (bucket_id, key, vid)) WITHOUT ROWID   -- noncurrent versions and markers

trash   (bucket_id, key, deleted_at, blob_id, <same columns as objects>,
         PRIMARY KEY (bucket_id, key, deleted_at))

blobs   (id TEXT PRIMARY KEY, size INTEGER, crc32c INTEGER, refs INTEGER NOT NULL,
         created_at INTEGER, unreferenced_at INTEGER)   -- set when refs drops to 0

uploads (id TEXT PRIMARY KEY /* customer UploadId */, bucket_id, key, blob_id,
         backend_upload_id TEXT, <object header columns>, created_at, acl, website_redirect,
         lock_mode, lock_until, legal_hold,          -- Object Lock from CreateMultipartUpload
         owner)                                      -- migration 12


bucket_config (bucket_id, kind TEXT, id TEXT /* '' = one per bucket */, value TEXT,
         PRIMARY KEY (bucket_id, kind, id)) WITHOUT ROWID
         -- kind: tagging, policy, public_access_block, ownership, acl, encryption, website,
         -- logging, accelerate, notification, replication, webhook_secret, limits (one per bucket);
         -- analytics, metrics, inventory, intelligent_tiering (by id). Branches copy their
         -- parent's, except the policy (its resources name the parent), notifications,
         -- replication and the webhook secret; deleted buckets drop theirs.
parts   (upload_id, part_number, size, etag, checksum, backend_etag, created_at, crc32c,
         PRIMARY KEY (upload_id, part_number))

keys    (id TEXT PRIMARY KEY /* access key id, 20 chars: RK + 18 upper base32 */,
         secret TEXT NOT NULL /* 40 chars */, name TEXT NOT NULL,
         permission TEXT NOT NULL /* read | write | admin */,
         buckets TEXT /* JSON array of bucket names; NULL = all; [] = none (IAM users' keys) */,
         created_at INTEGER, last_used_at INTEGER,
         account_id TEXT NOT NULL, user_name TEXT /* the IAM user; NULL = an account key */,
         status TEXT /* Active | Inactive */)            -- the last three: migration 8

usage   (bucket_id, day TEXT /* YYYY-MM-DD UTC */, class_a, class_b, bytes_in,
         egress_internet, egress_internal,
         byte_ms REAL /* stored bytes × ms; a float, since a PB-day overflows i64 */,
         egress_cached /* bytes the CDN served */, egress_cdn_origin /* bytes it pulled */,
         PRIMARY KEY (bucket_id, day))                  -- the last two: migration 12
cdn_batches (id TEXT PRIMARY KEY, received_at)          -- CDN usage batches counted (7 days)
usage_payer (account_id /* the requester who paid */, bucket_id, day, class_a, class_b,
         egress_internet, egress_internal,
         PRIMARY KEY (account_id, bucket_id, day))      -- Requester Pays, migration 7

orphans (blob_key TEXT PRIMARY KEY, size, found_at)   -- orphan-scan candidates
state   (k TEXT PRIMARY KEY, v TEXT)                  -- scrub report, job barriers, canonical owner id,
                                                      -- event_seq (the outbox's counter), job:inventory:*

blob_sse (blob_id TEXT PRIMARY KEY,                   -- no row: the backing bucket's default
          algorithm TEXT /* SSE-C | aws:kms | aws:kms:dsse */,
          customer_key_md5 TEXT /* SSE-C: base64 MD5 of the key, never the key */,
          kms_key_id, kms_context, bucket_key, created_at)  -- migration 4

events  (seq INTEGER PRIMARY KEY /* the sequencer; from state.event_seq, never reused */,
         bucket_id, key, dest TEXT /* destination ARN | replicate:<bucket> | backfill:<rule> */,
         kind TEXT /* notify | replicate | backfill */, body TEXT /* JSON */, created_at,
         attempts, next_at, error)                    -- the event outbox (see below), migration 5
replication_status (bucket_id, key, version_id /* S3's: 'null' or the vid */,
         status TEXT /* PENDING | COMPLETED | FAILED | REPLICA */,
         PRIMARY KEY (bucket_id, key, version_id)) WITHOUT ROWID
roles   (account_id, name TEXT /* IAM role name rules */, id TEXT /* RR… (control plane) | AROA… (IAM) */,
         description, permission /* read | write | admin; NULL = an IAM role */, buckets /* JSON; NULL = all */,
         policy TEXT /* a control-plane role's bound (inline IAM policy, JSON); NULL = none */,
         trust TEXT /* JSON { keys, oidc, external_id } */, max_session_s,
         generation INTEGER /* +1 on every change */, revoked_before INTEGER /* ms; 0 = none */,
         created_at, updated_at,
         iam TEXT /* JSON { path, trust_policy, policies, attached, boundary, tags }; NULL = none */,
         PRIMARY KEY (account_id, name)) WITHOUT ROWID       -- migration 6, keyed by account in 8
access_points (account TEXT /* 12 digits */, kind TEXT /* ap | olap */, name TEXT,
         alias TEXT UNIQUE /* <name>-<random>-s3alias | …--ol-s3 */,
         bucket TEXT /* ap: the bucket's name; olap: the supporting access point's */,
         network_origin TEXT /* Internet | VPC */, vpc_id, policy TEXT /* JSON */,
         block TEXT /* the access point's public access block, JSON */,
         config TEXT /* olap: the transformations, JSON */, created_at,
         PRIMARY KEY (account, kind, name)) WITHOUT ROWID       -- migration 7
replication_targets (name TEXT PRIMARY KEY, endpoint /* NULL = AWS S3 */, region,
         access_key_id /* NULL = the deployment's AWS credentials */,
         secret TEXT /* AES-256-GCM-sealed under state.replication_target_key */,
         path_style, buckets TEXT /* JSON */, created_at, updated_at)   -- migration 7
replica_versions (bucket_id, key, version_id /* the source version */, dest /* <bucket>@<target> */,
         target_version_id /* the version the target made of it */, replicated_at,
         PRIMARY KEY (bucket_id, key, version_id, dest)) WITHOUT ROWID   -- migration 9
blob_torrent (blob_id TEXT PRIMARY KEY, piece_length INTEGER, pieces BLOB /* 20-byte SHA-1s */,
         created_at)                                   -- GetObjectTorrent's cache, migration 10
derived (source_blob TEXT, params TEXT /* SHA-256 of the resolved transformation */,
         blob_id TEXT UNIQUE /* the derived image's own blob */, size, content_type,
         created_at, last_used_at,
         PRIMARY KEY (source_blob, params)) WITHOUT ROWID       -- image transformations, migration 12
image_usage (bucket_id, month TEXT /* YYYY-MM UTC */, source_blob, first_at,
         PRIMARY KEY (bucket_id, month, source_blob)) WITHOUT ROWID  -- billed, migration 12

accounts (id TEXT PRIMARY KEY /* 12 digits */, canonical TEXT UNIQUE /* 64 hex */,
          name TEXT /* display name */, email TEXT UNIQUE /* lowercase; NULL = none */,
          created_at)                                         -- migration 8
iam     (account_id, kind TEXT /* user | group | policy | mfa | oidc */,
         name TEXT /* lowercase: IAM names are case-insensitive */, value TEXT /* JSON */,
         PRIMARY KEY (account_id, kind, name)) WITHOUT ROWID  -- migration 8

api_projects (account_id TEXT PRIMARY KEY, value TEXT /* JSON: generation, issued_at, anon_key and
         service_key (sealed), jwt_secret (sealed), jwt_issuer, jwt_audience, oidc */,
         updated_at) WITHOUT ROWID                            -- the developer API, migration 12
api_uploads (id TEXT PRIMARY KEY /* = uploads.id */, account_id, length INTEGER /* NULL = deferred */,
         part_size INTEGER, value TEXT /* JSON { upsert, metadata, signed } */, created_at,
         expires_at) WITHOUT ROWID                             -- TUS uploads, migration 12; index on expires_at
```

Object ACLs, website redirects and the developer API's `owner` live in the row, so every row
copy carries them: branch push-down, the trash and restores, in-place updates (tagging), and
multipart uploads from Create to Complete. `CopyObject` sets the ACL and redirect from the
request, as S3 does; a copy keeps the source's `owner` unless the developer API names a new one.

The deployment's **canonical owner id** (64 hex chars) is random, created on the first start
and kept in `state`, so it survives restarts, recovery and moving hosts. It's the canonical id
of the **default account**, whose 12-digit id is derived from it (the first 15 hex digits, mod
10¹²), as the deployment's account id always was. Other accounts get random ids.

An object's **owner** (its account, when it isn't the bucket's) is kept in its ACL document
(`acl`, S3's `AccessControlPolicy` has an `Owner`), so every copy of the row carries it as the
ACL already did: branch push-down, the trash, versions, uploads from Create to Complete.

`refs` counts every `objects`, `versions`, `trash` and `derived` row that points at the blob. `objects`,
`versions` and `trash` have an index on `blob_id`, so the GC and the orphan scan can check that
nothing points at a blob before it goes.

Migrations are append-only entries of `MIGRATIONS` in `schema.rs`; log entries carry the schema
version they were written with and replay migrates before applying newer ones. Migration 2
added bucket configuration and object ACLs; migration 3 added versioning and Object Lock (rows
from before it became null versions, with a `vid` made from their `modified_at` the same way
new ids are); migration 4 added `blob_sse`; migration 5 added the event outbox
and replication statuses; migration 6 added `roles` (temporary credentials; sessions themselves
are never stored, see [Temporary credentials](#temporary-credentials-sts)); migration 7 added
`access_points` and `replication_targets`.
Migration 8 added accounts and IAM: `accounts`, `iam`, `usage_payer`, the owner account of buckets, keys and roles
(roles rebuilt keyed by account and name), keys' IAM user and status. It makes the deployment's
owner the default account, computing its id from `state.canonical_owner` in SQL exactly as
`config::account_id` does, and moves every existing bucket, key and role into it; it's
deterministic, so replaying the log over an older snapshot gives the same rows. A database
without an owner yet gets its default account when the store first opens.
Migration 9 added `replica_versions`: for replication outside roll, which version of the
destination each source version became (the source's own id on another roll deployment; the
target's on AWS S3 and other services), so a replica can be found from its source and kept in
sync. Nothing existing changes.
Migration 10 added `blob_torrent`.

`blob_torrent` is a cache keyed by blob, like `blob_sse`: a blob's bytes never change, so its
BitTorrent pieces are computed once (by reading it) and serve every key, copy, branch and
version that shares it. It's written relaxed (a lost row is computed again), only while the
blob's row exists, and removed with the blob's row by the sweeper.

Directory buckets (S3 Express) need no schema of their own: a bucket is one exactly when its name
has the form `<base>--<zone id>--x-s3`, which general purpose names never have
(`s3-store/src/express.rs`). The store refuses versioning and Object Lock for them, and keeps
branches of directory buckets directory buckets.
Migration 11 indexes `iam` by `(kind, name)` (account aliases are unique across the deployment,
and sign-in finds an account by its alias); login profiles, SSH keys, certificates, SAML
providers, password policies, credential reports and the throttle counts are rows of `iam`, so
it adds no tables.
Migration 12 added `derived` and `image_usage` (image transformations): nothing existing changes.

A **derived image** (an object resized, compressed or converted, `s3-server/src/render.rs`) is a
blob like any other: written once under a fresh id, with a `blobs` row whose one reference is
its `derived` row, keyed by the source blob and the resolved transformation. Source blobs never
change, so the cache never needs invalidating: an overwrite is a new source blob with new
entries. The entry is written durably (it adds a blob) and only while the source has
`refs > 0`; `last_used_at` is updated relaxed, at most daily. The sweeper drops entries unused
for `derived_expiry_days` (30, `ROLL_S3_DERIVED_EXPIRY_DAYS`), and when it deletes a source blob
it drops that blob's entries in the same transaction; either way the derived blob loses its
reference and goes through the quarantine like any other. A derived image of an SSE-KMS source
is encrypted with the same KMS parameters (a `blob_sse` row); SSE-C sources are never
transformed. `image_usage` records each source blob transformed per bucket per month, once,
durably (it's billed).

Migration 13 added the CDN's columns to `usage` (`egress_cached`, `egress_cdn_origin`, both
defaulting to 0, so existing rows are unchanged) and `cdn_batches`. A bucket's CDN setting is a
`bucket_config` row (`cdn`), the deployment's CDN settings a `state` row (`cdn_settings`, the
Cloudflare API token sealed like replication targets' secrets), and purges are outbox rows
(`cdn_purge`): no other tables.

Migration 14 added the developer API's state: an `owner` column on `objects`, `versions`, `trash`
and `uploads` (the `sub` of the app user that wrote the object, Supabase's `owner_id`; `NULL` for
every existing row and for writes through the S3 API), `api_projects` (an account's API keys and
auth providers) and `api_uploads` (resumable uploads). Bucket limits are `bucket_config` rows of
kind `limits`, so they need no table.

A blob's encryption is fixed when it's written, so `blob_sse` is keyed by blob: every row that
shares the blob (copies, branches, the trash) shares its encryption, and no row of `objects`,
`trash` or `uploads` changes. The row is written in the transaction that records the blob (PUT,
copy) or its upload (CreateMultipartUpload), and removed with the blob's row (sweeper) or the
upload (abort).

### Durability: the metadata log

This is the same pattern as `crates/tasks/src/store/log.rs`, minus tenants.

- Every write is one SQLite transaction recorded with a SQLite **session**.
- Its patchset joins the next **group commit**: one create-only (`If-None-Match: *`) object at
  `s3/meta/wal/<seq>`, written at most every `commit_ms` (default 20 ms).
- The call returns once that entry is in object storage. A failed commit fails every write in
  it, and the node fences itself: no further writes, and no reads either (fail-stop), until
  restart.
- Each entry carries the schema version it was written with, and ends in a CRC32C of the
  whole entry. A corrupt entry stops recovery with an error; nothing is skipped.
- **Fencing.** On start, a node writes a takeover entry at the next sequence number. A second
  writer's commit then fails on the create-only check, so it stops. Two writers can never
  interleave. While idle, a node HEADs the next entry every 5 s (`fence_check`), so a deposed
  node stops serving stale reads without having to write.
  - Fencing only works if object storage honours `If-None-Match: *`. On every start, a node
    writes a probe key create-only twice and refuses to start unless the second write is
    rejected.
  - A create-only write that finds its slot taken reads the entry back. If it holds exactly
    the bytes being written (the response to an earlier attempt, or to the client library's
    own retry, was lost), the commit succeeded. Only different bytes mean another writer, so
    the node fences.
  - If the committer task panics, the node fences: waiting writes fail and reads stop, rather
    than hanging.
- **Snapshots.** These are SQLite online backups, zstd-compressed, written every
  `snapshot_commits` (default 10 000) entries or `snapshot_bytes` (64 MiB) of log. They're
  copied a few pages at a time while serving, cut at an exact entry, then downloaded again
  and checked (`PRAGMA quick_check`) before they count. The newest two verified ones are
  kept, and log entries up to the older of the two are deleted. A snapshot that fails its
  check is deleted (or, failing that, deleted with the next prune).
- **Recovery.**
  1. Walk the snapshots newest first and load the first one that decodes, passes the check
     and still has every log entry after it. Read errors are retried a few times; they don't
     count as corruption. With no usable snapshot, recovery only proceeds if nothing was ever
     pruned. Corrupt and unread snapshots are deleted once a newer one is verified.
  2. Replay the log after it, without gaps, migrating the schema where entries need it. A row
     already holding a change's values is overwritten with them. A change that can't apply
     (its row is missing, or it breaks a constraint) stops recovery, as does an entry from a
     newer schema or one stored under the wrong sequence number.
  3. Write the takeover entry, then serve.

  The local SQLite file is only a cache.

### Branching (copy-on-write)

A branch is a bucket with `parent_id`. Reads resolve a key like this:

1. own row: a tombstone means "absent"; otherwise that row
2. otherwise the parent's row, resolved the same way, recursively

To keep a branch's view frozen at its creation, **writes to a bucket that has branches push
the old state down first**. For each child that has no own row for the key:

- if the parent's view had a row (its own or inherited) → copy that row into the child
  (`refs + 1`)
- if the parent's view had no row → insert a tombstone in the child

This happens in the same transaction as the parent's write. Creating a branch costs one row.

Two restrictions:

- A bucket with branches can't be deleted (409) until its branches are.
- Listing a branch merges its own rows with its ancestors' in key order. Nearer buckets
  win, and tombstones hide keys.

`objects`/`bytes` on a bucket count its own non-tombstone rows; that's what it's billed for.
Inherited objects are billed to the bucket that owns the rows.

### Versioning

A bucket is never versioned (`versioning` NULL), `Enabled` or `Suspended`; it can't go back to
never versioned. `objects` holds each key's **current** version, which may be a **delete
marker**; `versions` holds the **noncurrent** ones. A bucket only has noncurrent versions of a
key whose current row it owns.

- **Version ids** (`vid`) are 16 hex digits of `i64::MAX - ms` followed by 16 random base32
  characters: URL-safe, 32 characters, sorting newest first. A new version's id is made
  strictly smaller than every id the bucket has or sees for the key (one millisecond "later"
  than the newest if the clock went back or two writes share a millisecond), so a key's history
  is always in `vid` order with its current version first. S3 shows `null` for the null version
  (`is_null`), which still has a `vid` for ordering.
- **Never versioned:** as before. An overwrite or delete moves the old row to the trash.
- **`Enabled`:** an overwrite or a DELETE moves the current row to `versions` (`noncurrent_at`
  = now); a DELETE adds a delete marker, even for an absent key (as on S3). Nothing goes to the
  trash.
- **`Suspended`:** new writes and DELETE markers are the null version; the key's previous null
  version (current or noncurrent) is replaced, which is a permanent delete (below). A non-null
  current version becomes noncurrent.
- **Permanent deletes** (`DELETE ?versionId=`, lifecycle noncurrent expiry, a replaced null
  version) move the version to the bucket's trash for `recover_days` (or drop its reference if
  `recover_days` is 0). Delete markers carry no data and simply go. If the deleted version was
  current, the newest noncurrent one becomes current; if none is left, the key is absent there
  (a tombstone hides the parent's in a branch).
- **Restore from the trash** (`POST …/trash/restore`) in a versioned bucket puts the version
  back at its place in the key's history, by `vid`: behind a newer current version (or delete
  marker) it's noncurrent again, otherwise it's current. A restored null version replaces the
  key's null version. In a never-versioned bucket a restore is a new write, as before.
- **Reads:** GET/HEAD of a key whose current version is a marker is `404` with
  `x-amz-delete-marker: true`; asking for a marker by id is `405`. `?versionId=` resolves the
  bucket's own current row and noncurrent versions first, then the current row it inherits.
- **Tags, retention and legal holds** change a version in place (no new version). Children keep
  the old metadata of a current version (push-down, as for any write).
- **ListObjectVersions** merges the bucket's view of current versions (inherited ones too, with
  markers) with its own noncurrent versions, in key order, newest first within a key, rolling
  keys up into common prefixes. `key-marker`/`version-id-marker` resume inside a key.

**Branches.** A branch inherits its parent's versioning state and Object Lock configuration when
it's made. Its history of a key is its own rows plus whatever current version it inherits; the
parent's noncurrent versions aren't part of it. Push-down copies the parent's current row
(markers included) with the same `vid`, so version ids are stable across a branch and its
parent. A branch's own write over an inherited version copies that version into the branch's
`versions` (a new reference) before writing.

**Object Lock.** Enabled at CreateBucket (`x-amz-bucket-object-lock-enabled`, which turns
versioning on) or with PutObjectLockConfiguration on a versioning-enabled bucket; then the
bucket can't be suspended and Object Lock can't be turned off. A version is **locked** while it
has a legal hold or a retention (`GOVERNANCE`/`COMPLIANCE`) that hasn't run out. New versions
get the bucket's default retention unless the request sets one; copies get their own, never the
source's.

- A locked version is never deleted for good: not by `DELETE ?versionId=`, DeleteObjects,
  lifecycle (skipped), trash expiry (a trashed row still under a lock stays until it runs out),
  a forced bucket delete or the control plane. Every one of these checks the lock inside the
  transaction that would delete, before changing anything.
- A DELETE without a version id still adds a delete marker, as on S3.
- `x-amz-bypass-governance-retention: true` (`s3:BypassGovernanceRetention`) lifts a GOVERNANCE
  retention for a delete or a retention change. Nothing lifts COMPLIANCE or a legal hold. A
  version deleted with the bypass loses its retention, as on S3: it still goes to the trash for
  `recover_days` (restorable, now unlocked) but no longer holds its bucket, so a bucket emptied
  that way can be deleted at once (its trash goes with it; the blobs wait out the quarantine).
  A branch's push-down copy of that version keeps its own lock.
- COMPLIANCE can only be extended; GOVERNANCE can be extended or made COMPLIANCE, and shortened
  or removed only with the bypass.
- A bucket holding any locked row (current, noncurrent or trashed) can't be deleted, forced or
  not. Push-down copies carry the lock, so a branch that holds a copy of a locked version
  can't be deleted while it lasts either.

### Deletes, trash, quarantine, GC

- **Delete or overwrite** (never-versioned bucket). The old row moves to `trash` (if
  `recover_days > 0`), otherwise `refs - 1`. In a branch, deleting or overwriting an inherited
  object puts a copy of it in the branch's trash (`refs + 1`) and hides the key with a
  tombstone. Versioned buckets keep old versions as noncurrent instead (see
  [Versioning](#versioning)); only permanent deletes reach the trash.
- `refs` never goes below zero, and a blob at zero is never referenced again: either would
  be a bookkeeping bug, so the write fails instead.
- **Restore.** The trash row moves back to `objects`. Whatever is there now goes to the trash.
  (Versioned buckets: see [Versioning](#versioning).)
- **Sweeper (hourly).** It starts with a barrier (a durable write, which fails on a deposed
  node), then does three things:
  - Trash rows older than `recover_days` are removed (`refs - 1`), except versions still under
    Object Lock.
  - Blobs with `refs = 0` and `unreferenced_at` older than 24 h are deleted from the
    backend, then their row is removed. Before each, the sweeper checks that no `objects`,
    `versions`, `trash`, `uploads` or `derived` row points at it; one that does means `refs` is
    wrong, so the blob is kept and the problem logged. A deleted blob's derived images go in the
    same transaction (their blobs are quarantined in turn).
  - Derived images unused for `derived_expiry_days` are dropped (`refs - 1` on their blob).
  - Uploads older than the bucket's abort rule (default 7 days) are aborted.
- **Lifecycle (daily).** Rules filter by prefix, tags (all must match) and size, and do:
  - `Expiration` `Days`/`Date`: matching current versions are deleted as a normal delete (to
    the trash, or a delete marker in a versioned bucket).
  - `NoncurrentVersionExpiration`: versions noncurrent for `NoncurrentDays`, beyond the newest
    `NewerNoncurrentVersions`, are deleted for good (to the trash).
  - `ExpiredObjectDeleteMarker`: delete markers with no noncurrent versions behind them go.
  - `Transition`/`NoncurrentVersionTransition` are stored and returned, never acted on (data
    is already in Intelligent-Tiering).

  Everything is listed first and checked again inside the delete transaction (still that
  version, still old enough, still matching the filter, not locked), so a version written or
  retagged after the listing is never expired by it.
- **DeleteBucket.** Without `force`, the bucket's view (current versions, delete markers and
  noncurrent versions, as S3 requires) is checked empty inside the same transaction that
  deletes it, so a write that lands during the delete fails the delete (`BucketNotEmpty`)
  instead of being deleted with it. With `force`, current, noncurrent and trashed rows go in
  batches. Either way, a bucket with any version under Object Lock isn't deleted.
- **Orphan scan (daily).** A blob without a `blobs` row, an active upload or any version row
  pointing at it is left when a crash hits between the blob upload and its metadata commit.
  That write was never acknowledged. Deletion takes two scans, so a node that lost the log to another one (and
  doesn't know that node's new blobs) can never delete them:
  1. A scan records unknown blobs (not in flight on this node) in `orphans`, in a durable
     write, which fails on a deposed node.
  2. A scan at least 24 h later deletes the recorded ones still unknown.

  An unknown blob more than 7 days old means metadata went missing. It's never recorded or
  deleted, only logged and counted (`unexpected_orphans`).
- **Scrub (weekly).** It compares the backend listing with the `blobs` table and logs every
  referenced blob that's missing or has the wrong size (`tracing::error!`, and a counter in
  `GET /v1/storage`). Current, noncurrent or trashed versions pointing at a blob with no
  `blobs` row count as missing too. It never deletes.

### Events: notifications, replication, inventory

Event notifications and replication run off a **durable outbox** (`events.rs`), with these
rules:

- **Same transaction.** Every change S3 can notify about (`ObjectCreated:*`, `ObjectRemoved:*`,
  `LifecycleExpiration:*`, `ObjectTagging:*`, `ObjectAcl:Put`, `ObjectRestore:Completed` for a
  trash restore, `Replication:OperationFailedReplication`) calls `events::emit` inside the
  transaction that makes it. A write that fails or is fenced rolls its events back with it; one
  that commits has them in the log, so they survive restarts and recovery. Who made the change
  (access key, source IP, request id, `Put` or `Post`) comes from a task-local `Origin` that
  `s3-server` sets per request, so no store signature changed.
- **Free when unused.** `emit` reads the bucket's `notification` and `replication`
  configurations (one primary-key lookup) and writes nothing unless a configuration matches the
  event name and key filter (or a replication rule's prefix and tags).
- **One row per delivery,** all rows of one change sharing its sequencer. `seq` comes from a
  counter in `state` (`event_seq`), so it only grows, even after rows are deleted or the node
  recovers: S3's per-key `sequencer` comparison holds.
- **Only durable rows go out.** `Store::due_events` reads due rows with the batch that would make
  what it read durable (`Db::read_seen`), and waits for that batch before returning: a
  dispatcher never delivers an event whose write may still fail. A row is only handed out when
  no older row exists for the same destination and key, so each destination gets a key's events
  in order.
- **At least once.** Delivered rows are deleted with relaxed writes; failures update `attempts`
  and `next_at` (relaxed too). A lost relaxed write means a redelivery, never a lost event. Rows
  failing past the retention window (3 days) are dropped by the dispatcher, counted and logged.
- **Replication** rows are applied by `Store::replicate` in one durable transaction that also
  deletes the row: find the source version (`find_version`), add a reference to its blob, and
  place a copy with the same version id in the destination's history (`versioning::restore`'s
  placement), then set the statuses (`COMPLETED` once no other destination waits, `REPLICA` on
  the destination). Every refusal (destination gone, unversioned, a locked null version in the
  way) is checked before anything changes and makes the source `FAILED` with an
  `OperationFailedReplication` event. Running a row twice is a no-op (the row is gone, or the
  replica exists). Refs invariants hold: the replica is one more `objects`/`versions` row, so one
  more reference.
- **Backfill** rows (`ExistingObjectReplication`) walk the bucket's versions 500 at a time,
  queueing replication for those without a status, keeping their cursor in the row.
- **Buckets outside roll** (`s3-server/src/replicate_ext.rs`): a rule whose destination is on a
  replication target (`s3-store/src/targets.rs`) queues rows for `replicate:<bucket>@<target>`.
  They can't be applied in one metadata transaction (bytes have to move), so the dispatcher reads
  the source version (`Store::replica_source`), copies it, then ends the row in one durable
  write (`Store::finish_external_replication`: delete the row, record the version the target
  made of it in `replica_versions`, `COMPLETED` once no other destination waits, or `FAILED` with
  its event). A crash in between copies the version again: another roll deployment recognises
  the replica (same version id and content, below) and answers as before; elsewhere rewriting the
  same bytes to the same key is harmless, and nothing is lost. The copy is S3
  `CopyObject`/`UploadPartCopy` when the target is the backing bucket's own service with the
  same credentials (`Backend::s3_location`), and otherwise streamed from the blob (multipart
  from 64 MiB, each part a ranged read retried on its own, bounded memory). The blob's CRC32C
  must match on the target: sent as `x-amz-checksum-crc32c` with a single PUT, and combined from
  the parts' (each sent with its own) before a multipart upload is completed, or compared with a
  copy's. 4xx answers other than 408/409/429 fail the version at once; anything else is retried
  with the outbox's backoff.
  Multipart sources go part for part with their own part sizes (the replica's ETag is the
  source's), and the version's Object Lock retention and legal hold go with it.
- **Replicas that keep their version ids.** Every write of a replica to a target carries roll's
  replication extension (`x-roll-replica-version-id`, `x-roll-replica-last-modified`). A roll
  deployment receiving it (`replication.rs::replica_auth`: the identity needs
  `s3:ReplicateObject`/`ReplicateDelete`/`ReplicateTags` from a policy or an `admin` key) sets
  `Origin::replica` for the request, and the commit (`commit_row`, `complete_locked`,
  `delete_object_version`) places the row with that version id and `Last-Modified` through
  `events::place_replica` (versioning `Enabled` only; `versioning::restore`'s placement by
  version id) with status `REPLICA`, in the same transaction. `events::existing_replica` makes a
  retry idempotent (the same version id with the same size and CRC32C, or both delete markers:
  the stored replica answers, the new blob is discarded once the transaction is durable) and
  refuses a different version under that id. AWS S3 can't set version ids through its API, so
  there `replica_versions` holds S3's.
- **Metadata changes** (tags and ACL through `emit_update`, retention and legal hold through
  `put_retention`/`put_legal_hold`) of a version that's replicated (`COMPLETED`/`PENDING`), or of
  a `REPLICA` under a rule with `ReplicaModifications`, queue metadata-only `replicate` rows
  (`events::sync_metadata`), which wait behind the version's own row for that destination
  (per-key order). Between roll buckets the store applies them to the replica
  (`update_version`, Object Lock's rules with the bypass a replication role has); outside roll
  `replicate_ext.rs` sends PutObjectTagging/Retention/LegalHold to the version in
  `replica_versions`, with `x-roll-replica-sync`. Changes made by replication (`Origin::
  replica_sync`, or the store applying a row) never queue more rows, so two-way replication
  doesn't bounce. A failed metadata row sends `OperationFailedReplication` and leaves the
  version's status alone.
- **Replication Time Control.** Rows of rules with `ReplicationTime` carry `rtc`. Every 30 s the
  dispatcher runs `Store::replication_missed_threshold`: rows queued more than 15 minutes ago
  that haven't reported it emit `Replication:OperationMissedThreshold` and remember it (`missed`
  in the row, one durable write). A row that ends past the threshold emits
  `OperationReplicatedAfterThreshold` (and the missed event too, if the scan hadn't run), in the
  transaction that ends it. Both go through `emit`, so the bucket's notification configurations
  get them like any event.
- **Replication metrics** come from the outbox (`Store::replication_backlog`: pending
  operations and bytes, the oldest pending row, per rule) and from the dispatcher
  (`Notifier::replication_metrics`: the last and largest latency from a write to its replica,
  operations replicated, failed and replicated after the threshold, per rule, in memory since
  start). With `NotifyConfig::replication_metrics_interval` (and AWS delivery on) they're also
  sent to CloudWatch every minute (`publish_replication_metrics`: `PutMetricData`, S3's metric
  names, units and dimensions, in `roll/S3`).

The dispatcher, event JSON and delivery are `s3-server`'s (`notify.rs`): every 250 ms, and right
after any request that changed something (so replication is prompt), it takes up to 256 due rows, handles up to 32 at once (deliveries, or `replicate`/`backfill` in the store),
then deletes the delivered ones and reschedules failures (1 s doubling to 10 min, with jitter).
Roll queues and streams are reached through the `EventSink` trait, which rolld implements in
process with mq-core (publish to the queue, confirmed) and stream-broker (produce, keyed by the
object key, `acks=all`); the s3 crates don't depend on either. Webhooks go out over HTTPS with a
resolver that refuses internal addresses; AWS destinations are SigV4-signed calls with the
deployment's credentials. See [compatibility](compatibility.md#event-notifications).

**Replication targets' secrets** are sealed with AES-256-GCM (the session tokens' scheme) under
a dedicated 32-byte key in `state` (`replication_target_key`, created once and never rotated
away, so a sealed secret stays readable as long as the metadata exists). They're opened only to
build the target's S3 client, never returned by the API, and never printed (`Secret`'s `Debug`
is redacted).

**Inventory reports** (`s3-server/src/inventory.rs`) are checked hourly: each enabled inventory
configuration whose last run (`state` `job:inventory:<bucket id>:<id>`) is a day (or week) old
gets a report, listed with `Store::inventory_page` and written as gzip CSV data files, a
manifest, its checksum and a Hive symlink file through the normal put path. A report is
recorded as done only once its manifest is written.

### Write paths

- **PUT.** Upload the blob to the backend with its CRC32C. Then one metadata transaction:
  - insert the blob with `refs = 1`
  - upsert the object
  - give it its version id (and default retention), make the old version noncurrent (versioned)
    or move it to the trash, and push the old state down to children
  - check conditional writes (`If-None-Match: *`, `If-Match`) inside the transaction

  Wait for the commit, then reply. The blob stays registered as in flight until the request
  ends however it ends, including the client going away. A blob that never got committed is
  then the orphan scan's; one whose commit was already on its way is in the metadata, so
  it's kept.
- **Multipart.**
  - `CreateMultipartUpload` allocates the blob id and the backend upload and records
    `uploads` (durable) first.
  - Each part is a backend `UploadPart` plus a `parts` row.
  - `Complete` validates the part list (order, sizes ≥ 5 MiB except the last, ETags) and
    completes the backend upload. Then one transaction inserts the blob and the object and
    deletes the upload and its parts. The blob's CRC32C is combined from the parts'.
  - Completes of one upload run one at a time. A retry that finds the upload already
    completed into the key's current version returns that version (SDKs retry a slow
    Complete). A failed attempt never deletes a blob the metadata knows.
  - When the object's checksum is made from its parts' (an upload with a checksum algorithm),
    the parts' checksums are kept with the object, for GetObjectAttributes.
  - `UploadPartCopy` is server-side, so the gateway never sees the bytes. The part's ETag is
    the source's when it's a whole single-PUT object, otherwise an MD5 of the source blob
    and range.
- **Copy.** `refs + 1` on the source blob, then the new object row (with `REPLACE`d
  metadata, if asked). No data moves. A copy whose encryption differs from the source's can't
  share the blob: the backend copies it into a new blob (S3 `CopyObject`, or a multipart
  `UploadPartCopy` beyond 5 GiB), the new blob's CRC32C must equal the source's, and then it's
  committed like a PUT.
- **Append** (`x-amz-write-offset-bytes`, directory buckets; `s3-store/src/append.rs`). A blob
  is never changed, so an append writes a **new blob** with the old bytes and then the new ones:
  - the old object at least 5 MiB: a backend multipart upload of the new blob, part 1 an
    `UploadPartCopy` of the old blob (inside object storage; its CRC32C must equal the old
    blob's), part 2 the new data;
  - smaller: one backend PUT streaming the old blob's bytes (checked against its CRC32C as they
    go, or the PUT fails) and then the new data.

  Then one transaction checks that the key's current version is still the blob that was copied
  (`InvalidWriteOffset` if its size changed, `ConditionalRequestConflict` if it's another blob of
  the same size), inserts the new blob (its CRC32C combined from the two parts') and writes the
  version like any overwrite: the old one goes to the trash. Until then the new blob is in
  flight, then the orphan scan's: a crash anywhere before the commit leaves the old object and
  the refcounts exactly as they were. Cost: 4 backend requests above 5 MiB, a GET and a PUT of
  at most 5 MiB extra below; the old versions are billed as trash for `recover_days`.
- **Rename** (RenameObject; `s3-store/src/rename.rs`). One transaction: check both sides'
  conditions, `refs + 1` on the source's blob for the destination's new row (written like any
  write: the old destination to the trash, push-down to branches), then remove the source's
  row (`refs - 1`, not to the trash: its bytes moved) or, in a versioned bucket, put a delete
  marker over it. The blob never reaches zero references on the way, and no bytes move.

### Metering

`s3-server` records each request into the store's in-memory `Meter`:

- **class A:** PUT, POST, COPY, LIST*, CreateMultipartUpload, UploadPart, Complete, bucket
  writes
- **class B:** GET, HEAD and the rest
- **free:** DELETE and Abort, as on S3
- bytes in and bytes out

Egress counts as **internal** when the peer address is inside `internal_cidrs` (loopback,
private ranges and the docker bridge by default; set `ROLL_S3_INTERNAL_CIDRS` on AWS to the
VPC), and as **internet** otherwise.

Requests to a **Requester Pays** bucket from another account that accepted the charges
(`x-amz-request-payer: requester`) are metered to the requester (`Meter::request_by`,
`egress_by`): their class A/B requests and egress go to `usage_payer` (the requester's account,
the bucket, the day) instead of the bucket's `usage`. Storage is always the bucket owner's.

Every 60 s the meter flushes into `usage` (and `usage_payer`). It also adds
`(bytes + trash_bytes + version_bytes) × elapsed ms` for every bucket, which gives storage
GB-months, so restorable trash and noncurrent versions are billed as storage (as S3 bills
noncurrent versions).
Usage writes are relaxed: they ride the next group commit without waiting.

**Image transformations** are billed per unique source image per bucket per month
(`image_usage`, written durably the first time a source is transformed in a month, and only for
renders that succeed); `s3-control` prices them per account, after a monthly allowance.

Through the CDN (see [CDN](#cdn-cloudflare)): bytes the edge serves are **cached egress**
(`usage.egress_cached`), reported by the Worker in batches and written durably once per batch
id. A GET from the CDN to roll (a miss at the edge) is a class B request like any other, but its
bytes are an **origin pull** (`egress_cdn_origin`), not internet egress: the customer pays for
the cached egress instead, and origin pulls are our cost (the margin view). Gateway cache hits
are metered exactly like misses.

### Gateway cache (`s3-server/src/cache.rs`)

GETs read object bytes through a two-tier hot-object cache (memory, then local disk), so a hot
object is served without an S3 GET.

- **Keyed by blob id and chunk.** Blobs are immutable and every write gets a new blob, so an
  entry can't be stale and is never invalidated: a deleted or overwritten object's chunks are
  simply never asked for again and age out. Correctness never depends on a TTL.
- **Aligned chunks** (1 MiB) serve any range: a whole-object read fills the chunks a later
  range uses, and a range over a cached object hits. A miss fetches its run of missing chunks
  (up to 8) with one ranged GET; concurrent misses of a chunk share one fetch (single flight).
- **Admission.** Both tiers are `moka` caches with TinyLFU admission, weighted by bytes: a new
  chunk only displaces one that's asked for less often, so a one-off scan can't flush what's hot.
  A chunk reaches disk on its second fetch, or when memory evicts it after it was served from
  memory, so scans don't churn the disk either. Objects over `ROLL_S3_CACHE_MAX_OBJECT`
  (256 MiB) bypass the cache.
- **Checked disk entries.** A disk entry is `<dir>/<2 chars>/<blob>-<chunk>-<generation>`: a
  header with its CRC32C and length, then the bytes. Every read verifies it; a mismatch drops
  the entry (and the file) and the read falls through to S3: bad bytes are never served. What
  a previous run left is indexed at startup (blobs never change, so it's still good).
- **Never cached:** SSE-C objects (the key comes with each request), and SSE-KMS objects unless
  `ROLL_S3_CACHE_KMS` (S3 would check KMS permission on every read).
- **Sizes:** `ROLL_S3_CACHE_MEMORY` (256 MiB), `ROLL_S3_CACHE_DISK` (10 GiB) under
  `ROLL_S3_CACHE_DIR` (`<home>/storage/cache`); `0` turns a tier off.
- **Metrics** (`cache` in `GET /v1/storage`): memory and disk hits, joined fetches, misses,
  S3 GETs made and avoided, bytes saved, corrupt disk entries, bypassed reads, what's held.

### CDN (Cloudflare)

Cloudflare serves public reads at the edge; a Worker (`apps/cdn-worker`, deployed with
`wrangler`) sits in front of roll's public read endpoints. roll's side is `s3-server/src/cdn.rs`
and `s3-store/src/cdn.rs`.

- **Routes by path prefix** (the Worker's `ROLL_ROUTES`, the same list as the deployment's
  `routes`): `s3` (`/<bucket>/<key>`, S3 GET semantics), `immutable`
  (`/v/<token>/<bucket>/<key>`), `bucket-key` (`<prefix><bucket>/<key>` of another API, e.g. the
  storage REST API's `/storage/v1/object/public/`), and `website` (`<prefix><key>` of one
  bucket's website endpoint). Each route can have its own origin, so new public read APIs plug
  in by adding a route.
- **Edge checks.** Public objects pass. Signed URLs are verified at the edge, so a cached
  private object is never served without a valid one: SigV4 presigned URLs made with the CDN
  presigning key (`RKROLLCDNPRESIGN0000`, its secret published to the Worker as
  `ROLL_CDN_PRESIGN_SECRET`), and `?token=` JWTs (HS256 `ROLL_CDN_JWT_SECRET` or ES256
  `ROLL_CDN_JWT_ES256_JWK`, claims `url` = `<bucket>/<key>` and `exp`). Invalid or expired ones
  get 403 at the edge; header-signed requests and other keys' presigned URLs are refused there
  too. Cache keys never include credentials; credentialed copies live in their own namespace
  (`?roll-cdn-ns=private`, never accepted from clients) and are only served to requests with a
  valid credential for that object.
- **Origin.** The Worker reaches roll with `X-Roll-CDN-Auth: <ROLL_CDN_ORIGIN_SECRET>`; roll
  refuses CDN requests without the right secret, buckets without the CDN on, and anything but
  GET, HEAD and OPTIONS. A presigned URL the edge verified comes as `X-Roll-CDN-Presigned` (its
  query) and `X-Roll-CDN-Host` and is verified again by the S3 API's own authentication and
  access checks. Mutable responses get `CDN-Cache-Control: public, max-age=<mutable_ttl_s>`
  (60 s; `no-store` if the object's own `Cache-Control` is `private` or `no-store`); the
  object's `Cache-Control` still goes to browsers.
- **Immutable URLs** (`/v/<token>/…`, token = 20 hex digits of SHA-256 of the blob id) only
  serve the object whose current blob the token names, with
  `Cache-Control: public, max-age=31536000, immutable`. Overwriting or deleting the object makes
  the URL 404 at the origin; copies already at the edge go with a tag purge, but browsers keep
  them for a year, so they're for content that never needs unpublishing quickly.
- **Purge on change** ("Smart CDN"). A change to an object of a bucket with the CDN on (a new
  version, a delete or delete marker, an ACL change, lifecycle expiration, a trash restore)
  queues a `cdn_purge` row in the outbox in the same transaction (`events::emit`); so do changes
  that can make a whole bucket more private (its policy, ACL, public access block, ownership,
  public flag, CDN setting, deleting it). The dispatcher sends them to
  `POST /zones/{zone}/purge_cache` in batches (`purge_batch`, 30), at most `purge_per_minute`
  calls, and failed rows retry with the outbox's backoff (they're dropped past its retention).
  `purge: files` purges every mutable URL form of the object (each `s3`, `bucket-key` and
  matching `website` route, public and credentialed namespaces); `purge: tags` purges its
  `rk-<hash>` cache tag, which covers immutable URLs too. A bucket-wide purge sends its
  `rb-<hash>` tag and route prefixes.
- **Usage.** The Worker counts bytes served per bucket and posts them to
  `POST /_roll/cdn/usage` (on the S3 port, with the shared secret) as
  `{"batch": <uuid>, "entries": [{"bucket", "bytes", "requests", "hits"}]}`; a failed batch is
  resent with the same id and counted once.
- **Settings.** Per bucket, `cdn: {enabled, mode}` (`mutable` or `immutable`: which URL the
  control plane hands out). Per deployment, `PATCH /v1/storage/cdn`: `base_url`, `zone_id`,
  `api_token` (sealed, never shown), `purge`, `routes`, `mutable_ttl_s`, `purge_batch`,
  `purge_per_minute`. With the CDN on, a bucket's `public_url` and its objects' URLs are the
  CDN's.

## S3 API (`s3-server`, `s3-proto`)

**Addressing.**
- Path style is always on: `http://host:59000/<bucket>/<key>`.
- Virtual-host style works when `ROLL_S3_DOMAIN` is set (`<bucket>.<domain>`).

**Auth.**
- SigV4: the `Authorization` header and presigned query strings (max 7 days).
- SigV4a (`AWS4-ECDSA-P256-SHA256`): the same canonical request, signed with an ECDSA P-256 key
  derived from the secret (NIST SP 800-108 HMAC-SHA256 in counter mode, retried until the
  candidate is at most n − 2). Its public key is cached per access key. Any
  `x-amz-region-set` is accepted.
- SigV2 (`AWS key:signature`, presigned `AWSAccessKeyId`/`Expires`/`Signature`, POST policy v2):
  HMAC-SHA1 of the documented string to sign, with the bucket in the resource for
  virtual-host requests. Clients differ on the date line when `x-amz-date` is sent and on the
  signed subresources, so each variant they produce is accepted.
- Payloads: `UNSIGNED-PAYLOAD`, a hex SHA-256, `STREAMING-AWS4-HMAC-SHA256-PAYLOAD(-TRAILER)`,
  `STREAMING-AWS4-ECDSA-P256-SHA256-PAYLOAD(-TRAILER)` and `STREAMING-UNSIGNED-PAYLOAD-TRAILER`
  (current SDKs send CRC trailers by default).
- The credential scope's region is accepted as given (any region; R2 does the same). The
  service must be `s3` or `s3express` (S3 Express: SDKs sign directory bucket requests and
  CreateSession with it), or `sts` and `iam` for the STS and IAM query APIs on the same listener
  (a form `POST /` signed for `iam` is IAM's, any other STS's).
- An inactive key (IAM's `UpdateAccessKey`) is `InvalidAccessKeyId`, as an unknown one.
- S3 Express sessions (CreateSession) are a kind of session token: the claims name one
  directory bucket and the mode (`ReadOnly` or `ReadWrite`), and narrow whatever identity
  created them (a key, or another session's role and policies) to that bucket and, for
  `ReadOnly`, to reading. They last 5 minutes, come only in `x-amz-s3session-token`, and no other
  token is accepted there. A directory bucket's policy is evaluated with S3 Express's actions:
  data operations as `s3express:CreateSession` with `s3express:SessionMode`, administration as
  `s3express:<operation>`, on `arn:aws:s3express:<region>:<account>:bucket/<bucket>`
  (`s3-server/src/directory.rs`).
- Temporary credentials (`RT…` keys) carry their session token in `x-amz-security-token`
  (`X-Amz-Security-Token` in presigned URLs, a form field in POST uploads). See below.
- Clock skew is limited to 15 minutes (header-signed SigV2 too); presigned URLs until they
  expire.
- Anonymous requests get what the bucket's policy, ACLs or `public` flag allow.
- `x-amz-expected-bucket-owner` (and `x-amz-source-expected-bucket-owner`) must be the bucket
  owner's 12-digit account id when sent (`AccessDenied` otherwise).

### Accounts and IAM

**Identities** (`s3-server/src/identity.rs`). Every authenticated request resolves its key to an
identity once: the account it acts in, what it is (the account's root user, an account key
with a roll permission level, an IAM user, a role session, a federated user), how resource
policies and ACLs see it (its own ARNs and canonical ids, its role, its account), its condition
keys, and its policies as they are now: a user's inline and managed policies and its groups', a
role's, permissions boundaries, session policies. Policy documents come from the metadata on
every request (a few local SQLite reads for an IAM user; none for account keys), parsed once
per distinct text (a bounded cache), so a change to any policy applies from the next request.

**Evaluation** (`access.rs`) is AWS's: an explicit `Deny` anywhere refuses; the requester's own
side (identity policies, permission level or root, bounded by boundaries and session policies)
and the resource's grants (bucket policy, ACLs, `public`, each saying whether it named the
requester directly, through its role, or through its account) combine with OR in the same
account and AND across accounts. The resource's owner is the bucket's account, or for reads and
ACL changes of an object another account wrote (ACLs on) that account, whose ACL then is the
only grant.

**IAM entities** are JSON documents in `iam` (users with their tags, inline policies, attached
policy ARNs, group names, boundary, login profile and SSH keys; groups; customer-managed policies
with their versions; virtual MFA devices with their seed and last accepted step; OIDC and SAML
providers; server and signing certificates; the password policy, the account alias and the last
credential report, one per account; the throttle counts). The IAM query API
(`s3-server/src/iam/`) checks and changes them inside one metadata transaction
(`Store::iam_write`), so every IAM call is atomic and durable like any write. Users' access keys
are rows of `keys` (`user_name`); roles are rows of `roles` (`iam` holds an IAM role's side).
`iam` is indexed by `(kind, name)` too: aliases are unique across the deployment and sign-in
looks an account up by its alias.

**Principals in policies** (`principals.rs`). PutBucketPolicy, PutAccessPointPolicy, a role's
trust policy (`CreateRole`, `UpdateAssumeRolePolicy`) and the control plane's `PATCH policy`
check every principal: an ARN naming a user, role or account that doesn't exist (or an IAM ARN
that can't be a principal, an OIDC or SAML provider that doesn't exist, a roll key id that
doesn't exist) is `Invalid principal in policy`. The users and roles named are kept with the
policy as ARN → unique id (`bucket_config` kind `policy_principals`, written in the policy's
transaction; `RoleIam::trust_principals` for trust policies). A policy is read back, and
evaluated, as it reads now: a principal whose entity was deleted or replaced by a new one of the
same name reads as the old unique id (which matches nobody), a renamed user as its new ARN,
as on AWS. `CanonicalUser` principals aren't checked.

**Passwords** (`password.rs`, `iam/login.rs`): login profiles keep an argon2id PHC string
(19 MiB, 2 passes, a random salt; hashed on the blocking pool), never the password, and the
last `PasswordReusePrevention` hashes. New passwords are checked against the account's password
policy, or AWS's default one (8 characters, three of four kinds, not the account's name or
email address). Nothing logs or echoes a password or a hash (`LoginProfile`'s `Debug` leaves
the hash out).

**MFA** (`mfa.rs`): TOTP (RFC 6238) with 160-bit seeds. A code is accepted only if its 30-second
step is newer than the device's last accepted one, written durably (`Store::use_mfa_step`)
before the request goes on, so codes can't be replayed. Ten wrong codes in five minutes lock a
device for the rest of them; the counts are rows of `iam` (`s3-store/src/throttle.rs`), so the
lock survives a restart and holds across a takeover. Counting is a relaxed write, and a right
code only reads unless there were wrong ones to forget, so it adds no latency to a successful
check. Sign-in passwords are throttled the same way, per user. MFA Delete is the bucket setting
`mfa_delete` (`bucket_config`), enforced by `PutBucketVersioning` and permanent version deletes
with `x-amz-mfa`.

**Certificates and keys** (`x509.rs`, `iam/certs.rs`): server and signing certificates are
parsed with RustCrypto's `x509-cert` and must be valid now; a server certificate's private key
(PKCS#1, PKCS#8 or SEC1) must match its certificate and isn't kept (IAM never returns it and
nothing in roll uses it). SSH public keys are RSA (2048-16384 bits), `ssh-rsa` or PEM.

**The policy simulator** (`iam/simulate.rs`) builds an `Identity` from the principal's policies
as `identity.rs` collects them for a request (plus `PolicyInputList`; a
`PermissionsBoundaryPolicyInputList` replaces the boundary), asks it for its verdict, evaluates
a `ResourcePolicy` against the caller, and combines them with `access::combine`, the function
requests are decided with. The only condition values are the `ContextEntries` given, as on AWS;
`MatchedStatements` come from the statements that decided (`Policy::matching`), with their
positions in the policy's text.

### Dashboard sign-in

The dashboard had no auth model: the control plane is the operator's, and acts as an account's
root user. IAM users can now sign in to the storage dashboard (`s3-server/src/console.rs`,
`s3-control/src/console.rs`), and then the dashboard acts as them:

- `POST /v1/storage/console/signin` takes the account (its 12-digit id or alias), the user name,
  the password, and a code of the user's MFA device if it has one; a password that must be reset
  (or has expired) is changed as part of signing in. A wrong account, user or password, or a
  user without a login profile, is one answer (and takes as long: a hash is verified either
  way). Ten wrong passwords in five minutes lock the user's sign-in for the rest of them.
- The session is an STS session of the user (`Session::console`, no access key behind it, 12 h,
  `aws:MultiFactorAuthPresent` when MFA was used). The dashboard holds it as a bearer token,
  `<temporary key id>.<session token>`; its secret is never handed out, so it can't sign SigV4
  requests, and other STS sessions aren't accepted as bearer tokens. It ends when the user or
  its login profile goes, or when it expires.
- With `Authorization: Bearer <token>`, every control-plane request acts as the user in its
  account: a guard (`route_layer`) maps each operation to the S3 request it stands for (listing
  objects is `s3:ListBucket` on the bucket, an upload `s3:PutObject` on the key, each field of a
  bucket `PATCH` its `Put…`/`Delete…` action, a prefix delete `s3:DeleteObject` on `<prefix>*`)
  and authorizes it with `access::authorize`, bucket policies and cross-account rules included.
  Operations with no S3 equivalent (accounts, `/keys`, `/roles`, replication targets, session
  keys, every account's usage) are the operator's (`403 OperatorOnly`). `POST /v1/storage/iam`
  runs the IAM API's own dispatch as the user.
- Without a token, the control plane is the operator's, as before: keep its listener private.
  Presigned URLs a signed-in user asks for are signed with the control plane's key, after the
  user's permission is checked; they stay valid for their lifetime even if the user's
  permissions change.

### Temporary credentials (STS)

Browsers, phones and CI shouldn't hold long-lived keys. roll issues short-lived, scoped
credentials that every AWS SDK understands (`AWS_SESSION_TOKEN`, credential providers, web
identity):

- **The STS query API** on the S3 listener (`s3-server/src/sts`): `POST /` (or
  `GET /?Action=…`), form-encoded, `Version=2011-06-15`, answered in STS's XML, errors in STS's
  `<ErrorResponse>`. SDKs reach it with `endpoint_url` for `sts`. Calls are SigV4-signed with
  service `sts` (the payload is the body; STS clients don't send `x-amz-content-sha256`),
  except `AssumeRoleWithWebIdentity` and `AssumeRoleWithSAML`, which are unsigned. Bodies are
  read whole, at most 1 MiB (a 100 000-character SAML assertion, IdP metadata).
  - `GetSessionToken` (900 s–36 h, default 12 h; the root user's 900 s–1 h): a session of the
    calling permanent key, with MFA (`SerialNumber`, `TokenCode` of the caller's own device).
  - `AssumeRole` (900 s up to the role's maximum, default 1 h; a role session assuming a role:
    at most 1 h): a session of a role, in the role's account, for a caller its trust allows: an
    IAM trust policy (evaluated with the caller as principal, `sts:AssumeRole` as action, and
    conditions such as `sts:ExternalId` and `aws:MultiFactorAuthPresent`), or a control-plane
    trust naming the caller's permanent key (`ExternalId` checked in constant time). Another
    account's role, or a trust that names only the caller's account, needs the caller's own
    policies to allow `sts:AssumeRole` on the role. Session policies: `Policy` (2 KB) and
    `PolicyArns` (up to 10 managed policies, kept as ARNs and read when a request is checked).
    Session tags and transitive tag keys (at most 50, 2 KB together; `sts:TagSession` in a trust
    policy), `SourceIdentity`; chaining carries transitive tags and the source identity over.
  - `AssumeRoleWithWebIdentity`: an OIDC ID token of a provider in the role's trust (below):
    an IAM OIDC provider named by a trust policy's `Federated` principal (its URL is the issuer,
    its client ids the audiences), or a control-plane role's issuer list.
  - `GetFederationToken` (900 s–36 h; root 1 h): a federated user acting with the caller's
    policies bounded by the session policies (none: no permissions of its own).
  - `GetCallerIdentity`.
  - `AssumeRoleWithSAML`: a SAML response signed by an IAM SAML provider's IdP (below), whose
    `Role` attribute pairs the role with the provider, for a role whose trust policy allows the
    provider (`Federated`) with `sts:AssumeRoleWithSAML`.
- **Roles** (`roles` table, per account): control-plane roles have a trust (permanent keys;
  OIDC providers with issuer, audiences, an optional subject pattern and `jwks_uri`; an optional
  external id), a permission and bucket scope like a key's, an optional inline IAM policy that
  bounds their sessions (at most 10 KB); IAM roles (`CreateRole`) have a trust policy, inline and
  managed policies, a boundary and tags (`iam`). Both have a maximum session duration
  (15 min–12 h). `RoleArn` is `arn:aws:iam::<account>:role<path><name>`; a session's ARN is
  `arn:aws:sts::<account>:assumed-role/<name>/<session>` and its `aws:userid` is
  `<role id>:<session>`.
- **The control plane mints too**: `POST /v1/storage/roles/{name}/sessions` hands a trusted
  backend a role's credentials, optionally narrower (permission, buckets, session policy, tags,
  60 s up to the role's maximum), for it to pass to a browser. No STS client needed.

**Session tokens are self-contained** (`s3-store/src/sts.rs`). Nothing is written per session,
so minting is cheap at any scale and checking a request needs no session lookup:

```text
token = base64url( version (1) | key id (4) | salt (16) | nonce (12) | AES-256-GCM(claims) + tag (16) )
AES key = HMAC-SHA256(deployment key, "roll sts session token v1\0" | salt)
associated data = version | key id | salt | nonce
claims = JSON { access key id, secret, issued/expires (ms), key, role { name, id, generation },
                session name, permission, buckets, session policy, tags, transitive keys,
                source identity, web identity { issuer, subject, audience },
                account, user, federated user, MFA time, policy ARNs }
```

- The **deployment key** (32 random bytes) is created on first use and kept in the metadata
  (`state`, `sts_session_keys`), so it's as durable as the rest of the metadata and every node
  of a deployment shares it. `POST /v1/storage/session-keys/rotate` (`roll storage sessions
  rotate-key`) makes a new current key; tokens of the previous key keep working and older ones
  stop. Rotating at most once per 36 h never cuts a live session short.
- **One AES key per token.** Each token's key is derived from a random 128-bit salt, so random
  96-bit nonces never meet the GCM birthday bound however many tokens are minted.
- **Authenticated, not just encrypted.** Any change to any byte (header included) fails the
  tag check: `InvalidToken`. The claims name their access key id, so a token only works with
  its own key (compared in constant time); the secret inside it is what the request must be
  signed with.
- **Checks on every request:** the token's size (at most 12 KiB, before decoding), the tag,
  the key id, expiry (`ExpiredToken`, by the store's clock, before the request's time is
  checked). Then the session's role (or key) is read as it is now (a local SQLite read, no
  write): a role session acts with the role's *current* permission level, policies and boundary
  (never more than its permission and scope were narrowed to when minted); a `GetSessionToken`
  session with its key's current permission. A session whose role was deleted or replaced (`id`),
  whose key is gone or inactive, or issued at or before `revoked_before`, authenticates and is
  denied everything (`AccessDenied`, as on AWS). Revoking adds AWS's `AWSRevokeOlderSessions`
  inline policy too.
- **Errors as on AWS:** a temporary key without its token is `InvalidAccessKeyId`; a permanent
  key with a token, or a malformed, tampered or retired token, is `InvalidToken`.
- **What a session may do:** its role's identity policies (and permission level), bounded by
  the role's boundary (and a control-plane role's policy) and the session policies (inline and
  `PolicyArns`); a resource policy or ACL naming the session itself grants beyond them, one
  naming the role doesn't, as on AWS (see [Accounts and IAM](#accounts-and-iam)).
- **Principals and condition keys:** a role session is `Principal: {"AWS": <role ARN>}` or its
  assumed-role ARN in bucket policies. It sets `aws:PrincipalArn` (the role ARN, as on AWS),
  `aws:userid`, `aws:PrincipalType` (`AssumedRole`), `aws:PrincipalAccount` (the role's
  account), `aws:PrincipalTag/<key>`, `aws:TokenIssueTime`, `aws:SourceIdentity`,
  `aws:MultiFactorAuthPresent` and `aws:MultiFactorAuthAge`, `aws:FederatedProvider` and
  `<provider>:sub`/`<provider>:aud` for web identities. A `GetSessionToken` session is its key.
- **Never logged:** secrets, tokens and MFA seeds stay out of logs and `Debug` output; server
  access logs show `X-Amz-Security-Token=REDACTED`.
- **Billing doesn't change:** requests are metered to buckets as usual (to the requester on
  Requester Pays buckets).

**Web identity** (`s3-server/src/sts/oidc.rs`). Only issuers in the role's trust are ever
fetched from, so a token can't make roll connect anywhere. The provider's JWKS (`jwks_uri`, or
the issuer's `/.well-known/openid-configuration`) is fetched over HTTPS only, without
redirects, with a 5 s timeout and size caps (64 KiB discovery, 256 KiB JWKS, 100 keys), cached
for an hour, refetched early at most every 30 s when a token names an unknown key id, and kept
for a day if the provider is down. Tokens (at most 20 000 bytes) must be RS256 or ES256 (no
`none`, no HMAC, no `crit`), and `iss` (exact), `aud` (one of the provider's audiences),
`exp` and `nbf` (60 s leeway) and the subject pattern are checked. Session tags come from the
`https://aws.amazon.com/tags` claim and `SourceIdentity` from
`https://aws.amazon.com/source_identity`, as on AWS. The fetch is behind
`s3_server::JwksFetch` (`Server::set_jwks_fetcher`) so tests serve their own keys;
`Config::oidc_allow_http` allows plain HTTP for tests only.

**SAML** (`s3-server/src/sts/saml.rs`, `s3-proto/src/xmldsig.rs`). A provider's metadata
(`CreateSAMLProvider`) gives its `entityID` and RSA signing certificates (`KeyDescriptor`s
without `use`, or `use="signing"`). `AssumeRoleWithSAML` never fetches anything. The response is
hostile input, so:

- It's parsed by `s3_proto::xmldsig` on `quick-xml`: no DTDs or entities (only the predefined
  ones and character references), no processing instructions, 1 MiB, depth 64, 20 000 nodes;
  duplicate attributes (by expanded name) and duplicate `ID`s are errors. An element's text is
  all its text children together, so a comment can't cut a value short.
- Exactly one `Assertion`, a direct child of the `Response`; `EncryptedAssertion` is refused.
- Every `ds:Signature` must be a direct child of the `Response` or the `Assertion` and verify;
  one must cover the assertion. A signature verifies when `SignedInfo` uses exclusive
  canonicalization (`xml-exc-c14n#`, without comments), RSA-SHA256 or RSA-SHA1, and exactly one
  `Reference`, whose `URI` is `#` and the `ID` of the signature's own parent, which must be the
  element that `ID` names (so the node the assertion is read from is the node that was
  signed: no wrapping), whose transforms are exactly enveloped-signature then exclusive
  canonicalization (`InclusiveNamespaces` honoured), whose SHA-256 (or SHA-1) digest of the
  canonical parent without the signature matches (constant-time), and whose canonical
  `SignedInfo` verifies (`ring`, 2048-bit keys or more) with a certificate of the provider's
  metadata. `KeyInfo` in the response is ignored.
- Exclusive canonicalization is implemented in `xmldsig.rs` (namespaces rendered where visibly
  used and not already in effect from an output ancestor, `xmlns=""` only to undo a rendered
  default, attributes by namespace URI and local name, the recommendation's escaping); a unit
  test checks it byte for byte against a response signed by signxml on lxml.
- The assertion: its `Issuer` is the provider's `entityID` (and the response's, if it has one);
  a bearer `SubjectConfirmation` whose `Recipient` is `https://signin.aws.amazon.com/saml`, the
  region's sign-in endpoint, or one of `ROLL_S3_SAML_AUDIENCES` (`Config::saml_audiences`), within its `NotOnOrAfter`;
  `Conditions` `NotBefore`/`NotOnOrAfter` (5 minutes of skew either way); every
  `AudienceRestriction` naming an accepted audience (those endpoints or
  `urn:amazon:webservices`); the response's `Destination`, if any, an accepted endpoint; a
  `RoleSessionName`. `SessionNotOnOrAfter` caps the session.
- The trust policy decides with the provider's ARN as the `Federated` principal and `saml:aud`
  (the recipient), `saml:iss`, `saml:sub`, `saml:sub_type`, `saml:namequalifier`
  (Base64(SHA-1(issuer + account + "/" + provider))), the eduPerson and X.500 attribute keys
  AWS maps, `aws:FederatedProvider`, and the session tags (`PrincipalTag:*` attributes; they
  need `sts:TagSession`). The session's claims keep the `saml:*` values for its own requests'
  policies (`${saml:sub}`).

### Server-side encryption

- **The backing object store does the crypto.** The `Backend` trait takes the SSE parameters
  on every call that writes or reads a blob's bytes (`BlobAttrs::sse` for `put`,
  `create_multipart` and `copy`; an argument for `get_blob`, `upload_part`, `upload_part_copy`
  and `complete_multipart`; a `CopySource` carries the source's). `S3Backend` puts SSE-C
  (customer key and key MD5) and SSE-KMS (key id, context, bucket key) on the request, so
  durability and integrity are exactly S3's, and checksums stay those of the plaintext.
- **Never silently unencrypted.** Every encrypted write checks that the service confirmed the
  encryption in its response. An S3-compatible service that ignores the headers would store
  plaintext, so what it stored is deleted (or the upload aborted) and the request fails with
  `NotImplemented`.
- **Keys are never stored.** The metadata records how each blob is encrypted (`blob_sse`), with
  only the MD5 of an SSE-C key. The gateway checks a request's key against it before any bytes
  move (missing: `InvalidRequest`, wrong: `AccessDenied`); the store checks again.
- **SSE-KMS** is on for AWS S3 without a custom endpoint and off otherwise (`ROLL_S3_KMS`).
- **The local directory backend** (dev and tests) encrypts SSE-C blobs itself: AES-256-GCM in
  64 KiB chunks with per-segment random salts and keys derived from the customer key, chunk
  indexes as nonces and the index, last-chunk flag and length as associated data. A sidecar
  (`.s3-sse/<key>.json`, written before the blob) holds the salts, lengths and a key check, so
  ranges decrypt only the chunks they touch and listings report plaintext sizes. Each multipart
  part is its own segment, so completing an upload needs no re-encryption. SSE-KMS is refused.
- **GC, the orphan scan and the scrub** need no keys: deleting and listing work on any blob, and
  listings report plaintext sizes on S3 (checked against SeaweedFS too), so the scrub's size
  comparison holds for encrypted blobs.

**Permissions.** Account keys carry a roll permission level, which acts as an identity policy
attached to the key (IAM users and roles have IAM policies instead; see
[Accounts and IAM](#accounts-and-iam)):
- `read`: GET, HEAD, List*, reading bucket configuration
- `write`: read, plus PUT, DELETE, copy and multipart
- `admin`: write, plus create/delete bucket and bucket configuration; over every bucket, the
  account's root user

A key with a `buckets` list only sees those buckets (ListBuckets is filtered, and other buckets
answer `AccessDenied`, existing or not), unless a bucket's policy or ACL grants it more.

**Authorization** (`s3-server/src/access.rs`; every operation maps to its IAM action name):
1. an explicit `Deny` refuses, whoever asks: the bucket policy, the requester's identity
   policies, its boundary or session policies (a revoked or orphaned session is denied
   everything);
2. the requester's own side: its identity policies, its permission level, or being the root
   user, bounded by its boundary and session policies;
3. the resource's grants: a bucket policy `Allow` (anonymous requests too, unless
   `RestrictPublicBuckets` and the policy is public, which then only counts for the owner's
   account), ACL grants when Object Ownership enables ACLs (`IgnorePublicAcls` drops public
   ones), the `public` flag for `GetObject`;
4. the same account allows with 2 or 3 (a grant naming only the account needs 2; one naming the
   role is bounded like 2); another account needs both;
5. Requester Pays buckets refuse other accounts without `x-amz-request-payer: requester`.

The bucket owner's root user (and its admin keys covering the bucket) can always manage the
bucket's policy. Policies are parsed in `policy.rs`, ACLs in `acl.rs`; see
[compatibility](compatibility.md#policies-acls-and-public-access) for principals, condition keys
and the canonical ids. `ROLL_S3_BLOCK_PUBLIC_ACCESS` (store config `block_public_access`)
applies all four Block Public Access settings to every bucket.

**Operations.**
- **Service:** ListBuckets.
- **Bucket:**
  - CreateBucket, DeleteBucket (must be empty), HeadBucket and GetBucketLocation
  - Get/Put/DeleteBucketCors
  - Get/Put/DeleteBucketLifecycleConfiguration (the full rule model; transitions stored only)
  - Get/PutBucketVersioning, ListObjectVersions, Get/PutObjectLockConfiguration
  - ListObjectsV2, ListObjects (v1), ListMultipartUploads
  - DeleteObjects (batch, quiet mode, version ids; each key authorized on its own)
  - configuration (`s3-server/src/config.rs`, stored in `bucket_config`): tagging, policy and
    policy status, public access block, ownership controls, ACL, encryption, website, logging,
    accelerate, request payment, and the analytics, metrics, inventory and intelligent-tiering
    configurations with List*
  - Get/PutBucketNotificationConfiguration (`notification.rs`, `notify.rs`) and
    Get/Put/DeleteBucketReplication (`replication.rs`), see [Events](#events-notifications-replication-inventory)
    `config::default_encryption(ctx, &bucket) -> Option<(algorithm, kms_key_id, bucket_key_enabled)>`
    is the bucket's configured default encryption rule (`None`: the default SSE-S3), for SSE-KMS.
- **Website endpoint** (`website.rs`): `<bucket>.<website_domain>` (`ROLL_S3_WEBSITE_DOMAIN`)
  serves S3 website semantics: index and error documents, routing rules, redirects.
- **Access points** (`accesspoint/`): the S3 Control API (`control.rs`; `/v20180820/…` with
  `x-amz-account-id`, on any host, the account the store's `account_id()`), and requests through
  access points (`mod.rs`): right after addressing, a request naming an access point (its
  `<name>-<account>` host label, the `/arn:…` path, or its alias as the bucket) is pointed at the
  access point's bucket and keeps the access point (`Ctx::access_point`). `access.rs` hands its
  authorization to `accesspoint::authorize`: a `Deny` in either policy refuses, the key's own
  permission allows, otherwise both the access point's policy (resources
  `arn:aws:s3:<region>:<account>:accesspoint/<name>[/object/<key>]`) and the bucket's must allow;
  `s3:DataAccessPointArn`, `s3:DataAccessPointAccount` and `s3:AccessPointNetworkOrigin` are
  condition keys. Only object and listing operations go through access points. `VPC` access
  points take requests from internal peers only. A copy's source (`x-amz-copy-source`, for
  CopyObject and UploadPartCopy) can name an access point by its object ARN or alias
  (`accesspoint::copy_source`): it's read from the access point's bucket, authorized through that
  access point (`authorize_with`, with its condition keys in `Cond::ap`) whatever the
  destination is addressed by.
- **Object Lambda** (`accesspoint/lambda.rs`): an Object Lambda access point's `GetObject`,
  `HeadObject` and `ListObjects(V2)` run its function (AWS Lambda through the notifier's AWS
  path, or a signed webhook) with S3's event and an `inputS3Url` presigned with the caller's
  credentials through the supporting access point (`Config::public_endpoint`). A `GetObject`
  registers a route with a random 256-bit token (in memory, one use, removed when the caller
  stops waiting, 60 s at most) and waits for `WriteGetObjectResponse` on it, whose body is piped
  to the caller through a bounded channel; `HeadObject` and listings use the function's result.
  Everything else passes through to the supporting access point.
- **Server access logs** (`logging.rs`): lines are buffered in memory (at most 100 000) and
  written as objects to the target bucket every `access_log_interval` (5 min) through the
  normal put path, so they're durable once written and billed to the target. Best effort.
- **Object:**
  - PutObject, GetObject and HeadObject: `versionId`, `Range`, `partNumber`, conditional
    headers, `response-*` overrides, checksum mode and Object Lock headers
  - DeleteObject (with `versionId`)
  - CopyObject: `x-amz-metadata-directive`, copy-source conditionals and source `versionId`
  - Get/Put/DeleteObjectTagging (with `versionId`), Get/PutObjectAcl, RestoreObject
    (`InvalidObjectState`)
  - Get/PutObjectRetention, Get/PutObjectLegalHold
  - GetObjectAttributes (`versionId`, conditional headers, paging over parts with per-part
    checksums and sizes)
  - RenameObject (all buckets; see [Write paths](#write-paths)), appends (directory buckets)
  - GetObjectTorrent (`torrent.rs`): bencoded metainfo, pieces cached by blob
  - SelectObjectContent (`select.rs` over `s3-select`): the request and SQL are checked before
    the response starts, then the object (its `ScanRange`) streams through the query and the
    results go out as AWS event-stream messages (`s3_proto::eventstream`), at most 256 KiB each
    and 8 queued, so a slow client slows the read instead of filling memory
- **Directory buckets** (`directory.rs`): CreateBucket with a directory configuration,
  ListDirectoryBuckets, CreateSession; what S3 Express doesn't have answers `NotImplemented`.
- **Multipart:** CreateMultipartUpload, UploadPart, UploadPartCopy,
  CompleteMultipartUpload, AbortMultipartUpload, ListParts.
- **Browser uploads:** POST Object (presigned POST policy).
- **Everything else** returns `NotImplemented` (501) with the S3 XML error body.

**Limits (as S3):**
- keys ≤ 1024 bytes
- user metadata ≤ 2 KB
- single PUT ≤ 5 GiB
- object ≤ 5 TiB
- parts 1–10 000, ≥ 5 MiB except the last
- ≤ 1000 keys per list page and per DeleteObjects

Peer-sent lengths never size an allocation: bodies are streamed, and XML bodies are capped
(1 MiB).

**Consistency.** Strong read-after-write and list-after-write, as on S3 today: all metadata
goes through one SQLite database.

## Developer API (`s3-api`)

A Supabase-Storage-compatible REST API (`/storage/v1`) on its own listener (`--storage-api`,
default `127.0.0.1:59001`), over the same `Store` and `Server` as the S3 API. The reference,
the rules, the migration guide and the differences from Supabase are in [api.md](api.md).

- **Callers** (`s3-server/src/app.rs`): `apikey` and `Authorization: Bearer`.
  - The service key is the account's root user.
  - The anon key and user tokens are `Kind::App` identities with no identity policies, so in their
    account only resource grants (the bucket policy, the `public` flag) let them in.
  - Anonymous requests are the S3 API's anonymous requester.
- **Keys** (`s3-store/src/api.rs`): the anon and service keys are HS256 JWTs (`iss: roll`, `ref`,
  `role`, `gen`).
  - They're signed with a key derived by HMAC-SHA256 from the deployment's API key, which is 32
    random bytes kept in `state.storage_api_key`. Signed URLs use another derived key.
  - The keys and the project's JWT secret are AES-256-GCM-sealed (the session tokens' scheme)
    under a third derived key in `api_projects`, so they're shown again but never stored in the
    clear.
  - Rotation bumps the project's generation, which the keys carry.
  - User tokens are HS256 with the project's secret (optional `iss`/`aud`), or RS256/ES256 from
    its OIDC issuers through the same JWKS verifier as `AssumeRoleWithWebIdentity`.
- **Authorization** goes through the S3 API's one evaluator (`access::authorize_as`), with S3
  actions (`s3:GetObject`, `s3:PutObject`, `s3:DeleteObject`, `s3:ListBucket`).
  - Bucket policies name callers with `{"App": "<role>"}` principals (`Requester::app_role`).
  - The JWT's claims are `jwt:*` condition keys and policy variables (`Identity::condition`).
  - `roll:ObjectOwner` is the current object's `owner`, read on first use (`access.rs`).
  - A `Checker` caches each bucket's access rules for one request, so filtering a listing costs
    one policy parse.
  - Rule presets are bucket policies generated by `app::preset_policy`, and set through
    `app::put_bucket_policy` with `PutBucketPolicy`'s checks.
- **Writes** take the S3 API's write path: `s3_server::put_object` and the store's multipart
  uploads. Moves within a bucket are `rename_object`, and copies share the blob.
- **Bucket limits** (`file_size_limit`, `allowed_mime_types`) are a `bucket_config` row of kind
  `limits`.
  - `versioning::put_key` checks them in the transaction that writes any new version, so every
    write path honours them, the S3 API's included.
  - The handlers check them early too, before any bytes move.
  - A refused write is `StoreError::Refused` with the code `EntityTooLarge` or `InvalidMimeType`.
    The S3 API answers `EntityTooLarge` or `InvalidRequest`.
- **Owner.** `ObjectHeaders::owner` is the writer's `sub`, a row column (migration 12), so branch
  push-down, versions, the trash and uploads carry it.
- **Resumable uploads (TUS).** Each one is a multipart upload plus an `api_uploads` row with the
  same id.
  - The row is written with the upload and removed in the transaction that completes or aborts
    it.
  - Bytes are buffered on local disk in `<spool>/tus/<id>.<part number>` until a part (at least
    5 MiB) is full. The part is then uploaded and recorded, and its buffer deleted.
  - The offset is the recorded parts plus the buffer. After a restart an upload resumes from
    there, and on another node from its last recorded part.
  - Uploads expire after 24 h, and a sweeper aborts them.
  - An upload completes into one blob like any multipart upload.
- **Metering** is the S3 API's `Meter`:
  - class A: uploads, lists, copies and bucket writes
  - class B: reads and signing
  - free: deletes
  - plus bytes in and egress (internal or internet by the peer)

## Control-plane API (`s3-control`, `/v1/storage`)

All JSON. Errors are `{"error": "..."}` with 400/404/409/500. Times are Unix milliseconds.

**Accounts.** Every endpoint acts in one account: the one named by the `x-roll-account: <account
id>` header or the `?account=<account id>` query parameter, else the **default account** (the
deployment's original owner, so every existing caller keeps working unchanged). An unknown
account is a 404. Buckets, keys, roles and usage are the account's: another account's bucket is
as missing as one that doesn't exist (404), its keys and roles too.

```text
GET    /v1/storage
  → { endpoint, region, virtual_host_domain|null, website_domain|null, storage, storage_class, recover_days_default,
      owner_id /* the account's canonical id */, account_id /* its 12-digit id, for ARNs */, account: Account,
      default_account_id, block_public_access,
      pricing: Pricing, buckets, objects, bytes /* the account's */,
      deployment: { accounts, buckets, objects, bytes }, scrub: { last_at|null, missing, mismatched } }

GET    /v1/storage/accounts                      → [Account]
POST   /v1/storage/accounts  { name, email?, root_key?: bool (default true) }
  → Account & { root_key?: Key & { secret_access_key } /* the root user's key: admin over every bucket */ }
GET    /v1/storage/accounts/{id}                 → Account
PATCH  /v1/storage/accounts/{id}  { name?, email?: string | null } → Account
DELETE /v1/storage/accounts/{id}                 → { deleted }   (409 while it owns buckets; deletes its keys, roles and IAM entities;
                                                                   the default account can't be deleted)

POST   /v1/storage/iam  { Action, …params }     → the action's result as JSON   (the IAM query API: see Accounts and IAM)
       params: AWS's names; arrays become Name.member.N, objects Name.Field, nested. The result: lists are arrays,
       maps (SummaryMap) objects, leaves strings ("true", "3600"), empty elements null; PolicyDocument,
       AssumeRolePolicyDocument and Document come back URL-decoded. Errors: IAM's status and { error, code }.
       Without a session it acts as the account's root user; with one, as the signed-in IAM user.
POST   /v1/storage/console/signin  { account /* id or alias */, user, password, mfa_code?, new_password? }
  → Session & { token }   (401 InvalidCredentials | MfaRequired | InvalidMfaCode | PasswordResetRequired | PasswordExpired,
                            400 PasswordPolicyViolation, 429 Throttled)
GET    /v1/storage/console/session              → Session   (with Authorization: Bearer <token>; 401 otherwise)
Session = { expires_at, account_id, account_alias: string | null, user, arn, mfa: boolean }
  With Authorization: Bearer <token>, every /v1/storage request acts as that IAM user (see Dashboard sign-in):
  403 { code: "AccessDenied" } when its policies don't allow the S3 action an operation stands for,
  403 { code: "OperatorOnly" } for the operator's operations, 401 { code: "InvalidToken" | "ExpiredToken" }.

GET    /v1/storage/buckets                       → [Bucket]
POST   /v1/storage/buckets  { name, public?, recover_days?, object_lock?: bool }   → Bucket
GET    /v1/storage/buckets/{name}                → Bucket
PATCH  /v1/storage/buckets/{name}  { public?, recover_days?, cors?: [CorsRule], lifecycle?: [LifecycleRule],
                                     tags?: Record<string,string> /* replaces them; {} removes */,
                                     versioning?: "Enabled" | "Suspended",
                                     notifications?: { configurations?: [NotificationConfig] /* replaces them; [] removes;
                                                                                                 omitted: unchanged */,
                                                       event_bridge?: boolean, skip_destination_validation?: boolean,
                                                       rotate_webhook_secret?: boolean },
                                     replication?: { rules: [ReplicationRuleInput] } | null /* replaces them; null or [] removes */,
                                     website?: { index_document, error_document?: string | null } | null /* null removes */,
                                     policy?: IamPolicy (with Principal) | string | null /* the bucket policy; null removes */,
                                     public_access_block?: PublicAccessBlock | null /* the bucket's own; null removes */,
                                     file_size_limit?: number | string /* "20MB" */ | null,
                                     allowed_mime_types?: [string] /* "image/*" wildcards */ | null /* developer API limits; null removes */ } → Bucket
DELETE /v1/storage/buckets/{name}?force=true     → { deleted }   (force: delete all objects first)
POST   /v1/storage/buckets/{name}/branches  { name }  → Bucket

GET    /v1/storage/buckets/{name}/objects?prefix=&delimiter=/&cursor=&limit=200
  → { objects: [ObjectSummary], prefixes: [string], next_cursor: string|null }
GET    /v1/storage/buckets/{name}/versions?prefix=&cursor=&limit=200
  → { versions: [ObjectVersion], next_cursor: string|null }
GET    /v1/storage/buckets/{name}/object?key=&version_id=    → ObjectInfo
DELETE /v1/storage/buckets/{name}/object?key=&version_id=    → { deleted: key, version_id?, delete_marker? }
POST   /v1/storage/buckets/{name}/delete  { keys?: [string], prefix?: string } → { deleted: n }
GET    /v1/storage/buckets/{name}/download?key=&version_id=  → the bytes (Content-Disposition: attachment)
PUT    /v1/storage/buckets/{name}/upload?key=    (raw body, Content-Type) → ObjectInfo
POST   /v1/storage/buckets/{name}/presign  { key, method: "GET"|"PUT", expires_s? (default 3600) }
  → { url, method, expires_at }
POST   /v1/storage/buckets/{name}/url  { key, version_id?, immutable?, expires_s? }
  → { url, cdn: bool, immutable: bool, expires_at|null }   (the CDN's URL when the bucket has
     the CDN on, immutable by the bucket's mode; presigned unless the bucket is public)

GET    /v1/storage/cdn   → { base_url, zone_id, api_token_set, purge, routes, mutable_ttl_s,
                             purge_batch, purge_per_minute, worker, margin, stats, cache }
PATCH  /v1/storage/cdn   { base_url?, zone_id?, api_token?, purge?: "files"|"tags", routes?,
                           mutable_ttl_s?, purge_batch?, purge_per_minute? }
GET    /v1/storage/cdn/worker?secrets=   → { vars: {ROLL_ORIGIN, ROLL_ROUTES, …}, secrets: [names] }

GET    /v1/storage/buckets/{name}/trash?prefix=&cursor=&limit=
  → { objects: [TrashEntry], next_cursor }
POST   /v1/storage/buckets/{name}/trash/restore  { key, deleted_at } → ObjectInfo

GET    /v1/storage/api                          → { account_id, endpoint, anon_key, service_key, key_generation, keys_issued_at,
                                                     auth: { jwt_secret_set, jwt_issuer, jwt_audience, oidc }, presets }
POST   /v1/storage/api/keys/rotate              → the same (new keys; the old ones stop working at once)
PUT    /v1/storage/api/auth  { jwt_secret?, jwt_issuer?, jwt_audience?, oidc? }  → the same (omitted: unchanged, null: removed)
GET    /v1/storage/buckets/{name}/rules/{preset}  → { preset, policy }
POST   /v1/storage/buckets/{name}/rules  { preset }  → Bucket   (sets the preset's bucket policy)
       the developer API's project, auth providers and rule presets: see api.md

GET    /v1/storage/buckets/{name}/usage?days=30  → Usage
GET    /v1/storage/usage?days=30&accounts=false  → { days, account, cost: Cost /* the account's bill */,
                                                     daily: [same rows as Usage.daily, summed], buckets: [Usage],
                                                     as_requester?: RequesterCharges /* it paid on others' Requester Pays buckets */,
                                                     billed_to_requesters?: RequesterCharges /* requesters paid on its buckets */,
                                                     accounts?: [{ account_id, name, cost, as_requester, billed_to_requesters }] /* accounts=true */ }

GET    /v1/storage/keys                          → [Key]
POST   /v1/storage/keys  { name, permission, buckets?: [string] } → Key & { secret_access_key }
DELETE /v1/storage/keys/{id}                     → { deleted }

GET    /v1/storage/roles                         → [Role]
POST   /v1/storage/roles  { name, permission, description?, buckets?: [string], policy?: IamPolicy,
                            trust?: Trust, max_session_s? (900–43200, default 3600) } → Role
GET    /v1/storage/roles/{name}                  → Role
PATCH  /v1/storage/roles/{name}  { any of: description, permission, buckets /* null: all */,
                                   policy /* null: none */, trust, max_session_s } → Role
                                 /* sessions act with the new settings from the next request on */
DELETE /v1/storage/roles/{name}                  → { deleted }   (its sessions are denied everything)
POST   /v1/storage/roles/{name}/revoke           → Role   (AWSRevokeOlderSessions: sessions issued until now are denied)
POST   /v1/storage/roles/{name}/sessions  { session_name?, duration_s? (60 to max_session_s, default 3600),
                                            permission? /* ≤ the role's */, buckets? /* within the role's */,
                                            policy?: IamPolicy, tags?: Record<string,string>,
                                            transitive_tag_keys?: [string], source_identity? } → Session
POST   /v1/storage/session-keys/rotate           → { key_id }

GET    /v1/storage/replication-targets           → [Target]
POST   /v1/storage/replication-targets  { name, endpoint? /* none: AWS S3 */, region? (us-east-1),
                                          access_key_id?, secret_access_key? /* with the key; write-only */,
                                          path_style? (true), buckets?: [string] } → Target
GET    /v1/storage/replication-targets/{name}    → Target
PUT    /v1/storage/replication-targets/{name}  { same, without name; no secret_access_key keeps it } → Target
DELETE /v1/storage/replication-targets/{name}    → { deleted }   (409 while a bucket's replication uses it)
```

```ts
Bucket = { id, name, region, public, recover_days, created_at,
           parent: { id, name, at } | null, branches: [string],
           objects, bytes, trash_bytes, cors: CorsRule[], lifecycle: LifecycleRule[],
           tags: Record<string,string>, website: Website | null,
           versioning: "Enabled" | "Suspended" | null,
           object_lock: { mode: "GOVERNANCE" | "COMPLIANCE" | null, days: number | null, years: number | null } | null,
           versions /* own noncurrent versions, delete markers included */, version_bytes,
           notifications: { configurations: [NotificationConfig], event_bridge: boolean,
                            webhook_secret: string | null /* signs arn:roll:webhook deliveries */,
                            backlog: Record<destination, { pending, last_error: string | null }> /* undelivered only */ },
           replication: { rules: [ReplicationRule] } | null,
           policy: IamPolicy | null, policy_public: boolean,
           public_access_block: PublicAccessBlock | null /* the bucket's own; the deployment's block applies on top */,
           endpoint, url /* path-style */, virtual_host_url: string | null,
           website_url: string | null /* with a website domain and a website configuration */ }
ReplicationRule = { id, priority: number | null, enabled, prefix /* "" = every key */, tags: Record<string,string>,
                    delete_markers, existing_objects, destination /* a bucket */, target: string | null /* its replication target; null: a roll bucket */,
                    storage_class: string | null,
                    owner_override, pending /* versions waiting for this destination, at most 1000 counted */,
                    last_error: string | null, backfilling /* existing objects still being queued */,
                    metrics: { enabled /* Metrics (or RTC) on */, replication_time_control, operations_pending, bytes_pending,
                               oldest_pending_ms: number | null, latency_ms: number | null, max_latency_ms: number | null,
                               replicated, operations_failed, replicated_after_threshold } /* S3's replication metrics, since start */ }
ReplicationRuleInput = { destination, target? /* default: the target listing the bucket, else a roll bucket */, id?, priority?,
                         enabled? (default true), prefix?, tags?, delete_markers?, existing_objects?, storage_class?, owner_override? }
Target = { name, endpoint: string | null, region, access_key_id: string | null,
           credentials: "access_key" | "deployment", path_style, buckets: [string], created_at, updated_at }
PublicAccessBlock = { block_public_acls, ignore_public_acls, block_public_policy, restrict_public_buckets }
NotificationConfig = { id /* generated when empty */, type: "queue" | "topic" | "function" /* from the ARN when omitted */,
                       destination /* arn:roll:queues:<vhost>:<queue> | arn:roll:streams:<stream>:<topic> |
                                      arn:roll:webhook:<url> | arn:aws:sqs|sns|lambda:… */,
                       events: string[] /* s3:ObjectCreated:*, … */, prefix: string | null, suffix: string | null }
Website = { index_document: string | null, error_document: string | null,
            redirect_all: { host_name, protocol: "http"|"https"|null } | null,
            routing_rules: [{ condition: { key_prefix_equals: string|null, http_error_code_returned_equals: number|null } | null,
                              redirect: { host_name, protocol, http_redirect_code, replace_key_prefix_with, replace_key_with /* each string|number|null */ } }] }
CorsRule = { id?: string | null /* S3's optional rule ID */, allowed_origins: string[],
             allowed_methods: string[], allowed_headers: string[], expose_headers: string[],
             max_age_s: number | null }
LifecycleRule = { id, prefix, enabled, expire_days: number | null, abort_multipart_days: number | null,
                  // all optional on input:
                  tags?: Record<string,string>, size_greater_than?: number | null, size_less_than?: number | null,
                  expire_at?: number | null /* ms, midnight UTC */, expire_delete_markers?: boolean,
                  noncurrent_days?: number | null, newer_noncurrent_versions?: number | null,
                  transitions?: [{ days: number | null, date: number | null, storage_class }],
                  noncurrent_transitions?: [{ noncurrent_days, newer_noncurrent_versions: number | null, storage_class }],
                  legacy_prefix?: boolean /* shown as S3's <Rule><Prefix> */ }
ObjectSummary = { key, size, etag, content_type, last_modified }
ObjectInfo = ObjectSummary & { metadata: Record<string,string>, tags: Record<string,string>,
             cache_control, content_disposition, content_encoding, content_language, expires,
             checksum: { algorithm, type, value } | null, parts,
             version_id /* "null" outside versioned buckets */,
             lock: { mode, retain_until, legal_hold } | null,
             replication_status?: "PENDING" | "COMPLETED" | "FAILED" | "REPLICA" /* object?key= only, when it has one */,
             replicas?: [{ bucket, target, version_id: string | null, replicated_at }] /* where it went outside roll, and its version id there */ }
ObjectVersion = { key, version_id, is_latest, delete_marker, size, etag, last_modified }
TrashEntry = { key, size, etag, deleted_at, expires_at, version_id }
Account = { id /* 12 digits */, canonical /* 64 hex */, name, email: string | null, created_at, default: boolean,
            alias: string | null /* CreateAccountAlias */ }
Key = { id /* access key id */, name, permission, buckets: string[] | null, created_at, last_used_at,
        account, user: string | null /* an IAM user's key */, active }
RequesterCharges = { class_a, class_b, egress_internet_bytes, egress_internal_bytes, cost: Cost, accounts: [string] }
Role = { account, name, id /* RR… (AROA… made through IAM) */, arn, description,
         permission: "read" | "write" | "admin" | null /* null: an IAM role */, buckets: string[] | null,
         iam: { path, trust_policy?, policies, attached, boundary?, tags } /* IAM's side */,
         policy: IamPolicy | null, trust: Trust, max_session_s, generation, revoked_before /* ms, 0 = never */,
         created_at, updated_at }
Trust = { keys: string[] /* permanent access key ids (AssumeRole) */,
          oidc: [{ issuer /* exactly the tokens' iss */, audiences: string[], subject?: string /* * and ? */,
                   jwks_uri?: string /* default: the issuer's discovery document */ }],
          external_id?: string }
IamPolicy = an IAM identity policy document (no Principal), as JSON
Session = { access_key_id /* RT… */, secret_access_key, session_token, expiration /* ms */,
            role_arn, assumed_role_arn, permission, buckets: string[] | null, endpoint, region,
            env: { AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, AWS_SESSION_TOKEN, AWS_ENDPOINT_URL, AWS_REGION } }
Pricing = { usd_per_gb_month, usd_per_million_class_a, usd_per_million_class_b,
            usd_per_gb_egress_internet, usd_per_gb_egress_internal }
Usage = { bucket, days, class_a, class_b, bytes_in, egress_internet_bytes, egress_internal_bytes,
          gb_months, storage_bytes, objects,
          daily: [{ day, class_a, class_b, egress_bytes, storage_bytes, usd }],
          cost: Cost }
Cost = { storage_usd, class_a_usd, class_b_usd, egress_usd, total_usd, projected_month_usd }
```

Details:

- `PUT /upload` takes `Content-Type`, `Cache-Control`, `Content-Disposition`,
  `Content-Encoding`, `Content-Language`, `Expires` and `x-amz-meta-*` headers like `PutObject`,
  and returns `ObjectInfo`.
- `POST /delete` with `{ "prefix": "" }` empties the bucket. A body with neither `keys` nor
  `prefix` is a 400. In a versioned bucket these deletes add delete markers.
- `DELETE /object` without `version_id` is S3's DeleteObject (to the trash, or a delete marker
  in a versioned bucket); a key that isn't there is a 404 and adds no marker. With
  `version_id`, that version is deleted for good (to the trash); a missing version is a 404, a
  malformed id a 400, and a version under Object Lock a 409. The response names `version_id`
  and `delete_marker` only when there's a version to name.
- `GET /versions` lists the bucket's versions like ListObjectVersions (current version first
  within a key, delete markers included). `next_cursor` resumes after the last entry.
- Unversioned buckets show `version_id: "null"` everywhere. The usage endpoints count
  `version_bytes` as stored bytes. Object Lock can be turned on at creation
  (`object_lock: true`); its default retention and per-version locks are set through the S3
  API.
- `/download` errors use the same JSON `{error}` body as everything else.
- `DELETE /buckets/{name}?force=true` doesn't put the objects in a trash, because the bucket is
  gone. Their blobs still go through the internal 24 h quarantine. A bucket holding versions
  under Object Lock is a 409.
- Presigned PUT URLs sign only `host`, so clients may send any `Content-Type`.
- MFA Delete: on a bucket with MFA Delete on, `DELETE /object?version_id=`, a `versioning` change
  in a PATCH and `DELETE /buckets/{name}?force=true` are 409s: those take the S3 API with
  `x-amz-mfa`.
- Accounts: `email` is unique across accounts (a 409 otherwise) and stored lowercase; ACL grants
  by email address resolve to it.
- Usage: a bucket's own `usage` doesn't include what requesters paid (Requester Pays); usage of
  buckets deleted within the window counts toward the default account's total.
- Presigned URLs are signed with the control plane's own admin key (`RKROLLCONTROLPLANE00`), the
  default account's, so `/presign` is for the default account's buckets (other accounts presign
  with their own keys).
  It's created once and kept in the metadata, so URLs stay valid across restarts. It's never
  listed by `GET /keys` and can't be deleted through the API.
- `PUT /upload` needs a `Content-Length` (a chunked body is a 400), except for an empty body,
  and is limited to 5 GiB. Larger objects go through the S3 API's multipart upload.
- Uploads and downloads through the control plane are metered like S3 requests (class A for
  an upload and class B for a download). Download bytes count as egress inside roll, because
  the control plane can't tell where its caller is.
- Invalid JSON bodies and query parameters are 400s with the usual `{error}` body. A closed or
  fenced store answers 503.
- `tags` follow S3's bucket tag rules (at most 50, keys 1–128 and values up to 256 characters,
  no `aws:` prefix); invalid tags are a 400 and nothing in the PATCH is applied. `public: true`
  is a 400 when the bucket's public access block (or `ROLL_S3_BLOCK_PUBLIC_ACCESS`) blocks
  public policies.
- `website` is set through the S3 API (`PutBucketWebsite`) or the PATCH's `website`, which sets the
  index and error documents (validated as `PutBucketWebsite` does) and keeps routing rules set
  through the S3 API. `website_url` is `<scheme>://<bucket>.<website domain>[:port]`
  (`ROLL_S3_WEBSITE_DOMAIN`), only when the bucket has a website configuration; it serves what
  anonymous users may read.
- `replication` is validated exactly as `PutBucketReplication` is: unique ids and priorities,
  destinations that are other roll buckets or buckets of replication targets (`target`, or the
  target listing the bucket), and versioning `Enabled` on the bucket and every destination (a
  `versioning` in the same PATCH counts; a target's bucket is asked over the network before
  anything is applied). Rules are always written in S3's V2 (`Filter`) form; what a rule set
  through the S3 API stores but the control plane doesn't show (`SourceSelectionCriteria`, the
  destination's encryption, RTC and metrics, `Account`) is kept for rules whose id is unchanged.
  Replication inside roll is metadata-only: the replica shares the source version's blob, so it's
  instant and its bytes cost nothing to move. Replication to a target copies the bytes (see
  [Events](#events-notifications-replication-inventory)).
- `policy` is validated as `PutBucketPolicy` validates it (20 KB, resources in this bucket,
  `MalformedPolicy` messages as 400s); a public policy is refused while `BlockPublicPolicy` is in
  force (the bucket's own block, a `public_access_block` in the same PATCH, or the deployment's).
  `policy_public` says whether the policy grants anonymous access.
- Everything in a PATCH is validated before anything is applied: a refused field leaves the
  bucket unchanged.
- `notifications` are validated exactly as `PutBucketNotificationConfiguration` validates them:
  each destination must exist and take an `s3:TestEvent` unless `skip_destination_validation`.
  A refused configuration is a 400 and nothing in the PATCH is applied. The webhook secret is
  created with the first webhook destination; `rotate_webhook_secret` replaces it.
- Roles: `trust.keys` must name existing permanent keys; `policy` must be a valid identity
  policy (no `Principal`) of at most 10 KB; issuers and `jwks_uri` are URLs (keys are only ever
  fetched over HTTPS). Invalid input is a 400, a duplicate name a 409, an unknown role a 404.
- `POST /roles/{name}/sessions` has no trust check: the control plane's callers are trusted
  (a backend minting credentials for its users). The secret and token are returned once and
  never stored.

`ROLL_S3_ACCESS_KEY` and `ROLL_S3_SECRET_KEY`, if set, create (or update) an `admin` key with
that id and secret at startup, the way MinIO's root user works. This is for local dev and
tests.

## Pricing

Customers pay **S3 Standard list prices** (us-east-1):

| | price |
|---|---|
| storage | $0.023 / GB-month |
| class A (PUT, COPY, POST, LIST, multipart) | $5.00 / million |
| class B (GET, HEAD, other) | $0.40 / million |
| DELETE, Abort | free |
| egress to the internet | $0.09 / GB |
| egress inside roll (same region) | free |
| egress through the CDN (Cloudflare) | $0.03 / GB |
| image transformations | $5 / 1000 unique source images a month, 100 a month free per account |

Where the margin comes from (see the [pricing notes](pricing.md)):

- Blobs are stored in **S3 Intelligent-Tiering**. Data not read for 30 days costs
  $0.0125/GB, and after 90 days $0.004/GB. There are no retrieval fees and no minimum
  duration, and durability and latency are the same as Standard. Objects under 128 KB are
  never monitored and cost the Standard rate.
- S3's volume tiers are reached across all customers combined.
- Requests pass through at cost. Our backend does one request per customer request; the
  metadata log adds about one PUT per group commit, shared by every write in it.

This means the margin is about 0% for data that is always hot, and 20–40% blended once data
ages. It's never negative per line item. It doesn't depend on packing or compression, both of
which we rejected for reliability.

## Scaling beyond one node

Version 1 runs in rolld: one node owns the metadata (one "cell"). The design scales out as
follows; none of this is built yet:

- **Gateways.** `s3-server` instances are stateless and can run behind an NLB. They forward
  metadata operations to the cell leader, while data goes straight between the gateway and S3.
- **Cells.** Buckets are assigned to cells; each cell has its own log prefix and leader, with
  a standby taking over through the fencing above.
- **Recovery.** A cell can be restored from object storage alone.
