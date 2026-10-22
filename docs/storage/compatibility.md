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
| Anonymous requests | what the bucket's policy or ACLs allow, and GET/HEAD of objects in `public` buckets |
| Bucket policies | ✅ enforced; principals are roll access keys (see [Policies](#policies-acls-and-public-access)) |
| Website endpoint (`<bucket>.<website domain>`) | ✅ when `ROLL_S3_WEBSITE_DOMAIN` is set |
| IAM, STS, session tokens | ❌ use roll access keys (`read`, `write`, `admin`, optionally scoped to buckets) and bucket policies |

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
| DeleteObjects | ✅ up to 1000 keys, quiet mode; each key is authorized on its own |
| Get/Put/DeleteBucketTagging | ✅ up to 50 tags, S3's key/value rules |
| Get/Put/DeleteBucketPolicy, GetBucketPolicyStatus | ✅ enforced (see [Policies](#policies-acls-and-public-access)) |
| Get/Put/DeletePublicAccessBlock | ✅ enforced against ACLs, policies and the `public` flag |
| Get/Put/DeleteBucketOwnershipControls | ✅ `BucketOwnerEnforced` by default, as on S3 since 2023 |
| Get/PutBucketAcl, Get/PutObjectAcl, `x-amz-acl`, `x-amz-grant-*` | ✅ enforced when ownership allows ACLs; otherwise `AccessControlListNotSupported`, as on S3 |
| Get/Put/DeleteBucketEncryption | ✅ `AES256` by default (the backing bucket encrypts at rest); `aws:kms` rules are stored |
| Get/Put/DeleteBucketWebsite | ✅ served on the website endpoint |
| Get/PutBucketLogging | ✅ server access logs delivered to the target bucket every 5 minutes, best effort |
| Get/PutBucketAccelerateConfiguration | ✅ stored and returned; a no-op (there's one endpoint) |
| Get/PutBucketRequestPayment | ✅ `BucketOwner`; `Requester` is `NotImplemented` |
| GetBucketReplication | ✅ `ReplicationConfigurationNotFoundError`; Put/Delete are `NotImplemented` for now |
| Get/PutBucketNotificationConfiguration | ✅ empty configurations; anything else is `NotImplemented` for now |
| Analytics, metrics, inventory and intelligent-tiering configurations (Get/Put/Delete/List) | ✅ validated and stored. Inventory reports aren't generated, analytics aren't computed and metrics aren't published yet. Intelligent-tiering archive tiers are `NotImplemented` (nothing is ever archived). |

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
| POST Object (form upload) | ✅ including the `acl` and `x-amz-website-redirect-location` fields |
| RestoreObject | ✅ answers `InvalidObjectState` (403), as S3 does for objects that aren't archived: nothing ever is |

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

## Not supported

| feature | notes |
|---|---|
| Versioning (`PutBucketVersioning`, `versionId`) | Use the [trash](README.md#recovery-window-trash) for recovery, and [branches](README.md#branching-buckets) for snapshots. |
| Object Lock, legal hold, retention | |
| SSE-C (`x-amz-server-side-encryption-customer-*`) | rejected with `NotImplemented`, never ignored |
| SSE-KMS (`x-amz-server-side-encryption: aws:kms`) | rejected. Data is encrypted at rest by the backing S3 bucket (SSE-S3). |
| Replication (`PutBucketReplication`) | The backing bucket can be replicated by the operator. |
| Event notifications (non-empty `NotificationConfiguration`) | |
| Requester Pays | |
| Inventory reports, Storage Class Analysis, CloudWatch request metrics | The configurations are stored, nothing is produced yet. |
| S3 Select, Object Lambda | |
| Torrent | |
| Lifecycle transitions, `Expiration.Date`, tag and size filters | Intelligent-Tiering handles tiering. |
| Directory buckets (S3 Express) | |

## Policies, ACLs and public access

roll has no IAM: requests are signed with roll access keys, and every key belongs to the
deployment, which is the "account" that owns every bucket and object.

**Ids.**
- The **canonical owner id** is a 64-hex-char id created once per deployment and kept in its
  metadata. It's the `Owner` in ListBuckets, listings (`fetch-owner`) and ACLs.
- Each access key has a **canonical user id**: `sha256("roll canonical user\0<owner id>\0<access key id>")`
  in hex. ACLs show grantees by it; grants accept it or the access key id itself.
- The **account id** is 12 digits derived from the owner id, for IAM-style ARNs.
- `GET /v1/storage` shows `owner_id` and `account_id`.

**Bucket policy principals.**
- `"*"` and `{"AWS": "*"}` mean anyone, including anonymous requests.
- `{"AWS": "<access key id>"}` and `{"AWS": "arn:aws:iam::<account id>:user/<access key id>"}` mean that key.
- `{"AWS": "<account id>"}` and `{"AWS": "arn:aws:iam::<account id>:root"}` mean any roll key.
- `{"CanonicalUser": …}` takes the owner id (any key) or a key's canonical user id.
- `Service` and `Federated` principals are accepted and never match.

**What policies support.**
- `Version` `2012-10-17` or `2008-10-17`; `Statement` as one object or an array; `Sid`.
- `Effect`; `Principal`/`NotPrincipal`; `Action`/`NotAction` with `s3:` wildcards;
  `Resource`/`NotResource` with ARN wildcards (they must name the bucket).
- Every IAM condition operator (`String*`, `Numeric*`, `Date*`, `Bool`, `IpAddress`,
  `NotIpAddress`, `Arn*`, `Null`), with `…IfExists`, `ForAnyValue:` and `ForAllValues:`.
- Condition keys:
  - `aws:SourceIp` (the peer address), `aws:SecureTransport` (only `X-Forwarded-Proto: https`
    from an internal proxy counts; roll itself speaks HTTP), `aws:CurrentTime`, `aws:EpochTime`,
    `aws:Referer`, `aws:UserAgent`
  - `aws:username`/`aws:userid` (the access key id), `aws:PrincipalArn`, `aws:PrincipalAccount`,
    `aws:PrincipalType`
  - `s3:prefix`, `s3:delimiter`, `s3:max-keys`, `s3:VersionId`, `s3:signatureversion`, `s3:authType`
  - every `s3:x-amz-*` request header (`s3:x-amz-acl`, `s3:x-amz-server-side-encryption`, …)
  - `s3:ExistingObjectTag/<key>`, `s3:RequestObjectTag/<key>`, `s3:RequestObjectTagKeys`
- Missing keys make a condition false, except for negated operators and `…IfExists`, as in IAM.
- Policies over 20 KB, unknown actions, resources outside the bucket and unknown operators are
  `MalformedPolicy`.

**Evaluation.** A matching `Deny` refuses the request, whoever asks. Then the key's own
permission (`read`/`write`/`admin`) allows it. Then a matching `Allow`, which can open a bucket
to anonymous users or give keys more than their permission. Then ACL grants. Then the bucket's
`public` flag (anonymous `GetObject`). Admin keys covering a bucket can always read, replace and
delete its policy, like an AWS root user, so a policy can't lock its owner out.

**ACLs.** With `ObjectOwnership` `BucketOwnerPreferred` or `ObjectWriter` (set by
`PutBucketOwnershipControls`, or `x-amz-object-ownership` on CreateBucket), ACLs work and are
enforced: canned ACLs, `x-amz-grant-*` and `AccessControlPolicy` bodies, with the `AllUsers`,
`AuthenticatedUsers` and `LogDelivery` groups and canonical users. Email grantees are
`UnresolvableGrantByEmailAddress`. Copies don't take the source's ACL, as on S3.

**Public access block.** The bucket-level settings apply as on S3:
- `BlockPublicAcls` refuses public ACLs.
- `IgnorePublicAcls` ignores public grants.
- `BlockPublicPolicy` refuses public policies and turning the `public` flag on.
- `RestrictPublicBuckets` makes a public policy, and the `public` flag, apply to roll keys only.

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
returned but not applied.

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
