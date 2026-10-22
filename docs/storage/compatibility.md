# S3 compatibility

roll storage implements the part of the S3 API that SDKs and tools actually use. Anything not
listed under "supported" returns `501 NotImplemented` with the standard S3 XML error body, so
clients fail clearly instead of silently doing the wrong thing.

## Addressing and auth

| feature | status |
|---|---|
| Path style (`http://host:59000/<bucket>/<key>`) | ✅ always on |
| Virtual-host style (`<bucket>.<domain>`) | ✅ when `ROLL_S3_DOMAIN` is set |
| SigV4 `Authorization` header | ✅ |
| SigV4 presigned URLs (query string) | ✅ up to 7 days |
| `UNSIGNED-PAYLOAD`, hex SHA-256 payloads | ✅ |
| `STREAMING-AWS4-HMAC-SHA256-PAYLOAD` and `-TRAILER` (aws-chunked) | ✅ chunk signatures verified |
| `STREAMING-UNSIGNED-PAYLOAD-TRAILER` (current SDK default) | ✅ |
| Presigned POST policy (browser form upload) | ✅ |
| Any region in the credential scope | ✅ accepted as given, like R2's `auto` |
| SigV2, SigV4a | ❌ |
| Anonymous requests | GET and HEAD of objects in `public` buckets only |
| IAM, STS, session tokens | ❌ use roll access keys (`read`, `write`, `admin`, optionally scoped to buckets) |

## Operations

### Service and buckets

| operation | status |
|---|---|
| ListBuckets | ✅ filtered to the key's buckets |
| CreateBucket, DeleteBucket (must be empty), HeadBucket | ✅ need an `admin` key |
| GetBucketLocation | ✅ |
| Get/Put/DeleteBucketCors | ✅ |
| Get/Put/DeleteBucketLifecycleConfiguration | ✅ the full rule model: `Filter` (`Prefix`, `Tag`, `And`, `ObjectSizeGreaterThan`/`LessThan`) or the legacy `<Rule><Prefix>`; `Expiration` (`Days`, `Date`, `ExpiredObjectDeleteMarker`); `NoncurrentVersionExpiration` (`NoncurrentDays`, `NewerNoncurrentVersions`); `AbortIncompleteMultipartUpload`; `Transition` and `NoncurrentVersionTransition` (stored, see below). Validated like S3. |
| Get/PutBucketVersioning | ✅ `Enabled` and `Suspended`. `MfaDelete=Enabled` is `NotImplemented` (there are no MFA devices; use Object Lock). |
| ListObjectVersions | ✅ `prefix`, `delimiter`, `key-marker`, `version-id-marker`, `max-keys`, `encoding-type=url`, delete markers |
| Get/PutObjectLockConfiguration | ✅ enable on a versioned bucket (or at CreateBucket with `x-amz-bucket-object-lock-enabled`), default retention in `Days` or `Years` |
| ListObjectsV2, ListObjects (v1) | ✅ `prefix`, `delimiter`, `start-after`, continuation tokens, `max-keys`, `encoding-type=url` |
| ListMultipartUploads | ✅ |
| DeleteObjects | ✅ up to 1000 keys, `VersionId` per key, quiet mode, `DeleteMarker`/`DeleteMarkerVersionId` in the result, `x-amz-bypass-governance-retention` |

### Objects

| operation | status |
|---|---|
| PutObject | ✅ returns `x-amz-version-id` in versioned buckets |
| GetObject, HeadObject | ✅ `versionId`, `Range`, `partNumber`, `If-Match`, `If-None-Match`, `If-Modified-Since`, `If-Unmodified-Since`, `response-*` overrides, `x-amz-checksum-mode`; Object Lock headers; a delete marker answers 404 (current) or 405 (by id) with `x-amz-delete-marker: true` |
| DeleteObject | ✅ unversioned: the object goes to the bucket's trash. Versioned: a delete marker; with `versionId`, that version is deleted for good (to the trash, see below) |
| CopyObject | ✅ `x-amz-metadata-directive`, `x-amz-copy-source-if-*`, `?versionId=` in the source, `x-amz-copy-source-version-id`; no data is copied |
| Conditional writes: `If-None-Match: *`, `If-Match` on PutObject and CompleteMultipartUpload | ✅ |
| Get/Put/DeleteObjectTagging, `x-amz-tagging` | ✅ with `versionId` |
| Get/PutObjectRetention, Get/PutObjectLegalHold | ✅ `versionId`, `x-amz-bypass-governance-retention` (admin keys) |
| `x-amz-object-lock-mode`, `-retain-until-date`, `-legal-hold` | ✅ on PutObject (with `Content-MD5` or an `x-amz-checksum-*`, as S3 requires), CopyObject, CreateMultipartUpload and POST Object |
| CreateMultipartUpload, UploadPart, UploadPartCopy, CompleteMultipartUpload, AbortMultipartUpload, ListParts | ✅ UploadPartCopy takes a source `versionId`; Complete returns `x-amz-version-id` |
| POST Object (form upload) | ✅ |

