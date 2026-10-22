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
- **Copies are free.** `CopyObject` adds a reference to the same blob.
- **Checksums end to end.**
  - client → gateway: `Content-MD5` and the `x-amz-checksum-*` headers and trailers are verified.
  - gateway → S3: every backend upload carries a CRC32C that S3 verifies.

## Crates

| crate | what |
|---|---|
| `s3-proto` | Wire protocol, no I/O: SigV4 (header, presigned, chunked/streaming payloads with trailers), XML bodies, S3 error codes, checksums, ETags, ranges and conditional headers. |
| `s3-store` | Metadata (SQLite + durable log in object storage), branching, refcounts, trash, lifecycle, keys, metering, the blob backend (S3 via `aws-sdk-s3`, or a local directory), the GC/scrub jobs. No HTTP. |
| `s3-server` | The S3 HTTP API: routing (path and virtual-host style), auth, handlers, streaming to and from the backend, CORS and public buckets. |
| `s3-control` | Control-plane API `/v1/storage/...` (merged into rolld's API): buckets, branches, keys, browsing/upload/download for the dashboard, presigning, trash, usage and pricing. |

`rolld` runs all of it in one process (`--s3`, default `127.0.0.1:59000`), the CLI has
`roll storage` (alias `s3`), and the dashboard has `/dashboard/storage`.

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

## Metadata (`s3-store`)

### Schema (SQLite)

```sql
buckets (id TEXT PRIMARY KEY,           -- bkt_<12>
         name TEXT NOT NULL UNIQUE,     -- S3 bucket name rules
         region TEXT NOT NULL, created_at INTEGER NOT NULL,   -- unix ms
         public INTEGER NOT NULL,       -- anonymous GET/HEAD of objects
         recover_days INTEGER NOT NULL,
         cors TEXT, lifecycle TEXT,     -- JSON (see API), NULL = none
         parent_id TEXT, parent_at INTEGER,                   -- branch of, and when (ms)
         objects INTEGER NOT NULL, bytes INTEGER NOT NULL,    -- own visible rows
         trash_bytes INTEGER NOT NULL)

objects (bucket_id, key TEXT, blob_id TEXT,  -- blob_id NULL = tombstone (branches only)
         size, etag, content_type, cache_control, content_disposition, content_encoding,
         content_language, expires, metadata TEXT /* JSON map */, tags TEXT /* JSON map */,
         checksum_algorithm, checksum_type /* FULL_OBJECT|COMPOSITE */, checksum,
         parts INTEGER /* 0 = single PUT */, part_sizes TEXT /* JSON, for partNumber GETs */,
         modified_at INTEGER /* ms */,
         PRIMARY KEY (bucket_id, key)) WITHOUT ROWID

trash   (bucket_id, key, deleted_at, blob_id, <same columns as objects>,
         PRIMARY KEY (bucket_id, key, deleted_at))

blobs   (id TEXT PRIMARY KEY, size INTEGER, crc32c INTEGER, refs INTEGER NOT NULL,
         created_at INTEGER, unreferenced_at INTEGER)   -- set when refs drops to 0

uploads (id TEXT PRIMARY KEY /* customer UploadId */, bucket_id, key, blob_id,
         backend_upload_id TEXT, <object header columns>, created_at)
parts   (upload_id, part_number, size, etag, checksum, backend_etag, created_at, crc32c,
         PRIMARY KEY (upload_id, part_number))

keys    (id TEXT PRIMARY KEY /* access key id, 20 chars: RK + 18 upper base32 */,
         secret TEXT NOT NULL /* 40 chars */, name TEXT NOT NULL,
         permission TEXT NOT NULL /* read | write | admin */,
         buckets TEXT /* JSON array of bucket names; NULL = all */,
         created_at INTEGER, last_used_at INTEGER)

usage   (bucket_id, day TEXT /* YYYY-MM-DD UTC */, class_a, class_b, bytes_in,
         egress_internet, egress_internal,
         byte_ms REAL /* stored bytes × ms; a float, since a PB-day overflows i64 */,
         PRIMARY KEY (bucket_id, day))

orphans (blob_key TEXT PRIMARY KEY, size, found_at)   -- orphan-scan candidates
state   (k TEXT PRIMARY KEY, v TEXT)                  -- scrub report, job barriers
```

`refs` counts every `objects` row and every `trash` row that points at the blob.

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

### Deletes, trash, quarantine, GC

- **Delete or overwrite.** The old row moves to `trash` (if `recover_days > 0`), otherwise
  `refs - 1`. In a branch, deleting or overwriting an inherited object puts a copy of it in
  the branch's trash (`refs + 1`) and hides the key with a tombstone.
- `refs` never goes below zero, and a blob at zero is never referenced again: either would
  be a bookkeeping bug, so the write fails instead.
- **Restore.** The trash row moves back to `objects`. Whatever is there now goes to the trash.
- **Sweeper (hourly).** It starts with a barrier (a durable write, which fails on a deposed
  node), then does three things:
  - Trash rows older than `recover_days` are removed (`refs - 1`).
  - Blobs with `refs = 0` and `unreferenced_at` older than 24 h are deleted from the
    backend, then their row is removed.
  - Uploads older than the bucket's abort rule (default 7 days) are aborted.
- **Lifecycle (daily).** `expire_days` rules delete matching objects as a normal delete,
  so they go to the trash and are recoverable. Age and ETag are checked again inside the
  delete transaction, so a version written after the listing is never expired.
- **DeleteBucket.** Without `force`, the bucket's view is checked empty inside the same
  transaction that deletes it, so a write that lands during the delete fails the delete
  (`BucketNotEmpty`) instead of being deleted with it.
- **Orphan scan (daily).** A blob without a `blobs` row or an active upload is left when a
  crash hits between the blob upload and its metadata commit. That write was never
  acknowledged. Deletion takes two scans, so a node that lost the log to another one (and
  doesn't know that node's new blobs) can never delete them:
  1. A scan records unknown blobs (not in flight on this node) in `orphans`, in a durable
     write, which fails on a deposed node.
  2. A scan at least 24 h later deletes the recorded ones still unknown.

  An unknown blob more than 7 days old means metadata went missing. It's never recorded or
  deleted, only logged and counted (`unexpected_orphans`).
- **Scrub (weekly).** It compares the backend listing with the `blobs` table and logs every
  referenced blob that's missing or has the wrong size (`tracing::error!`, and a counter in
  `GET /v1/storage`). It never deletes.

