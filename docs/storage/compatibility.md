# S3 compatibility

roll storage implements the part of the S3 API that SDKs and tools actually use. Anything not
listed under "supported" returns `501 NotImplemented` with the standard S3 XML error body, so
clients fail clearly instead of silently doing the wrong thing.

## Conformance (ceph/s3-tests)

[ceph/s3-tests](https://github.com/ceph/s3-tests) is the S3 conformance suite most S3-compatible
stores measure themselves with. `pnpm test:s3-conformance` runs it (pinned, in Docker) against a
fresh roll server, with the selection AWS-compatible implementations use: `test_s3.py` and
`test_headers.py` and `test_s3select.py` without the tests AWS itself fails (`fails_on_aws`) and
without Ceph's extensions (RGW's append API, IAM/STS/SNS, cloud tiering, lifecycle transitions between storage
classes, RGW's logging journal and rollover). It runs on two backends:

- **local** (`pnpm test:s3-conformance`): blobs in a local directory, so no SSE-KMS and no
  encryption at rest. **477 of 681 tests pass (70.1%).**
- **LocalStack** (`pnpm test:s3-conformance-localstack`): the backing bucket on LocalStack S3
  with keys from LocalStack KMS (`ROLL_S3_KMS`), and `ROLL_S3_SSE_S3_AT_REST` set, since
  LocalStack's S3 encrypts every object at rest as AWS S3 does. This is how roll runs on AWS.
  **537 of 681 tests pass (79.0%).**

One more test is skipped by the suite in both. The suite's "alt" and "tenant" users are real
separate accounts. Every other test is listed with its reason in
[`tests/s3-conformance/expected.txt`](../../tests/s3-conformance/expected.txt) (a line for one
backend only says so), and the script fails if anything changes either way:

| local | LocalStack | why they don't pass |
|---|---|---|
| 88 | 88 | **S3's current defaults, which the suite predates.** New buckets are `BucketOwnerEnforced` (ACLs off), as on S3 since April 2023, so tests that set ACLs without `ObjectOwnership` get `InvalidBucketAclWithObjectOwnership`/`AccessControlListNotSupported` (as do ACLs that don't give the bucket owner full control, and logging `TargetGrants` for such a target), and objects another account writes belong to the bucket owner, as they would on S3. `GetBucketEncryption` returns the default `AES256` rule. |
| 62 | 0 | **The local directory backend.** SSE-KMS needs AWS KMS behind the backing bucket (`NotImplemented` on a local directory), and SSE-S3 is reported on reads only where the backing store encrypts at rest. |
| 34 | 34 | **The suite.** Expectations specific to Ceph RGW (error codes S3 doesn't use, `x-amz-delete-marker: false`, its accelerated lifecycle clock, tenant-qualified bucket names like `tenant:bucket`, which AWS doesn't have) and tests that can't pass on S3 as written (botocore re-signs after the test changes the `Authorization` header; a key that was invalid UTF-8 under Python 2 is valid under Python 3). |
| 10 | 11 | **roll answers as S3 does where RGW differs:** a wrong SSE-C key is `403 AccessDenied`, `GetBucketPolicyStatus` without a policy is `NoSuchBucketPolicy`, a policy repeating a `Sid` is `MalformedPolicy`, a logging target another account owns is `InvalidTargetBucketForLogging`; with KMS, `aws:kms` without a key id uses the AWS managed key. |
| 6 | 6 | **S3 Select: RGW's dialect where it differs from AWS's.** Bare values from JSON output (AWS writes `{"_1":…}` records), the text `null` for empty CSV fields, bracket classes in LIKE, a double-quoted `"_3"` read as a string (it's column 3), RGW's own error messages. |
| 0 | 1 | **LocalStack.** Its `UploadPartCopy` ignores SSE-C, so roll refuses the part it can't confirm was encrypted (`NotImplemented`) and keeps nothing. |

Nothing is listed as deliberate any more: a delete with a GOVERNANCE bypass removes the lock as
on S3 (see [Behaviour differences](#behaviour-differences)).

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
| SigV4a (`AWS4-ECDSA-P256-SHA256`) header and presigned URLs | ✅ any `x-amz-region-set` is accepted |
| `STREAMING-AWS4-ECDSA-P256-SHA256-PAYLOAD` and `-TRAILER` | ✅ ECDSA chunk signatures verified, `*`-padded ones too |
| SigV2 (`Authorization: AWS key:signature`) | ✅ including virtual-host style; clock skew 15 minutes |
| The `s3express` signing service, `x-amz-s3session-token` (S3 Express sessions) | ✅ see [Directory buckets](#directory-buckets-s3-express-one-zone) |
| SigV2 presigned URLs (`AWSAccessKeyId`, `Expires`, `Signature`) | ✅ no 7-day cap, as on S3 |
| Presigned POST policy (browser form upload) | ✅ SigV4 and SigV2 (`AWSAccessKeyId`, `signature`) |
| Any region in the credential scope | ✅ accepted as given, like R2's `auto` |
| Anonymous requests | what the bucket's policy or ACLs allow, and GET/HEAD of objects in `public` buckets |
| Bucket policies | ✅ enforced; principals are accounts, IAM users, roles, sessions and roll access keys (see [Policies](#policies-acls-and-public-access)) |
| Multiple accounts | ✅ each with its own 12-digit id, canonical id, buckets, keys, IAM entities (see [Accounts](#accounts)) |
| `x-amz-expected-bucket-owner`, `x-amz-source-expected-bucket-owner` | ✅ compared with the bucket owner's account id |
| Website endpoint (`<bucket>.<website domain>`) | ✅ when `ROLL_S3_WEBSITE_DOMAIN` is set |
| STS temporary credentials (`x-amz-security-token`, `AWS_SESSION_TOKEN`) | ✅ `GetSessionToken`, `AssumeRole`, `AssumeRoleWithWebIdentity`, `GetFederationToken`, `GetCallerIdentity` on the same endpoint (see [STS](#sts-temporary-credentials)) |
| IAM users, groups, roles, managed policies, MFA | ✅ the IAM query API on the same endpoint (see [IAM](#iam)); roll access keys with a permission level keep working |
| Access points (by ARN, `<name>-<account>.…` host, `/arn:…` path, or `-s3alias` alias) | ✅ see [Access points](#access-points) |
| S3 Control API (`<account>.s3-control.<domain>`, or `/v20180820/…` with `x-amz-account-id` on the S3 endpoint) | ✅ access points and Object Lambda access points only |

## Operations

### Service and buckets

| operation | status |
|---|---|
| ListBuckets | ✅ the caller's account's buckets (a key scoped to buckets sees those); `max-buckets` and `continuation-token` pages, `prefix`, `bucket-region` |
| CreateBucket, DeleteBucket (must be empty), HeadBucket | ✅ need `s3:CreateBucket` (an `admin` key, the root user, or an IAM policy). Bucket names are unique across the deployment: another account's name is `BucketAlreadyExists`; creating a bucket you already have is a no-op `200` in `us-east-1` and `BucketAlreadyOwnedByYou` in other regions, as on S3 (S3 also resets the bucket's ACL; roll changes nothing). |
| GetBucketLocation | ✅ |
| Get/Put/DeleteBucketCors | ✅ |
| Get/Put/DeleteBucketLifecycleConfiguration | ✅ the full rule model: `Filter` (`Prefix`, `Tag`, `And`, `ObjectSizeGreaterThan`/`LessThan`) or the legacy `<Rule><Prefix>`; `Expiration` (`Days`, `Date`, `ExpiredObjectDeleteMarker`); `NoncurrentVersionExpiration` (`NoncurrentDays`, `NewerNoncurrentVersions`); `AbortIncompleteMultipartUpload`; `Transition` and `NoncurrentVersionTransition` (stored, see below). Validated like S3. |
| Get/PutBucketVersioning | ✅ `Enabled` and `Suspended`, and MFA Delete (see [MFA](#mfa-and-mfa-delete)) |
| ListObjectVersions | ✅ `prefix`, `delimiter`, `key-marker`, `version-id-marker`, `max-keys`, `encoding-type=url`, delete markers |
| Get/PutObjectLockConfiguration | ✅ enable on a versioned bucket (or at CreateBucket with `x-amz-bucket-object-lock-enabled`), default retention in `Days` or `Years` |
| ListObjectsV2, ListObjects (v1) | ✅ `prefix`, `delimiter`, `start-after`, continuation tokens, `max-keys`, `encoding-type=url` (a space is `%20`; S3 sends `+`; SDKs decode both) |
| ListMultipartUploads | ✅ |
| DeleteObjects | ✅ up to 1000 keys, `VersionId` per key, quiet mode, `DeleteMarker`/`DeleteMarkerVersionId` in the result, `x-amz-bypass-governance-retention`; each key is authorized on its own |
| Get/Put/DeleteBucketTagging | ✅ up to 50 tags, S3's key/value rules |
| Get/Put/DeleteBucketPolicy, GetBucketPolicyStatus | ✅ enforced (see [Policies](#policies-acls-and-public-access)) |
| Get/Put/DeletePublicAccessBlock | ✅ enforced against ACLs, policies and the `public` flag |
| Get/Put/DeleteBucketOwnershipControls | ✅ `BucketOwnerEnforced` by default, as on S3 since 2023 |
| Get/PutBucketAcl, Get/PutObjectAcl, `x-amz-acl`, `x-amz-grant-*` | ✅ enforced when ownership allows ACLs; otherwise `AccessControlListNotSupported`, as on S3 |
| Get/Put/DeleteBucketEncryption | ✅ `AES256` by default (the backing bucket encrypts at rest); `aws:kms` rules are stored |
| Get/Put/DeleteBucketWebsite | ✅ served on the website endpoint |
| Get/PutBucketLogging | ✅ server access logs delivered to the target bucket every 5 minutes, best effort |
| Get/PutBucketAccelerateConfiguration | ✅ stored and returned; a no-op (there's one endpoint) |
| Get/PutBucketRequestPayment | ✅ `BucketOwner` and `Requester` (see [Requester Pays](#requester-pays)) |
| Get/Put/DeleteBucketReplication | ✅ between roll buckets (metadata-only) and to buckets outside roll (AWS S3 in any account or region, any S3-compatible endpoint, another roll deployment) through replication targets; replicas keep their source's version id (except on services that assign their own), Object Lock, metadata changes, `ReplicaModifications`, `SseKmsEncryptedObjects`, Replication Time Control and replication metrics (see [Replication](#replication)) |
| Get/PutBucketNotificationConfiguration | ✅ Topic, Queue, CloudFunction and EventBridge configurations, event wildcards, `prefix`/`suffix` filter rules, `x-amz-skip-destination-validation`, `s3:TestEvent` (see [Event notifications](#event-notifications)) |
| Inventory configurations (Get/Put/Delete/List) | ✅ reports generated Daily/Weekly as CSV (see [Inventory](#inventory-reports)); ORC and Parquet are `NotImplemented` |
| ListDirectoryBuckets, CreateSession | ✅ see [Directory buckets](#directory-buckets-s3-express-one-zone) |
| Analytics, metrics and intelligent-tiering configurations (Get/Put/Delete/List) | ✅ validated and stored. Analytics aren't computed and metrics aren't published yet. Intelligent-tiering archive tiers are `NotImplemented` (nothing is ever archived). |

### Objects

| operation | status |
|---|---|
| PutObject | ✅ returns `x-amz-version-id` in versioned buckets (not for the `null` version a suspended bucket writes, as on S3), and `x-amz-expiration` when a lifecycle rule will expire the object |
| GetObject, HeadObject | ✅ `versionId`, `Range`, `partNumber`, `If-Match`, `If-None-Match`, `If-Modified-Since`, `If-Unmodified-Since`, `response-*` overrides, `x-amz-checksum-mode`; Object Lock headers; `x-amz-replication-status`; `x-amz-expiration` when a lifecycle rule will expire the object; a delete marker answers 404 (current) or 405 (by id) with `x-amz-delete-marker: true`. A missing key is a 404 for a requester who may list it (`s3:ListBucket`, with `s3:prefix` the key) and a 403 otherwise, as on S3. `x-amz-server-side-encryption` on a read is `400 InvalidArgument`. |
| DeleteObject | ✅ unversioned: the object goes to the bucket's trash. Versioned: a delete marker; with `versionId`, that version is deleted for good (to the trash, see below) |
| CopyObject | ✅ `x-amz-metadata-directive`, `x-amz-copy-source-if-*`, `?versionId=` in the source, `x-amz-copy-source-version-id`; the source by bucket, by access point ARN (`arn:aws:s3:<region>:<account>:accesspoint/<name>/object/<key>`, URL-encoded or not) or by access point alias (see [Access points](#access-points)); no data is copied |
| Conditional writes: `If-None-Match: *`, `If-Match` on PutObject and CompleteMultipartUpload | ✅ `If-Match` on a missing key is `404 NoSuchKey`. `If-None-Match` also takes ETags (S3 only `*`): the write fails if the object there has one of them. |
| Get/Put/DeleteObjectTagging, `x-amz-tagging` | ✅ with `versionId` |
| GetObjectAttributes | ✅ `ETag`, `Checksum`, `ObjectParts` (part numbers, sizes and, when the upload had checksums, each part's checksum; `x-amz-max-parts`, `x-amz-part-number-marker`), `StorageClass`, `ObjectSize`; `versionId` and the conditional headers. Parts are numbered 1..N as for `GET ?partNumber=` |
| Get/PutObjectRetention, Get/PutObjectLegalHold | ✅ `versionId`, `x-amz-bypass-governance-retention` (admin keys). A GOVERNANCE retention can be extended freely; shortening it, removing it or turning it into COMPLIANCE needs the bypass. |
| `x-amz-object-lock-mode`, `-retain-until-date`, `-legal-hold` | ✅ on PutObject (with `Content-MD5` or an `x-amz-checksum-*`, as S3 requires), CopyObject, CreateMultipartUpload and POST Object |
| CreateMultipartUpload, UploadPart, UploadPartCopy, CompleteMultipartUpload, AbortMultipartUpload, ListParts | ✅ UploadPartCopy takes a source `versionId`, and a source named like CopyObject's; Complete returns `x-amz-version-id`. Completing an upload again (an SDK retrying a completion whose answer was lost) gets the same answer, as on S3, as long as the key still holds what those parts made. |
| POST Object (form upload) | ✅ including the `acl`, `x-amz-website-redirect-location` and Object Lock fields |
| RestoreObject | ✅ answers `InvalidObjectState` (403), as S3 does for objects that aren't archived: nothing ever is |
| PutObject with `x-amz-write-offset-bytes` (appends) | ✅ directory buckets, as on S3 (see [Appends](#appends)) |
| RenameObject (`x-amz-rename-source`, `x-amz-rename-source-if-*`, destination `If-*`, `x-amz-client-token`) | ✅ directory buckets, and general purpose buckets as a roll extension (see [RenameObject](#renameobject)) |
| SelectObjectContent | ✅ CSV and JSON, GZIP and BZIP2, `ScanRange`, S3 Select's SQL (see [S3 Select](#s3-select)); Parquet is `NotImplemented` |
| GetObjectTorrent | ✅ BitTorrent metainfo files, as S3 made them (see [Torrents](#torrents)) |

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
| `x-amz-website-redirect-location` | ✅ on PutObject, CopyObject and CreateMultipartUpload; stored, returned, and followed by the website endpoint |
| `x-amz-expected-bucket-owner`, `x-amz-source-expected-bucket-owner` | ✅ `AccessDenied` unless it's the deployment's 12-digit account id (`account_id` in `GET /v1/storage`) |

### Server-side encryption

The backing object store does the encryption: roll passes the SSE parameters through on the
object's own requests to S3, so durability and integrity are exactly S3's. Customer keys are
never stored; the metadata keeps their MD5 to tell a right key from a wrong one.

| feature | status |
|---|---|
| SSE-S3 (`x-amz-server-side-encryption: AES256`) | ✅ on AWS S3 every object is encrypted at rest, so every write and read reports `AES256`, as S3 does since 2023. On other backends it's reported only on the write that asked for it, unless the operator sets `ROLL_S3_SSE_S3_AT_REST=true` to vouch that the backing store encrypts everything at rest (roll doesn't encrypt or check it). |
| SSE-C (`x-amz-server-side-encryption-customer-*`) on PUT, GET, HEAD, CreateMultipartUpload, UploadPart, CompleteMultipartUpload, POST Object | ✅ |
| SSE-C copy sources (`x-amz-copy-source-server-side-encryption-customer-*`) on CopyObject and UploadPartCopy | ✅ |
| SSE-KMS (`aws:kms`, `aws:kms:dsse`, `-aws-kms-key-id`, `-context`, `-bucket-key-enabled`) | ✅ on AWS S3 backends; `NotImplemented` elsewhere (see below) |
| The bucket's default encryption | ✅ applied to writes without SSE headers |
| Ranges, `partNumber`, conditional reads, checksums on encrypted objects | ✅ checksums are of the plaintext, as on S3 |

S3's errors, as S3 sends them:

- GET or HEAD of an SSE-C object without its key: `400 InvalidRequest`. With another key:
  `403 AccessDenied`. A key whose MD5 doesn't match `-key-MD5`: `400 InvalidArgument`.
- SSE-C headers for an object that isn't SSE-C: `400 InvalidRequest`.
- A part upload without the upload's key: `400 InvalidRequest`; with another: `403 AccessDenied`.
- SSE-C together with `x-amz-server-side-encryption`, an unknown algorithm or a malformed key or
  context: `400 InvalidArgument`.

Details:

- **ETags.** SSE-C and SSE-KMS objects (and their parts) get opaque ETags that aren't the MD5 of
  the data, as on S3; multipart ones still end in `-<parts>`. SSE-S3 ETags are MD5s.
- **Listings** show the plaintext size.
- **Copies.** A copy is encrypted as its request says (or the bucket's default), never as its
  source was, as on S3. If that's the source's encryption, the copy shares the source's data and
  is free. Otherwise the data is copied inside object storage (S3 `CopyObject`, part by part
  beyond 5 GiB), never through roll; the copy's CRC32C must match the source's before it's
  committed. A copy to itself that changes the encryption is allowed.
- **HTTPS.** S3 refuses SSE-C over plain HTTP. roll doesn't enforce it (it usually sits behind a
  TLS-terminating proxy), but customer keys should only ever be sent over HTTPS.
- **SSE-KMS** needs AWS KMS behind the backing bucket: it's on by default for AWS S3 without a
  custom endpoint, and off (`NotImplemented`) for S3-compatible services and the local
  directory backend. `ROLL_S3_KMS=true|false` overrides it. `pnpm test:storage-aws` runs it end
  to end with the backing bucket on LocalStack S3 and keys from LocalStack KMS: PUT, GET, HEAD,
  multipart, copies between encryptions, the bucket's default encryption with a bucket key, and
  every blob checked on the backing bucket itself, and `pnpm test:s3-conformance-localstack`
  runs the conformance suite's SSE-KMS tests that way.
- **S3-compatible backends.** Every encrypted write checks that the backing service confirmed the
  encryption. One that ignores SSE headers (or doesn't confirm them for an operation, as
  SeaweedFS does for multipart uploads and copies with SSE-C) gets `NotImplemented` for that
  operation, and nothing it stored for the request is kept: data is never stored unencrypted
  when encryption was asked for.
- **The control plane** (`/v1/storage`, the dashboard) can't read SSE-C objects: it has no key.

## Not supported

| feature | notes |
|---|---|
| ORC and Parquet inventory reports | `NotImplemented` when the configuration is put; CSV works |
| Storage Class Analysis, CloudWatch request metrics | The configurations are stored, nothing is produced yet. |
| Parquet input for S3 Select | `NotImplemented`: it needs random access and whole row groups in memory; CSV and JSON work |

## Directory buckets (S3 Express One Zone)

A bucket named `<base>--<zone id>--x-s3` is a directory bucket, as on S3. It's created with
`CreateBucket` and a `CreateBucketConfiguration` with `Location` (`Type` `AvailabilityZone` or
`LocalZone`, `Name` the zone id) and `Bucket` (`Type` `Directory`, `DataRedundancy`
`SingleAvailabilityZone` or `SingleLocalZone`). **Any zone id is accepted** (`use1-az4`,
`usw2-lax1-az1`, …): a deployment is one region in one place, and the zone id only names the
bucket. It must be the one in the name (`InvalidBucketName` otherwise). General purpose bucket
names can't end in `--x-s3`, as on S3.

| feature | status |
|---|---|
| CreateBucket, DeleteBucket, HeadBucket | ✅ |
| ListDirectoryBuckets | ✅ `GET /?x-id=ListDirectoryBuckets` (what SDKs send) or `GET /` on `s3express-control.<domain>`; `max-directory-buckets`, `continuation-token`. ListBuckets doesn't show directory buckets, as on S3 |
| Zonal endpoints | ✅ path style, or `<bucket>.s3express-<zone id>.<ROLL_S3_DOMAIN>` virtual-host style |
| CreateSession (`GET /<bucket>?session`) | ✅ `x-amz-create-session-mode` `ReadWrite` or `ReadOnly` (none: `ReadWrite` if allowed, else `ReadOnly`); credentials last 5 minutes, scoped to the bucket. Requests sign with the session's secret and send its token in `x-amz-s3session-token` (or a presigned URL's `X-Amz-S3session-Token`); the token works nowhere else, and other session tokens don't work there. SDKs do all this themselves |
| Object operations | ✅ PutObject (and appends), GetObject, HeadObject, DeleteObject(s), CopyObject, GetObjectAttributes, RenameObject, multipart uploads and UploadPartCopy, ListObjectsV2, ListMultipartUploads |
| Bucket policies | ✅ in S3 Express's namespace: data operations are `s3express:CreateSession` with `s3express:SessionMode` (`ReadOnly` for reads, `ReadWrite` for writes, or the session's own mode); bucket administration is `s3express:CreateBucket`, `DeleteBucket`, `Get/Put/DeleteBucketPolicy`, `Get/PutEncryptionConfiguration`, `Get/PutLifecycleConfiguration` and `Get/PutInventoryConfiguration`; the resource is `arn:aws:s3express:<region>:<account>:bucket/<bucket>` |
| Encryption, lifecycle and inventory configurations | ✅ |
| ListObjectsV2 | ✅ `/` is the only delimiter (`InvalidArgument` otherwise). S3 returns directory buckets' keys in no particular order; roll's are in lexicographic order, which is one of them |
| Checksums | ✅ every object gets a CRC checksum (the client's, or CRC64NVME) |
| Multipart uploads | ✅ part numbers must be consecutive from 1 (`InvalidPartOrder`) |
| Server-side encryption | ✅ SSE-S3 and SSE-KMS; SSE-C is `InvalidRequest` and DSSE-KMS `InvalidArgument`, as on S3 |
| Versioning, Object Lock, ACLs, CORS, website, logging, replication, notifications, object and bucket tagging, accelerate, request payment, analytics, metrics, intelligent-tiering, ownership controls, public access block, ListObjects (v1), ListObjectVersions, S3 Select, torrents | `NotImplemented`, as S3 answers for directory buckets |

**Underneath, a directory bucket is a roll bucket:** one object is one blob, overwrites and
deletes go to the bucket's trash (and can be restored), and it can be branched. A branch of a
directory bucket is a directory bucket (named like one); only directory buckets have directory
branches. Prefixes act like directories: a "directory" is there while an object is under it.

**Differences from AWS:**
- A `ReadWrite` session does what the key that created it may do, on its bucket only. S3
  limits sessions to object operations, but SDKs given a custom endpoint (`endpoint_url`) sign
  CreateBucket, DeleteBucket and bucket policy calls with a session too. For the same reason an
  admin key may create a session for a directory bucket that doesn't exist yet (the SDK asks for
  one before CreateBucket). `ReadOnly` sessions only read.
- Keys can also sign directory bucket requests themselves (plain SigV4, with the service
  `s3express` or `s3`); the key's permission and the bucket policy decide, as for any bucket.
  SDKs do this with `disable_s3_express_session_auth`, which ListDirectoryBuckets needs when the
  SDK has a custom endpoint (its session auth needs a bucket).
- CreateSession's encryption headers are checked (`AES256` or `aws:kms`), but new objects take
  the request's or the bucket's default encryption, as in general purpose buckets.
- ETags of single PUTs are MD5s, as in general purpose buckets (S3's are random strings for
  directory buckets; both are opaque to clients).

### Appends

`PutObject` with `x-amz-write-offset-bytes`, in directory buckets: the offset must be the
object's size (`InvalidWriteOffset` otherwise), and 0 creates an object that doesn't exist. The
response has `x-amz-object-size`. Each append is one more part (at most 10 000,
`TooManyParts`), so `GET ?partNumber=` reads one appended piece. The object keeps its metadata
and encryption (`EncryptionTypeMismatch` if the request asks for another one). An object with a
full-object CRC checksum keeps one: the old value combined with the new data's. The offset is
checked again inside the commit, so concurrent appends at the same offset can't both succeed:
the loser gets `InvalidWriteOffset`, or `ConditionalRequestConflict` if the object was replaced
by another of the same size meanwhile.

**How (reliability first).** Objects are immutable blobs, so an append writes a new blob holding
the old bytes followed by the new ones, then commits it like any overwrite. The old blob is never
changed, and it goes to the trash. Above 5 MiB the old bytes are copied inside object storage (a
multipart upload whose first part is an `UploadPartCopy` of the old blob, checked against its
CRC32C). Below 5 MiB they're read and sent again with the new ones in one PUT, also checked. A
crash before the commit leaves the object as it was.

**Cost.** Each append is a backend multipart upload (4 requests) above 5 MiB, or a GET and one PUT
of at most 5 MiB more than the appended bytes below it. Overwritten versions stay in the trash
for `recover_days`, like any overwrite, and are billed as trash: many appends to a large object
within the recovery window keep that many old copies until they expire. Set `recover_days` to 0
on append-heavy directory buckets to keep only the current object.

### RenameObject

`PUT /<bucket>/<key>?renameObject` with `x-amz-rename-source` (`/<key>` or `<bucket>/<key>`, URL
encoded, in the same bucket). It's atomic and metadata only: the object's blob, ETag, checksum,
metadata and `Last-Modified` move to the new key in one transaction, whatever the size. Both
sides' conditions are checked inside that transaction:
- the source's: `x-amz-rename-source-if-match`, `-if-none-match`, `-if-modified-since` and
  `-if-unmodified-since`;
- the destination's: `If-Match`, `If-None-Match: *`, `If-Modified-Since` and
  `If-Unmodified-Since`.

A failed condition is `412 PreconditionFailed`, and a missing source `NoSuchKey`. An overwritten
destination goes to the trash. `x-amz-client-token` makes retries idempotent: tokens are
remembered for 10 minutes, in memory, and the same token for another rename is
`IdempotencyParameterMismatch`.

**Better than S3:** RenameObject works on general purpose buckets too (S3 only has it for
directory buckets). In a versioned bucket the destination gets a new version and the source a
delete marker, so the source's history stays.

## S3 Select

`SelectObjectContent` (`POST /<bucket>/<key>?select&select-type=2`) answers in S3's event stream:
`Records`, `Progress` when `RequestProgress` is enabled, `Cont`, `Stats` and `End`, with the
prelude and message CRCs. Errors after the stream started are error messages in it.

| feature | status |
|---|---|
| Input | ✅ CSV (`FileHeaderInfo` `USE`/`IGNORE`/`NONE`, `FieldDelimiter`, `RecordDelimiter`, `QuoteCharacter`, `QuoteEscapeCharacter`, `Comments`, `AllowQuotedRecordDelimiter`), JSON (`DOCUMENT`, `LINES`); `CompressionType` `NONE`, `GZIP`, `BZIP2` |
| Parquet input | ❌ `NotImplemented` |
| Output | ✅ CSV (`QuoteFields` `ALWAYS`/`ASNEEDED`, delimiters, quote and escape characters), JSON (`RecordDelimiter`) |
| `ScanRange` | ✅ uncompressed CSV and JSON Lines: the records that start in the range |
| SQL | ✅ S3 Select's dialect: `SELECT` with `*`, projections, `s._1`, `s."col"` and aliases; `FROM S3Object[*]…` paths; `WHERE` with comparisons, `AND`/`OR`/`NOT`, `LIKE … ESCAPE`, `BETWEEN`, `IN`, `IS [NOT] NULL` and arithmetic; `CASE`, `CAST`, `COALESCE`, `NULLIF`; `LOWER`, `UPPER`, `TRIM`, `SUBSTRING`, `CHAR_LENGTH`; `UTCNOW`, `DATE_ADD`, `DATE_DIFF`, `EXTRACT`, `TO_STRING`, `TO_TIMESTAMP`; `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`; `LIMIT` |
| Errors | ✅ S3 Select's codes (`ParseUnexpectedToken`, `CastFailed`, `InvalidColumnIndex`, `OverMaxRecordSize`, …) |
| SSE-C objects | ✅ with the key's headers |

Records and results are at most 1 MiB each (`OverMaxRecordSize`), as on S3, and a request's
memory is bounded whatever the object's size. Directory buckets don't have S3 Select, as on S3.
Requests are metered as class B, and the results as egress.

## Torrents

`GET /<bucket>/<key>?torrent` returns the object's BitTorrent metainfo file
(`application/x-bittorrent`). It's bencoded: `info` has `name` (the key's last segment),
`length`, `piece length` 262 144 and the SHA-1 `pieces`, plus `x-amz-bucket` and `x-amz-key` as
S3's had. `announce` is `ROLL_S3_TORRENT_ANNOUNCE` if set (none otherwise: trackerless). The
pieces come from reading the object once, and are kept by blob: a blob never changes, so later
requests for it (the same key, copies, branches, versions) read nothing. As on S3, objects over
5 GB and SSE-C objects have no torrent (`InvalidRequest`). roll serves the metainfo; seeding is
up to the clients (or a web seed).

## Accounts

A deployment holds any number of **accounts**, as AWS has accounts. Each has:

- a **12-digit account id**: ARNs (`arn:aws:iam::<account>:…`), `x-amz-expected-bucket-owner`,
  `aws:PrincipalAccount`, `aws:ResourceAccount`, `s3:ResourceAccount`, `GetCallerIdentity`;
- a **canonical user id** (64 hex characters): `Owner` elements in ListBuckets, listings, ACLs;
  ACL grants by `id=`;
- a **display name**, and an optional **email** address (ACL grants by `emailAddress=`).

The deployment's original owner is the **default account**: its canonical id is the canonical
owner id the deployment always had, and its account id is derived from it as before, so existing
ARNs, policies and expected-owner headers keep their meaning. Every existing bucket, key and role
belongs to it. Accounts are managed with `/v1/storage/accounts` (see
[architecture](architecture.md#control-plane-api-s3-control-v1storage)); creating one makes its
root user's key too.

- **Buckets** belong to the account that created them. Names are unique across the deployment
  (as S3's are across AWS): another account's name is `BucketAlreadyExists`, your own
  `BucketAlreadyOwnedByYou` (a no-op `200` in `us-east-1`). ListBuckets lists the caller's
  account's buckets.
- **Objects** have an owner account, decided by the bucket's Object Ownership exactly as on S3:
  `BucketOwnerEnforced` (the default) the bucket owner; `BucketOwnerPreferred` the bucket owner
  when written with `bucket-owner-full-control`, else the writer's account; `ObjectWriter` (or no
  ownership controls) the writer's account. With ACLs on, the object's owner decides who reads it
  and its ACL; the bucket owner's policy can deny but not grant on objects it doesn't own, and the
  bucket owner can always delete them, as on S3.
- **Cross-account access** works as on AWS: the other account's own side (its root user, or its
  user's or role's policies) *and* the bucket's policy or ACL must allow it.
- An account's **root user** is an account key with `admin` over every bucket (what
  `ROLL_S3_ACCESS_KEY` and the control plane's account creation make). Like AWS's root user it may
  do anything to its account's resources and call every IAM action; unlike AWS's it signs
  requests with access keys (it has no console password: the dashboard without a sign-in is the
  operator's, acting as an account's root user).
- **Account aliases** (`CreateAccountAlias`, `ListAccountAliases`, `DeleteAccountAlias`): one
  per account, unique across the deployment, as across AWS; IAM users sign in to the dashboard
  with the account's id or alias.

## IAM

The IAM query API answers on the S3 endpoint, like STS: point an SDK's `iam` endpoint
(`endpoint_url`, `AWS_ENDPOINT_URL_IAM`, or `AWS_ENDPOINT_URL`) at roll. `POST /` with
`Action=…&Version=2010-05-08`, SigV4 with service `iam` and any region; IAM's XML responses and
`<ErrorResponse>` errors with IAM's codes (`NoSuchEntity` 404, `EntityAlreadyExists` 409,
`MalformedPolicyDocument` 400, `LimitExceeded` 409, `DeleteConflict` 409, `ValidationError`,
`InvalidInput`, `AccessDenied` 403). Every call acts in the caller's account and needs
`iam:<Action>` on the entity's ARN (the root user may do everything). Federated users can't call
IAM, and `GetSessionToken` sessions only with MFA, as on AWS.

| feature | status |
|---|---|
| Users: `CreateUser`, `GetUser`, `ListUsers`, `UpdateUser`, `DeleteUser`, `TagUser`, `UntagUser`, `ListUserTags` | ✅ paths, tags; names unique regardless of case; `DeleteUser` needs keys, policies, groups and MFA devices gone (`DeleteConflict`) |
| Groups: `CreateGroup`, `GetGroup`, `ListGroups`, `DeleteGroup`, `AddUserToGroup`, `RemoveUserFromGroup`, `ListGroupsForUser` | ✅ |
| Access keys: `CreateAccessKey`, `ListAccessKeys`, `UpdateAccessKey` (`Active`/`Inactive`), `DeleteAccessKey`, `GetAccessKeyLastUsed` | ✅ roll keys owned by users (two per user); an inactive key is `InvalidAccessKeyId`; the root user's own keys without `UserName` |
| Inline policies: `Put/Get/List/Delete` `UserPolicy`, `GroupPolicy`, `RolePolicy` | ✅ documents returned URL-encoded, as IAM does; IAM's size quotas (2 048/5 120/10 240 characters per user/group/role) |
| Managed policies: `CreatePolicy`, `GetPolicy`, `ListPolicies`, `DeletePolicy`, `CreatePolicyVersion`, `GetPolicyVersion`, `ListPolicyVersions`, `SetDefaultPolicyVersion`, `DeletePolicyVersion`, `ListEntitiesForPolicy` | ✅ five versions; AWS's `AmazonS3FullAccess` and `AmazonS3ReadOnlyAccess` are built in |
| `Attach`/`Detach`/`ListAttached` `UserPolicies`, `GroupPolicies`, `RolePolicies` | ✅ ten per entity |
| Permissions boundaries: `Put/DeleteUserPermissionsBoundary`, `Put/DeleteRolePermissionsBoundary`, `PermissionsBoundary` on create | ✅ |
| Roles: `CreateRole`, `GetRole`, `ListRoles`, `UpdateRole`, `UpdateRoleDescription`, `UpdateAssumeRolePolicy`, `DeleteRole`, `TagRole`, `UntagRole`, `ListRoleTags` | ✅ trust policies (`AssumeRolePolicyDocument`) with `AWS` (accounts, users, roles) and `Federated` (OIDC and SAML providers) principals and conditions; control-plane roles show their trust as the equivalent document |
| OIDC providers: `CreateOpenIDConnectProvider`, `GetOpenIDConnectProvider`, `ListOpenIDConnectProviders`, `DeleteOpenIDConnectProvider` | ✅ `AssumeRoleWithWebIdentity` verifies tokens with the provider's discovery document and client ids; thumbprints are stored (the provider's TLS certificate is checked by the usual trust roots) |
| SAML providers: `CreateSAMLProvider`, `GetSAMLProvider`, `ListSAMLProviders`, `UpdateSAMLProvider`, `DeleteSAMLProvider`, `TagSAMLProvider`, `UntagSAMLProvider`, `ListSAMLProviderTags` | ✅ the IdP's metadata document (an `entityID` and RSA signing certificates; `validUntil` is `ValidUntil`); see [`AssumeRoleWithSAML`](#sts-temporary-credentials) |
| Login profiles: `CreateLoginProfile`, `GetLoginProfile`, `UpdateLoginProfile`, `DeleteLoginProfile`, `ChangePassword` | ✅ passwords hashed with argon2id, checked against the password policy on set and change (`PasswordPolicyViolation`), `PasswordResetRequired`; users sign in to the [dashboard](#dashboard-sign-in) with them |
| Password policy: `GetAccountPasswordPolicy`, `UpdateAccountPasswordPolicy`, `DeleteAccountPasswordPolicy` | ✅ every setting (length, character kinds, `AllowUsersToChangePassword`, `MaxPasswordAge`, `PasswordReusePrevention`, `HardExpiry`); without one, AWS's default policy |
| Account aliases: `CreateAccountAlias`, `ListAccountAliases`, `DeleteAccountAlias` | ✅ one per account, unique across the deployment |
| Server certificates: `UploadServerCertificate`, `GetServerCertificate`, `ListServerCertificates`, `UpdateServerCertificate`, `DeleteServerCertificate`, `Tag`/`Untag`/`ListServerCertificateTags` | ✅ the certificate must be valid and its private key (RSA or EC; PKCS#1, PKCS#8, SEC1) must match (`KeyPairMismatch`); twenty per account |
| Signing certificates: `UploadSigningCertificate`, `ListSigningCertificates`, `UpdateSigningCertificate`, `DeleteSigningCertificate` | ✅ users' and (without `UserName`) the root user's; two per user, no duplicates in the account |
| SSH public keys: `UploadSSHPublicKey`, `GetSSHPublicKey` (`SSH` or `PEM`), `ListSSHPublicKeys`, `UpdateSSHPublicKey`, `DeleteSSHPublicKey` | ✅ RSA, 2048-16384 bits, five per user; MD5 fingerprints |
| Policy simulation: `SimulatePrincipalPolicy`, `SimulateCustomPolicy`, `GetContextKeysForPrincipalPolicy`, `GetContextKeysForCustomPolicy` | ✅ on the evaluator requests are decided with: `PolicyInputList`, `PermissionsBoundaryPolicyInputList`, `ResourcePolicy`, `ResourceOwner`, `CallerArn`, `ContextEntries`; `EvaluationResults` with `MatchedStatements` (positions in the policy's text), `MissingContextValues`, `PermissionsBoundaryDecisionDetail`, `EvalDecisionDetails` |
| `GetAccountAuthorizationDetails` | ✅ users, groups, roles, the account's policies and AWS's that are in use, with their documents; `Filter`, paging |
| Credential report: `GenerateCredentialReport`, `GetCredentialReport` | ✅ AWS's CSV (root and users: passwords, MFA, access keys, signing certificates); a report is current for four hours (`ReportExpired`) |
| Virtual MFA devices | ✅ see [MFA](#mfa-and-mfa-delete) |
| `GetAccountSummary` | ✅ |
| Service-linked roles, instance profiles, access advisor (`GenerateServiceLastAccessedDetails`, …), organizations' SCPs | ❌ `InvalidAction`: they belong to AWS services roll doesn't have |

**Principals in policies are checked**, as AWS checks them: a bucket policy, an access point
policy or a role's trust policy whose principal ARN names a user, role or account that doesn't
exist (or an IAM ARN that can't be a principal, or an OIDC or SAML provider that doesn't exist)
is `MalformedPolicy` / `MalformedPolicyDocument`, `Invalid principal in policy`. A principal
whose user or role is deleted afterwards reads as its unique id (`AIDA…`, `AROA…`) in
`GetBucketPolicy` and `GetRole`, and matches nobody, so a new user or role of the same name
doesn't inherit the old one's access; a renamed user reads with its new ARN.

### Dashboard sign-in

IAM users with a login profile sign in to the roll dashboard (`/dashboard/storage/-/signin`) with
the account's id or alias, their user name and password, and a code of their MFA device if they
have one, as at the AWS console's IAM-user sign-in. A password that must be reset (or has
expired) is changed there. The storage UI then acts as the user: every operation is authorized
by the user's IAM permissions, bucket policies and ACLs, exactly as the S3 request it stands
for. The IAM pages call IAM as the user. Ten wrong passwords in five minutes lock a user's
sign-in for the rest of them. See [architecture](architecture.md#dashboard-sign-in).

**Evaluation** is AWS's
([policy evaluation logic](https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_policies_evaluation-logic.html),
[for S3](https://docs.aws.amazon.com/AmazonS3/latest/userguide/how-s3-evaluates-access-control.html)):

1. An explicit `Deny` in any policy refuses: identity policies, permissions boundary, session
   policies, the bucket policy.
2. The requester's own side: its identity policies (a user's own and its groups', or a role's)
   allow, *bounded* by its permissions boundary and session policies (each must allow too).
3. Same account: its own side allows, **or** a resource grant (bucket policy, ACL) names it
   directly (`*`, its user or session ARN: not bounded), or names its role (bounded by the
   session's policies and boundary). A grant naming only the account delegates to the
   account's own policies.
4. Another account: its own side **and** a resource grant must allow.

Policies support every condition operator and key listed under [Policies](#policies-acls-and-public-access),
`aws:PrincipalTag/<key>` from users' tags, and policy variables (`${aws:username}`,
`${aws:userid}`, `${aws:PrincipalTag/team}`, `${key, 'default'}`) in resources and condition
values of `2012-10-17` policies. Service control policies don't exist (there are no
organizations).

**roll permission levels** (account keys created with `/v1/storage/keys` or `roll storage keys`,
and control-plane roles) keep working as a compatibility layer, each equivalent to an attached
identity policy on the key's buckets (all of them without a scope):

| level | equivalent policy |
|---|---|
| `read` | `s3:Get*`, `s3:List*` (reads, listings, reading bucket configuration) |
| `write` | `read`, plus `s3:PutObject*`, `s3:DeleteObject*`, `s3:AbortMultipartUpload`, `s3:RestoreObject`, retention and legal holds |
| `admin` | `s3:*` (with every bucket: the account's root user, including IAM) |

A level grants S3 only, never `iam:` or `sts:` actions (except the root user). A key scoped to
buckets learns nothing about other buckets (`AccessDenied` rather than `NoSuchBucket`), as
before.

## MFA and MFA Delete

- **Virtual MFA devices** (TOTP, RFC 6238: SHA-1, 30-second steps, six digits):
  `CreateVirtualMFADevice` returns `Base32StringSeed` and a `QRCodePNG` (an
  `otpauth://totp/roll:<user>@<account>` code for authenticator apps); `EnableMFADevice` takes two
  consecutive codes; `ResyncMFADevice`, `DeactivateMFADevice`, `ListMFADevices`,
  `ListVirtualMFADevices`, `DeleteVirtualMFADevice` (a device in use is `DeleteConflict`). Without
  `UserName`, `EnableMFADevice` enables the device for the caller, so the root user can have one
  through the API (on AWS that's the console's job).
- **Codes work once.** A code is accepted for the current 30-second step or the one either side
  of it, and only if its step is newer than the last one accepted for the device, so a code can't
  be replayed; the accepted step is stored durably before the request goes on. After 10 wrong
  codes within 5 minutes a device refuses every code for the rest of those 5 minutes, so codes
  can't be guessed. The count is kept in the metadata, so the lock survives a restart and holds
  across a takeover.
- **MFA-authenticated sessions:** `GetSessionToken` and `AssumeRole` take `SerialNumber` and
  `TokenCode` (the caller's own device); their sessions set `aws:MultiFactorAuthPresent` (`true`)
  and `aws:MultiFactorAuthAge`. Other sessions set `aws:MultiFactorAuthPresent` to `false`;
  long-term keys set neither, as on AWS.
- **MFA Delete:** `PutBucketVersioning` with `MfaDelete=Enabled` (or back to `Disabled`) takes the
  bucket owner's root user and `x-amz-mfa: <serial> <code>` of its own device, as on S3. While it's
  on, permanently deleting a version (`DeleteObject` or `DeleteObjects` with a version id) and any
  `PutBucketVersioning` need `x-amz-mfa` too, from the bucket owner's account; missing or wrong is
  `403 AccessDenied`. `GetBucketVersioning` shows `MfaDelete`. The control plane refuses to delete
  versions of such a bucket or change its versioning (that takes the S3 API with `x-amz-mfa`).
  Lifecycle expiration isn't affected (expired versions go to the trash).

## Requester Pays

`PutBucketRequestPayment` with `Payer=Requester` makes requests from **other accounts** pay
their own request and data transfer charges, as on S3:

- they must send `x-amz-request-payer: requester` (a header, or the query parameter of a
  presigned URL's signed headers), else `403 AccessDenied`; anonymous requests are refused;
- responses to them carry `x-amz-request-charged: requester`;
- their requests (class A and B) and egress are metered to the requester's account
  (`usage_payer`), not to the bucket; storage stays the bucket owner's;
- the bucket owner's own account's requests are billed to the owner, as on S3.

The control plane's usage shows both sides per account (`as_requester`, `billed_to_requesters`;
`GET /v1/storage/usage?accounts=true` for every account).

## STS (temporary credentials)

The STS query API answers on the S3 endpoint: point an SDK's `sts` endpoint (`endpoint_url`,
`AWS_ENDPOINT_URL_STS`, or `AWS_ENDPOINT_URL` for every service) at roll. `POST /` (or `GET`)
with `Action=…&Version=2011-06-15`, SigV4 with service `sts` and any region; STS's XML
responses and `<ErrorResponse>` errors.

| feature | status |
|---|---|
| `GetSessionToken` | ✅ users and account keys 900 s–36 h (default 12 h), the root user 900 s–1 h; `SerialNumber`/`TokenCode` for MFA; the session acts as its key |
| `AssumeRole` | ✅ a role's trust policy (accounts, users, roles; conditions such as `sts:ExternalId`, `aws:MultiFactorAuthPresent`, `aws:PrincipalTag`), or a control-plane role's trust (roll keys, an external id); another account's role needs the caller's own `sts:AssumeRole` permission too; `DurationSeconds` 900 s up to the role's maximum (≤ 12 h) |
| Role chaining (a role session calling `AssumeRole`) | ✅ at most an hour (longer is `ValidationError`, as on AWS); transitive session tags and the source identity carry over |
| `AssumeRole` `Policy`, `PolicyArns` (session policies) | ✅ inline (2 KB) and managed (up to 10, AWS's or the account's): they bound the role's identity policies, never widen them |
| `AssumeRole` `Tags`, `TransitiveTagKeys`, `ExternalId`, `SourceIdentity`, `SerialNumber`/`TokenCode` | ✅ tags need `sts:TagSession` in a trust policy |
| `AssumeRoleWithWebIdentity` | ✅ OIDC ID tokens (RS256, ES256) from providers in the role's trust: an IAM OIDC provider named by a `Federated` principal (with conditions on `<provider>:sub`/`:aud`), or a control-plane role's issuer list; Google, Auth0, Clerk, Cognito, GitHub Actions, Kubernetes service accounts, …; tags from `https://aws.amazon.com/tags` |
| `GetFederationToken` | ✅ `Name`, `Policy`, `PolicyArns`, `Tags`, 900 s–36 h (root: 1 h); the caller's permissions bounded by the session policies (none: no permissions of its own); `arn:aws:sts::<account>:federated-user/<name>` |
| `GetCallerIdentity` | ✅ the root user is `arn:aws:iam::<account>:root`, an account key `arn:aws:iam::<account>:user/<key id>` |
| `x-amz-security-token` header, `X-Amz-Security-Token` in presigned URLs and POST forms | ✅ SigV4, SigV4a and SigV2 |
| Errors | ✅ S3: `InvalidToken` (a malformed, tampered or retired token), `ExpiredToken`, `InvalidAccessKeyId` (temporary key without its token), `AccessDenied` (a revoked session, or one whose role is gone, as on AWS); STS: `AccessDenied`, `ExpiredToken`, `ExpiredTokenException`, `InvalidIdentityToken`, `IDPCommunicationError`, `MalformedPolicyDocument`, `PackedPolicyTooLarge`, `ValidationError`, `InvalidParameterValue`, `InvalidClientTokenId` |
| `AssumeRoleWithSAML` | ✅ SAML 2.0 responses signed by the IdP of an IAM SAML provider (enveloped XML signature, exclusive canonicalization, RSA-SHA256 or RSA-SHA1, verified with the metadata's certificates; wrapping attacks refused); `Issuer`, `Audience` and `Recipient` (`https://signin.aws.amazon.com/saml`), `NotBefore`/`NotOnOrAfter`, the `Role` attribute (role and provider ARNs), `RoleSessionName`, `PrincipalTag:*`, `TransitiveTagKeys`, `SourceIdentity`; `DurationSeconds` up to the role's maximum and `SessionNotOnOrAfter`; trust policy conditions on `saml:aud`, `saml:iss`, `saml:sub`, `saml:sub_type`, `saml:namequalifier` and the eduPerson/X.500 attribute keys, and `${saml:sub}` in the role's policies |
| `DecodeAuthorizationMessage`, `GetAccessKeyInfo`, `AssumeRoot` | ❌ `InvalidAction` |
| `RegionDisabledException` | never: every region is accepted |

**Sessions act with their role as it is now**, as on AWS: changing a role's policies (through
IAM or `PATCH /v1/storage/roles`) changes what its sessions may do from the next request on;
deleting a role leaves them with nothing (`AccessDenied`), and a new role of the same name is a
different role. **Revocation** is AWS's: `roll storage roles revoke` (or
`POST /v1/storage/roles/{name}/revoke`) adds the `AWSRevokeOlderSessions` inline policy, a
`Deny` of everything for `aws:TokenIssueTime` before now, so older sessions get `AccessDenied` and
new ones work; a deny on `aws:TokenIssueTime` you write yourself works the same way.

**Control-plane roles** (`/v1/storage/roles`, `roll storage roles …`) are IAM roles too, with
roll's extensions: a trust listing roll keys (and OIDC issuers), a permission level and bucket
scope (an attached policy, in effect), and a `policy` that bounds their sessions like a
permissions boundary. Through IAM they show their trust as the equivalent trust policy
(`UpdateAssumeRolePolicy` replaces it with a real one).

**Better than S3:**
- A backend can mint a role's credentials without any STS client:
  `POST /v1/storage/roles/{name}/sessions` (or `roll storage sessions create <role> --duration
  15m --bucket uploads`), narrowed to fewer buckets, a lower permission, a session policy and
  tags, and as short as 60 s.
- Browser apps can trade their users' Auth0, Clerk or Google ID tokens for scoped credentials
  directly (`AssumeRoleWithWebIdentity` is unsigned and answers CORS), with no signing key on
  the client.

**Differences from AWS** (each on purpose):
- A `GetSessionToken` or `GetFederationToken` session has no permissions once the key it was
  made with is deleted or deactivated (AWS keeps such sessions until they expire): taking a key
  away takes its sessions away.
- The root user may assume a control-plane role whose trust lists its key (a roll extension); a
  role with an IAM trust policy refuses the root user, as on AWS.
- Session tokens sealed with a session-token key retired by two rotations are `InvalidToken`.
- SAML: encrypted assertions (`EncryptedAssertion`, `AssertionEncryptionMode=Required`, provider
  private keys) aren't supported; signatures must use exclusive canonicalization (all current
  IdPs do) and RSA. Besides AWS's sign-in endpoints, a deployment may accept its own
  (`ROLL_S3_SAML_AUDIENCES`).
- IAM: a server certificate's private key is checked against the certificate and not kept (IAM
  never returns it, and nothing in roll serves TLS with it). `CanonicalUser` principals aren't
  checked for existence. `ChangePassword` with a wrong old password is `AccessDenied`.
  `CreateAccountAlias` refuses 12-digit aliases (they'd read as account ids at sign-in).

## Policies, ACLs and public access

**Ids.**
- Every account has a 12-digit **account id** and a 64-hex-char **canonical user id** (see
  [Accounts](#accounts)); `Owner` elements show the owning account's canonical id and display
  name.
- Each roll account key also has a **canonical user id**:
  `sha256("roll canonical user\0<default account's canonical id>\0<access key id>")` in hex. ACLs
  accept it (or the access key id itself) to grant one key.
- `GET /v1/storage` shows the account's `owner_id` (canonical id) and `account_id`.

**Bucket policy principals.**
- `"*"` and `{"AWS": "*"}` mean anyone, including anonymous requests.
- `{"AWS": "<account id>"}`, `{"AWS": "arn:aws:iam::<account id>:root"}` and
  `{"CanonicalUser": "<account canonical id>"}` mean that account: its root user, and its users
  and roles whose own policies allow it (delegation, as on AWS).
- `{"AWS": "arn:aws:iam::<account id>:user/<path><name>"}` means an IAM user;
  `{"AWS": "<access key id>"}` and `arn:aws:iam::<account id>:user/<access key id>` a roll account
  key; `{"CanonicalUser": …}` with a key's canonical id that key.
- `{"AWS": "arn:aws:iam::<account id>:role/<path><name>"}` means any session of that role (bounded
  by its session policies), `{"AWS": "arn:aws:sts::<account id>:assumed-role/<name>/<session>"}`
  one session (not bounded), `arn:aws:sts::<account id>:federated-user/<name>` a federated user.
  A `GetSessionToken` session is its key.
- `Service` principals are accepted and never match; `Federated` principals match web identities
  and SAML providers in trust policies.
- `{"App": "<role>"}` principals name callers of the [developer API](api.md#rules-bucket-policies)
  and never match an S3 request (see
  [below](#developer-api-limits-and-rules-seen-from-s3)).
- Each one must exist when the policy is put (`Invalid principal in policy`); one deleted later
  reads as its unique id (see [IAM](#iam)).

**What policies support.**
- `Version` `2012-10-17` or `2008-10-17`; `Statement` as one object or an array; `Sid`.
- `Effect`; `Principal`/`NotPrincipal` (`NotPrincipal` only with `Deny`, as on AWS);
  `Action`/`NotAction` with `s3:` wildcards; `Resource`/`NotResource` with ARN wildcards (they
  must name the bucket); policy variables in `2012-10-17` policies.
- Every IAM condition operator (`String*`, `Numeric*`, `Date*`, `Bool`, `IpAddress`,
  `NotIpAddress`, `Arn*`, `Null`), with `…IfExists`, `ForAnyValue:` and `ForAllValues:`.
- Condition keys:
  - `aws:SourceIp` (the peer address), `aws:SecureTransport` (only `X-Forwarded-Proto: https`
    from an internal proxy counts; roll itself speaks HTTP), `aws:CurrentTime`, `aws:EpochTime`,
    `aws:Referer`, `aws:UserAgent`
  - `aws:PrincipalArn`, `aws:PrincipalAccount`, `aws:PrincipalType` (`Account`, `User`,
    `AssumedRole`, `FederatedUser`, `Anonymous`), `aws:userid`, `aws:username`,
    `aws:PrincipalTag/<key>` (a user's tags, or a session's), `aws:ResourceAccount`,
    `s3:ResourceAccount`
  - temporary credentials: `aws:TokenIssueTime`, `aws:SourceIdentity`,
    `aws:MultiFactorAuthPresent`, `aws:MultiFactorAuthAge`, `aws:FederatedProvider`,
    `<provider>:sub` and `<provider>:aud`; for role sessions `aws:PrincipalArn` is the role's
    ARN and `aws:userid` is `<role id>:<session>`, as on AWS
  - `s3:prefix`, `s3:delimiter`, `s3:max-keys`, `s3:VersionId`, `s3:signatureversion`, `s3:authType`
  - every `s3:x-amz-*` request header (`s3:x-amz-acl`, `s3:x-amz-server-side-encryption`, …)
  - `s3:ExistingObjectTag/<key>`, `s3:RequestObjectTag/<key>`, `s3:RequestObjectTagKeys`
- Missing keys make a condition false, except for negated operators and `…IfExists`, as in IAM.
- Policies over 20 KB, unknown actions, resources outside the bucket and unknown operators are
  `MalformedPolicy`.

**Evaluation** is AWS's (see [IAM](#iam)). The bucket owner's root user (and its `admin` keys
covering the bucket) can always read, replace and delete the bucket policy, so a policy can't
lock its owner out; and, as on S3, only the bucket owner's account may (another account allowed
by a policy gets `405 MethodNotAllowed`).

**ACLs.** With `ObjectOwnership` `BucketOwnerPreferred` or `ObjectWriter` (set by
`PutBucketOwnershipControls`, or `x-amz-object-ownership` on CreateBucket), ACLs work and are
enforced: canned ACLs (`bucket-owner-full-control` and `bucket-owner-read` grant the bucket
owner), `x-amz-grant-*` and `AccessControlPolicy` bodies, with the `AllUsers`,
`AuthenticatedUsers` and `LogDelivery` groups, accounts by canonical id or email address
(`UnresolvableGrantByEmailAddress` for an unknown one), and roll keys. A grant to an account is a
resource grant to it (its users need their own policies to allow too). With
`BucketOwnerEnforced`, writes may only carry `bucket-owner-full-control` (or grants to the bucket
owner, or `private` from its own account): anything else is `AccessControlListNotSupported`.
Copies don't take the source's ACL, as on S3.

**Public access block.** The bucket-level settings apply as on S3:
- `BlockPublicAcls` refuses public ACLs.
- `IgnorePublicAcls` ignores public grants.
- `BlockPublicPolicy` refuses public policies and turning the `public` flag on.
- `RestrictPublicBuckets` makes a public policy, and the `public` flag, apply only to the bucket owner's account (as on S3: not to anonymous requests or other accounts).

The `public` flag counts as a public policy. `ROLL_S3_BLOCK_PUBLIC_ACCESS=true` turns on all
four for every bucket of the deployment, on top of the bucket's own (like S3's account-level
block); `GetPublicAccessBlock` still shows the bucket's own settings.

**Website endpoint.** With `ROLL_S3_WEBSITE_DOMAIN=web.example.com`,
`http://<bucket>.web.example.com/` serves the bucket's website configuration:
- `GET` and `HEAD` only, always anonymous; the bucket must be public through its policy, ACLs or
  `public` flag.
- Index documents for `/` and `dir/`, and a `302` from `dir` to `dir/` when that has an index.
- `ErrorDocument` with the error's status, `RoutingRules` (`KeyPrefixEquals`,
  `HttpErrorCodeReturnedEquals`; `HostName`, `Protocol`, `ReplaceKeyWith`,
  `ReplaceKeyPrefixWith`, `HttpRedirectCode`), `RedirectAllRequestsTo`.
- Objects' `x-amz-website-redirect-location` answers a `301`.
- S3's HTML error pages.
- One difference: a missing key is a `404` even without `s3:ListBucket` (S3 answers `403`).

**Access logs.** Requests to a bucket with logging on are buffered in memory and written to the
target bucket every 5 minutes. Keys are `SimplePrefix` (`<prefix>YYYY-mm-DD-HH-MM-SS-<id>`) or
`PartitionedPrefix` (`<prefix><account>/<region>/<bucket>/YYYY/MM/DD/…`). Lines use S3's server
access log format. Like S3's, delivery is best effort: lines in memory when the process stops are
lost. The objects are ordinary writes, billed to the target bucket. `TargetGrants` are stored and
returned but not applied. The target bucket must belong to the source bucket's account (`InvalidTargetBucketForLogging` otherwise), as on S3.

## Event notifications

`PutBucketNotificationConfiguration` takes S3's full XML: `TopicConfiguration`,
`QueueConfiguration`, `CloudFunctionConfiguration` and `EventBridgeConfiguration`, `Event` lists
with wildcards (`s3:ObjectCreated:*`), `Filter`/`S3Key`/`FilterRule` `prefix` and `suffix`, and
`Id` (generated when missing). S3's rules apply: unknown event types, two rules of the same
name, and configurations that overlap (a shared event type with overlapping prefix and suffix
filters) are `InvalidArgument`. An empty configuration removes it.

**Events.** Every change S3 notifies about records its event in the same metadata transaction as
the change, so an event exists if and only if the write was acknowledged:

| event | when |
|---|---|
| `s3:ObjectCreated:Put`, `:Post`, `:Copy`, `:CompleteMultipartUpload` | PutObject (and control-plane uploads), POST Object, CopyObject, CompleteMultipartUpload |
| `s3:ObjectRemoved:Delete`, `:DeleteMarkerCreated` | DeleteObject(s) of an object or a version; a new delete marker |
| `s3:LifecycleExpiration:Delete`, `:DeleteMarkerCreated` | lifecycle expiration (current, noncurrent, expired delete markers) |
| `s3:ObjectTagging:Put`, `:Delete` | Put/DeleteObjectTagging |
| `s3:ObjectAcl:Put` | PutObjectAcl |
| `s3:ObjectRestore:Completed` | **roll's trash restore** (`POST /v1/storage/buckets/{name}/trash/restore`): the version is readable again. There's no `s3:ObjectRestore:Post` (nothing is archived, so RestoreObject never starts a restore), and the restore never expires, so `glacierEventData` has no `lifecycleRestorationExpiryTime`. |
| `s3:Replication:OperationFailedReplication` | a version (or a change to its metadata) couldn't be replicated (see [Replication](#replication)). S3 only sends it for rules with `Metrics` enabled; roll sends it for every rule. |
| `s3:Replication:OperationMissedThreshold`, `:OperationReplicatedAfterThreshold` | with Replication Time Control: a version not replicated within 15 minutes, and when it then is |
| `s3:TestEvent` | sent to each destination when a configuration is put (unless validation is skipped) |

Records are S3's JSON: `Records[]` with `eventVersion` (2.1; 2.2 for replication events; 2.3
for lifecycle, tagging and ACL events, as S3 documents), `eventSource` `aws:s3`, `awsRegion`,
`eventTime`, `eventName` without `s3:`, `userIdentity.principalId` (the access key id, or
`anonymous`; `roll` for changes outside a request, `roll:replication` for replicas),
`requestParameters.sourceIPAddress`, `responseElements` (`x-amz-request-id`, `x-amz-id-2`),
`s3.configurationId`, `s3.bucket.name`/`ownerIdentity`/`arn`, and `s3.object.key` (URL-encoded
like S3's: `red flower.jpg` is `red+flower.jpg`), `size` and `eTag` (not for removals),
`versionId` (versioned buckets) and `sequencer`. Sequencers are 18 hex digits that grow with
every change, so a key's events order by comparing them as strings.

**Destinations**, by ARN:

| ARN | delivery |
|---|---|
| `arn:roll:queues:<vhost>:<queue>` | published into the roll queue (persistent, `content-type: application/json`, `type` = the event name, `message-id` = a delivery id), confirmed before it counts. `<vhost>` may be percent-encoded (`%2F` for `/`). |
| `arn:roll:streams:<stream>:<topic>` | produced to the roll stream's topic (Kafka), keyed by the object key, so a key's events stay in one partition in order; headers `s3-event`, `s3-bucket`, `delivery-id`; durable (`acks=all`) before it counts |
| `arn:roll:webhook:<url>` | an HTTP(S) POST of the JSON, signed: `X-Roll-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256(secret, "<t>.<body>")>`, plus `X-Roll-Event` and `X-Roll-Delivery` (stable across retries). The secret is per bucket, created with the first webhook, shown (and rotated) in the control plane. Redirects aren't followed. Loopback, private, link-local (cloud metadata) and CGNAT addresses are refused unless `ROLL_S3_WEBHOOK_ALLOW_PRIVATE` is set. Any 2xx counts as delivered. |
| `arn:aws:sqs:…`, `arn:aws:sns:…`, `arn:aws:lambda:…`, `EventBridgeConfiguration` | real AWS, SigV4-signed with the deployment's AWS credentials (the default chain), when `ROLL_S3_NOTIFY_AWS` is on: SQS `SendMessage`, SNS `Publish` (subject `Amazon S3 Notification`), Lambda `Invoke` (`Event`), EventBridge `PutEvents` on the default bus in the deployment's region (source `roll.s3`, since `aws.s3` is reserved for AWS). Otherwise these fail destination validation. `ROLL_S3_NOTIFY_AWS_ENDPOINT` sends every service's calls to another endpoint (a VPC endpoint, LocalStack), `ROLL_S3_NOTIFY_AWS_ENDPOINTS` (`sqs=<url>,sns=<url>,lambda=<url>,events=<url>`) one per service; `pnpm test:storage-aws` delivers to each of the four on LocalStack, test events and validation failures included. |

**Validation.** Unless `x-amz-skip-destination-validation: true`, each destination is checked
and sent an `s3:TestEvent` before the configuration is stored, as S3 does: a roll queue or stream
must exist, a webhook must answer 2xx, and AWS destinations need AWS delivery enabled. Failures
are S3's `InvalidArgument` *Unable to validate the following destination configurations*, with
`ArgumentName1`/`ArgumentValue1`… naming each ARN and why.

**Delivery** is at least once. A dispatcher hands out an event only once its write is durable,
retries failures with exponential backoff (1 s doubling to 10 min, with jitter), and keeps a
key's events for one destination in order (a later one waits while an earlier one is retried).
Events still failing after 3 days are dropped, counted and logged. Without any notification or
replication configuration on a bucket, nothing is recorded at all.

Branches don't inherit their parent's notification or replication configuration (or its webhook
secret); a branch can have its own.

## Replication

`PutBucketReplication` takes S3's `ReplicationConfiguration`: `Role` (accepted, returned, not
needed), and rules with `ID`, `Priority`, `Status`, `Filter` (`Prefix`, `Tag`, or `And`) or the
legacy rule `Prefix`, `DeleteMarkerReplication`, `Destination` (`Bucket` ARN, `StorageClass`,
`Account`, `AccessControlTranslation`, `EncryptionConfiguration`, `ReplicationTime`, `Metrics`),
`ExistingObjectReplication` and `SourceSelectionCriteria` (`SseKmsEncryptedObjects`,
`ReplicaModifications`). As on S3, the source and every destination need versioning `Enabled`
(`InvalidRequest` otherwise), a source with Object Lock needs destinations with Object Lock
(`InvalidRequest`), priorities must be unique, V2 rules need `DeleteMarkerReplication`, the
destination can't be the source, `ReplicationTime` `Time` and `Metrics` `EventThreshold` take 15
minutes only (`InvalidArgument`), and `ReplicationTime` needs `Metrics` (`InvalidRequest`).

`arn:aws:s3:::<bucket>` names, in this order:
1. the bucket on the replication target named by roll's extension element
   `<RollReplicationTarget>name</RollReplicationTarget>` in the rule's `Destination` (or the
   control plane's `target`);
2. the bucket on the replication target whose `buckets` list it (so SDKs and the AWS CLI work
   unchanged);
3. a roll bucket of the same deployment.

Anything else is `InvalidRequest` (*Destination bucket must exist*), as on S3.

**What replicates**, wherever the destination is, as on S3:
- new versions and (with `DeleteMarkerReplication` `Enabled`, always for legacy `Prefix` rules)
  delete markers made by DeleteObject. Lifecycle delete markers and deletes of a version by id
  never replicate. Replicas keep the source's version id and `Last-Modified` (see below for
  services that assign their own ids);
- user metadata, content headers, tags, Object Lock retention and legal hold, and between roll
  buckets the ACL (unless `AccessControlTranslation` `Owner=Destination`);
- **later metadata changes** to a replicated version (tags, ACL, retention, legal hold): they
  replicate to its replicas, in order behind the version itself;
- **`ReplicaModifications` `Enabled`** on a rule: changes made to *replicas* in that bucket (tags,
  ACL, retention, legal hold) replicate to the rule's destination, which is how two-way
  replication keeps metadata in sync. A change made by replication itself never replicates
  again, so two buckets replicating to each other don't bounce;
- **SSE-KMS versions** only with `SourceSelectionCriteria` `SseKmsEncryptedObjects` `Enabled`
  (between roll buckets too); otherwise they're out of scope and get no status, as on S3.
  SSE-C versions only replicate between roll buckets.

Replicas aren't replicated again (no chains, no loops). GET and HEAD return
`x-amz-replication-status`: `PENDING`, `COMPLETED`, `FAILED` on sources, `REPLICA` on replicas.

**Replication Time Control** (`ReplicationTime` `Enabled`): a version not replicated within 15
minutes reports `s3:Replication:OperationMissedThreshold` (once), and
`s3:Replication:OperationReplicatedAfterThreshold` when it then completes, through the event
outbox to the bucket's notification configurations (with `replicationEventData`:
`replicationRuleId`, `destinationBucket`, `s3Operation`, `requestTime`; `failureReason` on
`OperationFailedReplication`). Replication runs as soon as a write is acknowledged (the
dispatcher wakes on every write, and looks every 250 ms anyway): tens of milliseconds between
roll deployments in the test suite (`replication_latency_is_measured` in `tests/targets.rs`).

**Replication metrics**, per rule, for replication inside and outside roll:
- in the control plane, `metrics` in each of the bucket's `replication.rules`:
  `operations_pending` (`OperationsPendingReplication`, metadata changes included),
  `bytes_pending` (`BytesPendingReplication`), `oldest_pending_ms`, `latency_ms` /
  `max_latency_ms` (`ReplicationLatency`, from the write to the replica), `replicated`,
  `operations_failed` (`OperationsFailedReplication`), `replicated_after_threshold`, and whether
  `Metrics` and Replication Time Control are `enabled`;
- in CloudWatch, for rules with `Metrics` (or RTC) enabled, when
  `ROLL_S3_REPLICATION_METRICS_CLOUDWATCH` is on (with `ROLL_S3_NOTIFY_AWS`; `ROLL_S3_NOTIFY_AWS_ENDPOINT`, or
  `monitoring=<url>` in `ROLL_S3_NOTIFY_AWS_ENDPOINTS`, for another endpoint): `PutMetricData`
  every minute with S3's metric names and units (`ReplicationLatency` in seconds: the minute's
  largest, or the oldest pending operation's age if larger; `BytesPendingReplication` in bytes;
  `OperationsPendingReplication` and `OperationsFailedReplication`, the minute's failures, as
  counts) and dimensions (`SourceBucket`, `DestinationBucket`, `RuleId`), in the `roll/S3`
  namespace: CloudWatch keeps `AWS/` namespaces for AWS's own services. `pnpm test:storage-aws`
  publishes them to LocalStack and reads them back.

### Replication between roll buckets

- **Metadata-only.** A replica shares the source version's blob (one more reference): it's
  instant whatever the size, and copies no bytes. It keeps the source's version id and
  `Last-Modified`, and takes its place in the destination's history by version id. Deleting the
  source version later doesn't affect the replica.
- **Asynchronous,** through the event outbox: the source version is `PENDING`, then `COMPLETED`
  once every destination has it (or `FAILED`, with an `s3:Replication:OperationFailedReplication`
  event, if a destination can't take it: it was deleted, its versioning was suspended, or a
  replica would replace a locked null version). A metadata change that can't be applied (a
  replica's COMPLIANCE retention can't be shortened, the replica is gone) sends the event and
  leaves the status as it was.
- **`ExistingObjectReplication` `Enabled`** replicates the versions already in the bucket (S3
  needs a support case or Batch Replication for that): a background walk queues every version in
  scope that has no replication status yet.
- `StorageClass` is stored but has no effect (roll has one storage class). A shared blob stays
  encrypted as the source's is: `ReplicaKmsKeyID` applies to targets outside roll.
- **Branches.** Replication *from* a branch replicates the branch's own writes (a branch doesn't
  inherit its parent's configuration). Replication *to* a branch writes the replica into the
  branch like any write: the branch's own branches keep their view (push-down), and the parent
  doesn't see it.
- `DeleteBucketReplication` stops new versions from replicating; ones already queued still go.

### Replication to buckets outside roll

A **replication target** is an S3 service roll can write to: AWS S3 in another account or region,
or any S3-compatible endpoint (MinIO, R2, another roll deployment). It's managed in the control
plane (`/v1/storage/replication-targets`): a name, an `endpoint` (none: AWS S3), a `region`,
credentials (an access key, or none to use the deployment's own AWS credentials), `path_style`,
and the `buckets` of that service that rules may name. The secret access key is write-only: it's
sealed with AES-256-GCM under a dedicated key in the metadata and never shown again.

- **Checked like S3 checks a destination:** when the configuration is put, the target must
  exist and the bucket there must have versioning enabled, and Object Lock when the source has
  it (asked of the target).
- **Through the same durable outbox** as everything else: the source version is `PENDING`, then
  `COMPLETED`, or `FAILED` with an `s3:Replication:OperationFailedReplication` event when the
  target refuses it (no such bucket, access denied). Network errors and 5xx are retried with
  backoff for up to 3 days, then the version is `FAILED`.
- **Version ids.** Every write of a replica carries roll's replication extension (below).
  **Another roll deployment** keeps the source's version id and `Last-Modified`, exactly as S3's
  replication does. **AWS S3** (and any other service) assigns its own: the public S3 API has no
  way to set a version id. Only S3's own replication, run by S3 for a source bucket in S3 (live
  replication, or S3 Batch Replication, which replicates with the source bucket's replication
  configuration and role), creates replicas with the source's version id, and a bucket on roll
  isn't a source S3 can replicate from. roll records the version the target made of each source
  version instead: `replicas` in `GET /v1/storage/buckets/{name}/object?key=…&version_id=…` lists
  each destination bucket, its target and the version id there, and later metadata changes are
  applied to that version.
- **The bytes move server-side when they can:** a target on the backing bucket's own service with
  the same credentials (no access key of its own, same endpoint) gets an S3 `CopyObject`
  (`UploadPartCopy` beyond 5 GiB) from the blob. Otherwise they're streamed from the blob to the
  target, as a multipart upload from 64 MiB (16 MiB parts, each read and retried on its own), in
  bounded memory. A multipart source is written part for part with its own part sizes, so the
  replica has the source's ETag.
- **Checksums end to end:** the blob's CRC32C (verified by S3 when it was written) goes with a
  single PUT as `x-amz-checksum-crc32c`, so the target refuses anything else; multipart parts
  each carry their CRC32C, and the parts' combined CRC32C must equal the blob's before the upload
  is completed; a server-side copy's CRC32C must equal it before the version counts as
  replicated.
- **What's applied on the target:** the data, `Content-Type`, `Cache-Control`,
  `Content-Disposition`, `Content-Encoding`, `Content-Language`, `Expires`, user metadata, tags,
  the website redirect, Object Lock retention (`x-amz-object-lock-mode`,
  `-retain-until-date`; one that has run out isn't sent) and legal hold
  (`x-amz-object-lock-legal-hold`); the rule's `StorageClass`; `AccessControlTranslation` as
  `x-amz-acl: bucket-owner-full-control`. Delete markers replicate as a `DeleteObject` (a delete
  marker on the versioned target). Later changes to tags, retention and legal hold are applied
  with PutObjectTagging/DeleteObjectTagging, PutObjectRetention and PutObjectLegalHold on the
  replica's version. ACL changes aren't (a target's ACL grants name its own accounts).
- **SSE-KMS versions** are encrypted on the target with the destination's
  `EncryptionConfiguration` `ReplicaKmsKeyID` (else the target's default KMS key), as on S3.
- **SSE-C versions** can't be replicated outside roll (roll never keeps customer keys): `FAILED`.
- **`ExistingObjectReplication`** backfills to targets like to roll buckets.

**roll's replication extension** (what a roll deployment accepts from another one's
replication): `x-roll-replica-version-id` (the source's version id, `null` for a null version)
and `x-roll-replica-last-modified` (Unix milliseconds) on PutObject, CreateMultipartUpload,
UploadPart, CompleteMultipartUpload (where the version is made) and DeleteObject (a delete
marker); `x-roll-replica-sync: true` on Put/DeleteObjectTagging, PutObjectRetention,
PutObjectLegalHold and PutObjectAcl (a replica kept in sync, not synced on). They're honoured
only on a signed request whose identity may replicate into the bucket: `s3:ReplicateObject`
(`s3:ReplicateDelete` for delete markers, `s3:ReplicateTags` for tags and metadata sync), the
permissions an AWS replication role needs, from an identity or bucket policy or a roll `admin`
key covering the bucket (a `write` key can't rewrite a bucket's history); such a request doesn't
also need `s3:PutObject`, as a replication role doesn't. Only versioning-enabled buckets take
them (`InvalidRequest`). A replica takes its place in the key's history by version id (an older
one arriving late becomes noncurrent); a version id that's there with other content is refused
(`InvalidRequest`), and the same content (a retry after a lost answer) is answered as the first
write was. The replica is `REPLICA` and never replicates on.

## Access points

Access points are named entry points to a bucket with their own policy, public access block and
network origin, managed with the S3 Control API on the S3 endpoint.

**S3 Control API.** SDKs and the AWS CLI with a custom endpoint send it to
`<account>.<endpoint host>` (the account id is a host prefix) with `x-amz-account-id`; roll
recognises `/v20180820/…` with that header on any host, and the account must be the
deployment's (`account_id` in `GET /v1/storage`). Point a wildcard DNS name at roll (or use
`<account>.s3-control.<domain>`). Errors are S3 Control's (`<ErrorResponse>`).

| operation | status |
|---|---|
| CreateAccessPoint | ✅ `Bucket`, `VpcConfiguration`, `PublicAccessBlockConfiguration` (all four on when omitted, as on S3), `BucketAccountId`; returns the ARN and the alias. Needs an `admin` key covering the bucket. |
| GetAccessPoint, ListAccessPoints, DeleteAccessPoint | ✅ `bucket`, `maxResults` and `nextToken` pages; `NetworkOrigin`, `Endpoints`, `CreationDate` |
| Put/Get/DeleteAccessPointPolicy, GetAccessPointPolicyStatus | ✅ resources must be the access point (`arn:aws:s3:<region>:<account>:accesspoint/<name>` and `…/object/<key>`); a public policy is refused while `BlockPublicPolicy` is on (the access point's, the bucket's or the deployment's) |
| `NetworkOrigin` `VPC` | ✅ accepted; such an access point only takes requests from inside roll's network (`ROLL_S3_INTERNAL_CIDRS`): roll can't see VPC ids, so the VPC is the deployment's network |
| Create/Get/Delete/ListAccessPointsForObjectLambda, Get/PutAccessPointConfigurationForObjectLambda, Put/Get/DeleteAccessPointPolicyForObjectLambda, GetAccessPointPolicyStatusForObjectLambda | ✅ see [Object Lambda](#object-lambda) |
| Every other S3 Control operation (Batch Operations, Storage Lens, Multi-Region Access Points, account Public Access Block, …) | `NotImplemented` |

**Requests through an access point** work on its bucket:
- by ARN: aws-sdk-s3, boto3 and the AWS CLI turn `arn:aws:s3:<region>:<account>:accesspoint/<name>`
  into the host `<name>-<account>.<endpoint host>` (with AWS's own domain,
  `<name>-<account>.s3-accesspoint.<region>.<domain>`); roll takes any host whose first label is
  `<name>-<account>` for one of its access points;
- by the ARN as the first path segment (`/arn:aws:s3:…:accesspoint/<name>/<key>`, encoded or not);
- by alias (`<name>-<random>-s3alias`) wherever a bucket name goes, path style or virtual-host.

Object operations, multipart uploads, DeleteObjects, ListObjects(V2), ListObjectVersions,
ListMultipartUploads, HeadBucket and GetBucketLocation work through access points; bucket
configuration doesn't (`InvalidRequest`), as on S3.

**Copy sources** of CopyObject and UploadPartCopy can name an access point, as on S3: its object
ARN, `x-amz-copy-source: arn:aws:s3:<region>:<account>:accesspoint/<name>/object/<key>`
(URL-encoded as AWS documents it, or with only the key encoded, as the AWS CLI sends it;
`?versionId=` as usual), or its alias as the bucket (`<alias>/<key>`). Reading the source is
authorized through that access point exactly as a GetObject through it is (its network origin,
its policy and the bucket's, below); an Object Lambda access point can't be a copy source
(`InvalidArgument`).

**Authorization** is AWS's for access points: a `Deny` in the access point policy or the bucket
policy refuses; the key's own permission on the bucket allows (an identity allow in the same
account); otherwise both the access point policy and the bucket policy must allow. A bucket
policy can delegate to its access points with `s3:DataAccessPointAccount` or
`s3:DataAccessPointArn` (and sees `s3:AccessPointNetworkOrigin`). A public grant doesn't apply
while the access point's or the bucket's block restricts public buckets. Difference: ACL grants
and the bucket's `public` flag don't apply through access points.

## Object Lambda

Object Lambda access points (`arn:aws:s3-object-lambda:<region>:<account>:accesspoint/<name>`,
alias `<name>-<random>--ol-s3`) transform `GetObject`, `HeadObject`, `ListObjects` and
`ListObjectsV2` answers with a function and pass every other request to their supporting access
point. Requests signed for `s3-object-lambda` are accepted on the S3 endpoint.

- **The configuration** is S3's: `SupportingAccessPoint`, `AllowedFeatures`
  (`GetObject-Range`, `GetObject-PartNumber`, `HeadObject-Range`, `HeadObject-PartNumber`;
  otherwise such requests are `InvalidRequest`), `CloudWatchMetricsEnabled` (stored), and
  `TransformationConfigurations` with `Actions` and `ContentTransformation.AwsLambda`
  (`FunctionArn`, `FunctionPayload`).
- **The function** is an AWS Lambda ARN, invoked synchronously (`RequestResponse`) through the
  notification path (`ROLL_S3_NOTIFY_AWS`, its endpoints and credentials), or roll's extension
  `arn:roll:webhook:<url>`: an HTTP POST of the same event, signed like notification webhooks
  with the bucket's webhook secret (`X-Roll-Signature`), under the same address rules.
- **The event** is S3's: `xAmzRequestId`, `getObjectContext` (`inputS3Url`, `outputRoute`,
  `outputToken`) or `headObjectContext`/`listObjectsContext`/`listObjectsV2Context`
  (`inputS3Url`), `configuration` (`accessPointArn`, `supportingAccessPointArn`, `payload`),
  `userRequest` (`url`, `headers` without credentials), `userIdentity`, `protocolVersion` `1.00`.
  `inputS3Url` is the original request through the supporting access point, presigned with the
  caller's own credentials (and session token) for 5 minutes, so the function reads exactly what
  the caller may. It's on `ROLL_S3_PUBLIC_ENDPOINT` (default `http://<public host>:<S3 port>`).
- **`GetObject`'s answer comes through `WriteGetObjectResponse`** (`POST /WriteGetObjectResponse`
  with `x-amz-request-route` and `x-amz-request-token`; SDKs need host prefixes off or a
  wildcard name for `<route>.<endpoint host>`): `StatusCode`, the `x-amz-fwd-header-*` headers,
  user metadata, `ErrorCode`/`ErrorMessage`, and the body, streamed to the waiting caller as it
  arrives. The token authorizes the call (a random 256-bit secret, valid once, while the caller
  waits, at most 60 s), not a signature, so the function needs no roll credentials; the body
  must be plain (not `aws-chunked`). `HeadObject` and `ListObjects` answers are the function's
  result: `statusCode`, `headers`, `metadata`, `errorCode`/`errorMessage`, and `listResultXml` or
  `listBucketResult`.
- **Errors** are S3's: `LambdaInvocationFailed`, `LambdaRuntimeError`, `LambdaTimeout`,
  `LambdaResponseNotReceived` (the function returned without calling `WriteGetObjectResponse`).
- **Authorization:** the Object Lambda access point's policy (`s3-object-lambda:*` actions) or the
  key's own permission on the bucket; pass-through requests are then authorized through the
  supporting access point. `pnpm test:storage-aws` runs a Python Lambda function on LocalStack
  that reads `inputS3Url` and answers through `WriteGetObjectResponse` with boto3.

## Inventory reports

Enabled inventory configurations produce reports on their `Schedule` (`Daily` or `Weekly`,
checked hourly; the first report comes within the hour), in S3's layout in the destination
bucket (`arn:aws:s3:::<roll bucket>`):

```text
<prefix>/<source-bucket>/<config-id>/data/<uuid>.csv.gz
<prefix>/<source-bucket>/<config-id>/<YYYY-MM-DDTHH-MMZ>/manifest.json
<prefix>/<source-bucket>/<config-id>/<YYYY-MM-DDTHH-MMZ>/manifest.checksum
<prefix>/<source-bucket>/<config-id>/hive/dt=<YYYY-MM-DD-HH-MM>/symlink.txt
```

- CSV only (ORC and Parquet are `NotImplemented` when the configuration is put), every field
  quoted, keys URL-encoded as S3 does. Columns: `Bucket`, `Key`, then `VersionId`, `IsLatest`
  and `IsDeleteMarker` with `IncludedObjectVersions` `All`, then the `OptionalFields` asked for,
  in S3's order.
- Filled fields: `Size`, `LastModifiedDate`, `ETag`, `StorageClass` (`STANDARD`, what the API
  reports), `IsMultipartUploaded`, `ReplicationStatus`, `EncryptionStatus` (`SSE-S3`),
  `ObjectLockRetainUntilDate`, `ObjectLockMode`, `ObjectLockLegalHoldStatus`, `BucketKeyStatus`
  (`DISABLED`), `ChecksumAlgorithm` and `ObjectOwner`. `IntelligentTieringAccessTier` and
  `ObjectAccessControlList` are empty.
- `Filter` `Prefix` applies. `manifest.json` has S3's fields (`sourceBucket`,
  `destinationBucket`, `version` `2016-11-30`, `creationTimestamp`, `fileFormat`, `fileSchema`,
  `files` with `key`, `size`, `MD5checksum`); `manifest.checksum` is the manifest's MD5.
- Files are written through the normal put path: durable once written, billed to the destination
  bucket, and they trigger the destination's own notifications. Data files are split every
  16 MiB compressed. A report that fails is retried at the next hourly check.

## Developer API: limits and rules seen from S3

The [developer API](api.md) (Supabase-Storage-compatible) shares buckets with the S3 API. Two of
its settings show up here.

**Bucket limits** (`file_size_limit`, `allowed_mime_types`, set through the developer API, the
control plane's bucket PATCH or the dashboard) apply to S3 writes too:
- **Errors:**
  - A write over the size limit is `400 EntityTooLarge`.
  - A `Content-Type` that isn't allowed is `400 InvalidRequest`, `mime type <type> is not
    supported`. No `Content-Type` counts as `application/octet-stream`.
- **When it's checked:**
  - `PutObject` checks both before reading the body.
  - `CreateMultipartUpload` checks the type, `UploadPart` refuses a part that alone exceeds the
    size limit, and `CompleteMultipartUpload` checks the object's size.
  - Every write is checked again in the transaction that writes the version: POST uploads,
    `CopyObject`, appends, `RenameObject` and the control plane.
- Keys ending in `.emptyFolderPlaceholder` are exempt.
- Limits are copied to branches like the rest of a bucket's configuration.

**Bucket policies** can hold the developer API's rules next to S3 statements:
- `{"App": "<role>"}` principals (`anon`, `authenticated`, a JWT's `role`, or `*`). Role names are
  letters, digits, `_`, `-` and `*`, up to 64 characters (`Invalid principal in policy`
  otherwise).
- `jwt:<claim>` condition keys and `${jwt:sub}`-style policy variables, from the developer API
  caller's token.
- `roll:ObjectOwner`, the `sub` of the developer API user who wrote the object now at the key
  (`""` for objects written otherwise).

`App` principals and `jwt:*` keys never match an S3 request. An S3 request has no app role, and
`jwt:*` keys are absent for it, so their conditions are false unless they're negated or
`…IfExists`, and a resource or value with a `${jwt:…}` variable matches nothing. `roll:ObjectOwner` is evaluated for any
request. Objects written through the S3 API have no owner (`""`), and a `CopyObject` keeps the
source's. For Block Public Access, `{"App": "anon"}` and `{"App": "*"}` count as public, since the
anon key is public.

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
  trash expiry, forced bucket deletes and the control plane. A bucket holding locked versions
  can't be deleted. A delete with `x-amz-bypass-governance-retention` removes a GOVERNANCE
  retention as on S3, so the version no longer holds its bucket; unlike S3 it still goes to the
  trash for `recover_days` (restorable, unlocked) unless the bucket is deleted first. COMPLIANCE
  and legal holds are never bypassed. A default retention in `Years` counts 365 days a year. `x-amz-bypass-governance-retention` needs `s3:BypassGovernanceRetention` (an `admin` key, the root user, or a policy). Branches copy a version's
  lock with the version, so a branch holding a locked version can't be deleted either. S3
  requires `Content-MD5` for uploads into a bucket with a default retention; roll only requires
  an integrity check when the request itself sets Object Lock headers, and verifies every body's
  checksum anyway.
- **Versioning and branches:** a branch inherits its parent's versioning state and Object Lock
  configuration when it's made. Its version history is its own versions plus the current
  version it inherits; the parent's noncurrent versions aren't part of it.
- **Copies are free and instant,** whatever the size, unless they change the encryption. On S3 a
  copy re-reads and re-writes the data.
- **Completing an SSE-C multipart upload needs the key** (`x-amz-server-side-encryption-customer-*`
  on CompleteMultipartUpload). S3 asks for it only when the upload uses checksums; roll's uploads
  to the backing bucket always do. Current SDKs and the AWS CLI send it.
- **SSE-KMS responses name the KMS key only if the request did.** S3 also names the default
  `aws/s3` key.
- **SigV2 clients disagree on a few details** (the date line with `x-amz-date`, the list of
  signed subresources); roll accepts each variant real clients produce.
- **Writes have higher latency.** A write is acknowledged once its metadata log commit is in S3
  too, which adds roughly one S3 PUT of latency.
- **One region per deployment.** `GetBucketLocation` returns the deployment's region, and
  `CreateBucket`'s `LocationConstraint` is accepted but not used.
- **Bucket names are unique per deployment,** not across all of AWS.