### Headers and metadata

| feature | status |
|---|---|
| `Content-Type`, `Cache-Control`, `Content-Disposition`, `Content-Encoding`, `Content-Language`, `Expires` | ✅ stored and returned |
| `x-amz-meta-*` user metadata (2 KB) | ✅ |
| `Content-MD5` | ✅ verified (`BadDigest` on mismatch) |
| `x-amz-checksum-crc32`, `-crc32c`, `-crc64nvme`, `-sha1`, `-sha256`, in headers or trailers | ✅ verified, stored, and returned with `x-amz-checksum-mode: ENABLED` |
| Multipart checksums | ✅ `COMPOSITE`, and `FULL_OBJECT` for the CRC algorithms |
| ETags | ✅ MD5 for single PUTs; `md5-of-md5s-N` for multipart, as on S3 |
| `x-amz-storage-class` | accepted; everything is stored in Intelligent-Tiering, so no other class is needed |

## Not supported

| feature | notes |
|---|---|
| MFA Delete | No MFA devices. Object Lock protects versions instead. |
| SSE-C (`x-amz-server-side-encryption-customer-*`) | rejected with `NotImplemented`, never ignored |
| SSE-KMS (`x-amz-server-side-encryption: aws:kms`) | rejected. Data is encrypted at rest by the backing S3 bucket (SSE-S3). |
| ACLs (`PutObjectAcl`, `x-amz-acl`, grants) | Use the bucket's `public` flag and scoped keys. `x-amz-acl: private` is accepted. |
| Bucket policies (`PutBucketPolicy`) | Use scoped keys and the `public` flag. |
| Bucket tagging | |
| Replication configuration | The backing bucket can be replicated by the operator. |
| Static website hosting | |
| S3 Select, Object Lambda | |
| Torrent | |
| Requester pays, transfer acceleration, inventory, analytics, metrics configuration, notifications | |
| Restore from Glacier (`RestoreObject`) | Nothing is archived in a way that needs a restore. |
| Directory buckets (S3 Express) | |

## Behaviour differences

- **Overwrites and deletes are recoverable** for `recover_days` (default 7). On S3 they're gone
  unless versioning is on. Trashed bytes are billed as storage.
- **Even permanent deletes are recoverable.** In a versioned bucket, overwrites and deletes
  make the old version noncurrent (as on S3), and deleting a version by id, or lifecycle
  noncurrent expiry, moves it to the trash for `recover_days` instead of destroying it. A
  restored version goes back to its place in the key's history: behind a newer current version
  (or delete marker) it's noncurrent again, otherwise current. Delete markers carry no data and
  aren't kept in the trash.
- **Version ids** are 32 lowercase letters and digits that sort newest first within a key.
  Objects written before versioning was enabled are the `null` version, as on S3. A malformed
  version id is `400 InvalidArgument`; a well-formed one that doesn't exist is `NoSuchVersion`
  (and a no-op `204` for DeleteObject, as on S3).
- **Noncurrent versions are billed** as stored bytes, as on S3 (`version_bytes` on the bucket).
- **Lifecycle transitions are accepted, validated, stored and returned, but never move data.**
  Every object is already in S3 Intelligent-Tiering, which moves cold data to cheaper tiers by
  access with no retrieval fees, so a transition to `STANDARD_IA` or `GLACIER` would only add
  cost and latency. No object ever reports a different storage class because of a transition.
- **Lifecycle expiration days** are counted from the version's time (or from when it became
  noncurrent), without S3's rounding to the next midnight UTC, and the job runs daily.
  `ExpiredObjectDeleteMarker` removes markers only when the rule says so.
- **Object Lock is enforced everywhere a version could be destroyed:** the API, lifecycle,
  trash expiry (a version deleted with a GOVERNANCE bypass stays in the trash until its lock
  ends), forced bucket deletes and the control plane. A bucket holding locked versions can't be
  deleted. A default retention in `Years` counts 365 days a year. `x-amz-bypass-governance-retention` needs an `admin` key. Branches copy a version's
  lock with the version, so a branch holding a locked version can't be deleted either. S3
  requires `Content-MD5` for uploads into a bucket with a default retention; roll only requires
  an integrity check when the request itself sets Object Lock headers, and verifies every body's
  checksum anyway.
- **Versioning and branches:** a branch inherits its parent's versioning state and Object Lock
  configuration when it's made. Its version history is its own versions plus the current
  version it inherits; the parent's noncurrent versions aren't part of it.
- **Copies are free and instant,** whatever the size. On S3 a copy re-reads and re-writes the
  data.
- **Writes have higher latency.** A write is acknowledged once its metadata log commit is in S3
  too, which adds roughly one S3 PUT of latency.
- **One region per deployment.** `GetBucketLocation` returns the deployment's region, and
  `CreateBucket`'s `LocationConstraint` is accepted but not used.
- **Bucket names are unique per deployment,** not across all of AWS.