### Write paths

- **PUT.** Upload the blob to the backend with its CRC32C. Then one metadata transaction:
  - insert the blob with `refs = 1`
  - upsert the object
  - move the old row to the trash and push the old state down to children
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
  - `UploadPartCopy` is server-side, so the gateway never sees the bytes. The part's ETag is
    the source's when it's a whole single-PUT object, otherwise an MD5 of the source blob
    and range.
- **Copy.** `refs + 1` on the source blob, then the new object row (with `REPLACE`d
  metadata, if asked). No data moves.

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

Every 60 s the meter flushes into `usage`. It also adds `(bytes + trash_bytes) × elapsed ms`
for every bucket, which gives storage GB-months, so restorable trash is billed as storage.
Usage writes are relaxed: they ride the next group commit without waiting.

## S3 API (`s3-server`, `s3-proto`)

**Addressing.**
- Path style is always on: `http://host:59000/<bucket>/<key>`.
- Virtual-host style works when `ROLL_S3_DOMAIN` is set (`<bucket>.<domain>`).

**Auth.**
- SigV4: the `Authorization` header and presigned query strings (max 7 days).
- Payloads: `UNSIGNED-PAYLOAD`, a hex SHA-256, `STREAMING-AWS4-HMAC-SHA256-PAYLOAD(-TRAILER)`
  and `STREAMING-UNSIGNED-PAYLOAD-TRAILER` (current SDKs send CRC trailers by default).
- The credential scope's region is accepted as given (any region; R2 does the same). The
  service must be `s3`.
- Clock skew is limited to 15 minutes.
- Anonymous requests are allowed only for GET/HEAD of objects in `public` buckets.

**Permissions.**
- `read`: GET, HEAD, List*
- `write`: read, plus PUT, DELETE, copy and multipart
- `admin`: write, plus create/delete bucket and bucket configuration

A key with a `buckets` list only sees those buckets (ListBuckets is filtered).

**Operations.**
- **Service:** ListBuckets.
- **Bucket:**
  - CreateBucket, DeleteBucket (must be empty), HeadBucket and GetBucketLocation
  - Get/Put/DeleteBucketCors
  - Get/Put/DeleteBucketLifecycleConfiguration (Expiration.Days and
    AbortIncompleteMultipartUpload, filtered by prefix)
  - GetBucketVersioning (always unversioned)
  - ListObjectsV2, ListObjects (v1), ListMultipartUploads
  - DeleteObjects (batch, quiet mode)
- **Object:**
  - PutObject, GetObject and HeadObject: `Range`, `partNumber`, conditional headers,
    `response-*` overrides and checksum mode
  - DeleteObject
  - CopyObject: `x-amz-metadata-directive` and copy-source conditionals
  - Get/Put/DeleteObjectTagging
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

