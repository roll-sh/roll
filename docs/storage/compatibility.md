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
| SigV4a (`AWS4-ECDSA-P256-SHA256`) header and presigned URLs | ✅ any `x-amz-region-set` is accepted |
| `STREAMING-AWS4-ECDSA-P256-SHA256-PAYLOAD` and `-TRAILER` | ✅ ECDSA chunk signatures verified, `*`-padded ones too |
| SigV2 (`Authorization: AWS key:signature`) | ✅ including virtual-host style; clock skew 15 minutes |
| SigV2 presigned URLs (`AWSAccessKeyId`, `Expires`, `Signature`) | ✅ no 7-day cap, as on S3 |
| Presigned POST policy (browser form upload) | ✅ SigV4 and SigV2 (`AWSAccessKeyId`, `signature`) |
| Any region in the credential scope | ✅ accepted as given, like R2's `auto` |
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
| Get/Put/DeleteBucketLifecycleConfiguration | ✅ `Expiration.Days` and `AbortIncompleteMultipartUpload`, with a prefix filter |
| GetBucketVersioning | ✅ always reports "not enabled" |
| ListObjectsV2, ListObjects (v1) | ✅ `prefix`, `delimiter`, `start-after`, continuation tokens, `max-keys`, `encoding-type=url` |
| ListMultipartUploads | ✅ |
| DeleteObjects | ✅ up to 1000 keys, quiet mode |

### Objects

| operation | status |
|---|---|
| PutObject | ✅ |
| GetObject, HeadObject | ✅ `Range`, `partNumber`, `If-Match`, `If-None-Match`, `If-Modified-Since`, `If-Unmodified-Since`, `response-*` overrides, `x-amz-checksum-mode` |
| DeleteObject | ✅ the object goes to the bucket's trash |
| CopyObject | ✅ `x-amz-metadata-directive`, `x-amz-copy-source-if-*`; no data is copied |
| Conditional writes: `If-None-Match: *`, `If-Match` on PutObject and CompleteMultipartUpload | ✅ |
| Get/Put/DeleteObjectTagging, `x-amz-tagging` | ✅ |
| CreateMultipartUpload, UploadPart, UploadPartCopy, CompleteMultipartUpload, AbortMultipartUpload, ListParts | ✅ |
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
| `x-amz-expected-bucket-owner`, `x-amz-source-expected-bucket-owner` | ✅ `AccessDenied` unless it's the deployment's owner id (`roll`) |

### Server-side encryption

The backing object store does the encryption: roll passes the SSE parameters through on the
object's own requests to S3, so durability and integrity are exactly S3's. Customer keys are
never stored; the metadata keeps their MD5 to tell a right key from a wrong one.

| feature | status |
|---|---|
| SSE-S3 (`x-amz-server-side-encryption: AES256`) | ✅ echoed; everything is encrypted at rest by the backing bucket anyway |
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
  directory backend. `ROLL_S3_KMS=true|false` overrides it.
- **S3-compatible backends.** Every encrypted write checks that the backing service confirmed the
  encryption. One that ignores SSE headers (or doesn't confirm them for an operation, as
  SeaweedFS does for multipart uploads and copies with SSE-C) gets `NotImplemented` for that
  operation, and nothing it stored for the request is kept: data is never stored unencrypted
  when encryption was asked for.
- **The control plane** (`/v1/storage`, the dashboard) can't read SSE-C objects: it has no key.

## Not supported

| feature | notes |
|---|---|
| Versioning (`PutBucketVersioning`, `versionId`) | Use the [trash](README.md#recovery-window-trash) for recovery, and [branches](README.md#branching-buckets) for snapshots. |
| Object Lock, legal hold, retention | |
| ACLs (`PutObjectAcl`, `x-amz-acl`, grants) | Use the bucket's `public` flag and scoped keys. `x-amz-acl: private` is accepted. |
| Bucket policies (`PutBucketPolicy`) | Use scoped keys and the `public` flag. |
| Bucket tagging | |
| Replication configuration | The backing bucket can be replicated by the operator. |
| Static website hosting | |
| S3 Select, Object Lambda | |
| Torrent | |
| Requester pays, transfer acceleration, inventory, analytics, metrics configuration, notifications | |
| Lifecycle transitions, `Expiration.Date`, tag and size filters | Intelligent-Tiering handles tiering. |
| Restore from Glacier (`RestoreObject`) | Nothing is archived in a way that needs a restore. |
| Directory buckets (S3 Express) | |

## Behaviour differences

- **Overwrites and deletes are recoverable** for `recover_days` (default 7). On S3 they're gone
  unless versioning is on. Trashed bytes are billed as storage.
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
