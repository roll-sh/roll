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

## Not supported

| feature | notes |
|---|---|
| Versioning (`PutBucketVersioning`, `versionId`) | Use the [trash](README.md#recovery-window-trash) for recovery, and [branches](README.md#branching-buckets) for snapshots. |
| Object Lock, legal hold, retention | |
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
| Lifecycle transitions, `Expiration.Date`, tag and size filters | Intelligent-Tiering handles tiering. |
| Restore from Glacier (`RestoreObject`) | Nothing is archived in a way that needs a restore. |
| Directory buckets (S3 Express) | |

## Behaviour differences

- **Overwrites and deletes are recoverable** for `recover_days` (default 7). On S3 they're gone
  unless versioning is on. Trashed bytes are billed as storage.
- **Copies are free and instant,** whatever the size. On S3 a copy re-reads and re-writes the
  data.
- **Writes have higher latency.** A write is acknowledged once its metadata log commit is in S3
  too, which adds roughly one S3 PUT of latency.
- **One region per deployment.** `GetBucketLocation` returns the deployment's region, and
  `CreateBucket`'s `LocationConstraint` is accepted but not used.
- **Bucket names are unique per deployment,** not across all of AWS.