## Control-plane API (`s3-control`, `/v1/storage`)

All JSON. Errors are `{"error": "..."}` with 400/404/409/500. Times are Unix milliseconds.

```text
GET    /v1/storage
  → { endpoint, region, virtual_host_domain|null, storage, storage_class, recover_days_default,
      pricing: Pricing, buckets, objects, bytes, scrub: { last_at|null, missing, mismatched } }

GET    /v1/storage/buckets                       → [Bucket]
POST   /v1/storage/buckets  { name, public?, recover_days? }   → Bucket
GET    /v1/storage/buckets/{name}                → Bucket
PATCH  /v1/storage/buckets/{name}  { public?, recover_days?, cors?: [CorsRule], lifecycle?: [LifecycleRule] } → Bucket
DELETE /v1/storage/buckets/{name}?force=true     → { deleted }   (force: delete all objects first)
POST   /v1/storage/buckets/{name}/branches  { name }  → Bucket

GET    /v1/storage/buckets/{name}/objects?prefix=&delimiter=/&cursor=&limit=200
  → { objects: [ObjectSummary], prefixes: [string], next_cursor: string|null }
GET    /v1/storage/buckets/{name}/object?key=    → ObjectInfo
DELETE /v1/storage/buckets/{name}/object?key=    → { deleted: key }
POST   /v1/storage/buckets/{name}/delete  { keys?: [string], prefix?: string } → { deleted: n }
GET    /v1/storage/buckets/{name}/download?key=  → the bytes (Content-Disposition: attachment)
PUT    /v1/storage/buckets/{name}/upload?key=    (raw body, Content-Type) → ObjectInfo
POST   /v1/storage/buckets/{name}/presign  { key, method: "GET"|"PUT", expires_s? (default 3600) }
  → { url, method, expires_at }

GET    /v1/storage/buckets/{name}/trash?prefix=&cursor=&limit=
  → { objects: [TrashEntry], next_cursor }
POST   /v1/storage/buckets/{name}/trash/restore  { key, deleted_at } → ObjectInfo

GET    /v1/storage/buckets/{name}/usage?days=30  → Usage
GET    /v1/storage/usage?days=30                 → { days, cost: Cost, daily: [same rows as Usage.daily, summed], buckets: [Usage] }

GET    /v1/storage/keys                          → [Key]
POST   /v1/storage/keys  { name, permission, buckets?: [string] } → Key & { secret_access_key }
DELETE /v1/storage/keys/{id}                     → { deleted }
```

```ts
Bucket = { id, name, region, public, recover_days, created_at,
           parent: { id, name, at } | null, branches: [string],
           objects, bytes, trash_bytes, cors: CorsRule[], lifecycle: LifecycleRule[],
           endpoint, url /* path-style */, virtual_host_url: string | null }
CorsRule = { id?: string | null /* S3's optional rule ID */, allowed_origins: string[],
             allowed_methods: string[], allowed_headers: string[], expose_headers: string[],
             max_age_s: number | null }
LifecycleRule = { id, prefix, enabled, expire_days: number | null, abort_multipart_days: number | null }
ObjectSummary = { key, size, etag, content_type, last_modified }
ObjectInfo = ObjectSummary & { metadata: Record<string,string>, tags: Record<string,string>,
             cache_control, content_disposition, content_encoding, content_language, expires,
             checksum: { algorithm, type, value } | null, parts }
TrashEntry = { key, size, etag, deleted_at, expires_at }
Key = { id /* access key id */, name, permission, buckets: string[] | null, created_at, last_used_at }
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
  `prefix` is a 400.
- `/download` errors use the same JSON `{error}` body as everything else.
- `DELETE /buckets/{name}?force=true` doesn't put the objects in a trash, because the bucket is
  gone. Their blobs still go through the internal 24 h quarantine.
- Presigned PUT URLs sign only `host`, so clients may send any `Content-Type`.
- Presigned URLs are signed with the control plane's own admin key (`RKROLLCONTROLPLANE00`).
  It's created once and kept in the metadata, so URLs stay valid across restarts. It's never
  listed by `GET /keys` and can't be deleted through the API.
- `PUT /upload` needs a `Content-Length` (a chunked body is a 400), except for an empty body,
  and is limited to 5 GiB. Larger objects go through the S3 API's multipart upload.
- Uploads and downloads through the control plane are metered like S3 requests (class A for
  an upload and class B for a download). Download bytes count as egress inside roll, because
  the control plane can't tell where its caller is.
- Invalid JSON bodies and query parameters are 400s with the usual `{error}` body. A closed or
  fenced store answers 503.

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
