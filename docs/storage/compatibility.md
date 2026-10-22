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
| Get/Put/DeleteBucketLifecycleConfiguration | ✅ the full rule model: `Filter` (`Prefix`, `Tag`, `And`, `ObjectSizeGreaterThan`/`LessThan`) or the legacy `<Rule><Prefix>`; `Expiration` (`Days`, `Date`, `ExpiredObjectDeleteMarker`); `NoncurrentVersionExpiration` (`NoncurrentDays`, `NewerNoncurrentVersions`); `AbortIncompleteMultipartUpload`; `Transition` and `NoncurrentVersionTransition` (stored, see below). Validated like S3. |
| Get/PutBucketVersioning | ✅ `Enabled` and `Suspended`. `MfaDelete=Enabled` is `NotImplemented` (there are no MFA devices; use Object Lock). |
| ListObjectVersions | ✅ `prefix`, `delimiter`, `key-marker`, `version-id-marker`, `max-keys`, `encoding-type=url`, delete markers |
| Get/PutObjectLockConfiguration | ✅ enable on a versioned bucket (or at CreateBucket with `x-amz-bucket-object-lock-enabled`), default retention in `Days` or `Years` |
| ListObjectsV2, ListObjects (v1) | ✅ `prefix`, `delimiter`, `start-after`, continuation tokens, `max-keys`, `encoding-type=url` |
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
| Get/PutBucketRequestPayment | ✅ `BucketOwner`; `Requester` is `NotImplemented` |
| Get/Put/DeleteBucketReplication | ✅ between roll buckets, metadata-only (see [Replication](#replication)); a destination outside roll is `NotImplemented` |
| Get/PutBucketNotificationConfiguration | ✅ Topic, Queue, CloudFunction and EventBridge configurations, event wildcards, `prefix`/`suffix` filter rules, `x-amz-skip-destination-validation`, `s3:TestEvent` (see [Event notifications](#event-notifications)) |
| Inventory configurations (Get/Put/Delete/List) | ✅ reports generated Daily/Weekly as CSV (see [Inventory](#inventory-reports)); ORC and Parquet are `NotImplemented` |
| Analytics, metrics and intelligent-tiering configurations (Get/Put/Delete/List) | ✅ validated and stored. Analytics aren't computed and metrics aren't published yet. Intelligent-tiering archive tiers are `NotImplemented` (nothing is ever archived). |

### Objects

| operation | status |
|---|---|
| PutObject | ✅ returns `x-amz-version-id` in versioned buckets |
| GetObject, HeadObject | ✅ `versionId`, `Range`, `partNumber`, `If-Match`, `If-None-Match`, `If-Modified-Since`, `If-Unmodified-Since`, `response-*` overrides, `x-amz-checksum-mode`; Object Lock headers; `x-amz-replication-status`; a delete marker answers 404 (current) or 405 (by id) with `x-amz-delete-marker: true` |
| DeleteObject | ✅ unversioned: the object goes to the bucket's trash. Versioned: a delete marker; with `versionId`, that version is deleted for good (to the trash, see below) |
| CopyObject | ✅ `x-amz-metadata-directive`, `x-amz-copy-source-if-*`, `?versionId=` in the source, `x-amz-copy-source-version-id`; no data is copied |
| Conditional writes: `If-None-Match: *`, `If-Match` on PutObject and CompleteMultipartUpload | ✅ |
| Get/Put/DeleteObjectTagging, `x-amz-tagging` | ✅ with `versionId` |
| Get/PutObjectRetention, Get/PutObjectLegalHold | ✅ `versionId`, `x-amz-bypass-governance-retention` (admin keys) |
| `x-amz-object-lock-mode`, `-retain-until-date`, `-legal-hold` | ✅ on PutObject (with `Content-MD5` or an `x-amz-checksum-*`, as S3 requires), CopyObject, CreateMultipartUpload and POST Object |
| CreateMultipartUpload, UploadPart, UploadPartCopy, CompleteMultipartUpload, AbortMultipartUpload, ListParts | ✅ UploadPartCopy takes a source `versionId`; Complete returns `x-amz-version-id` |
| POST Object (form upload) | ✅ including the `acl`, `x-amz-website-redirect-location` and Object Lock fields |
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
| MFA Delete | No MFA devices. Object Lock protects versions instead. |
| SSE-C (`x-amz-server-side-encryption-customer-*`) | rejected with `NotImplemented`, never ignored |
| SSE-KMS (`x-amz-server-side-encryption: aws:kms`) | rejected. Data is encrypted at rest by the backing S3 bucket (SSE-S3). |
| Replication to buckets outside roll (real AWS S3 buckets, other deployments) | `NotImplemented`, with a message saying so. The backing bucket can be replicated by the operator. |
| Replication Time Control, replica modification sync, `SourceSelectionCriteria` (SSE-KMS objects) | stored and returned, not acted on (replication between roll buckets is immediate, and copies no data) |
| Requester Pays | |
| ORC and Parquet inventory reports | `NotImplemented` when the configuration is put; CSV works |
| Storage Class Analysis, CloudWatch request metrics | The configurations are stored, nothing is produced yet. |
| S3 Select, Object Lambda | |
| Torrent | |
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
| `s3:Replication:OperationFailedReplication` | a version couldn't be replicated (see [Replication](#replication)) |
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
| `arn:aws:sqs:…`, `arn:aws:sns:…`, `arn:aws:lambda:…`, `EventBridgeConfiguration` | real AWS, SigV4-signed with the deployment's AWS credentials (the default chain), when `ROLL_S3_NOTIFY_AWS` is on: SQS `SendMessage`, SNS `Publish` (subject `Amazon S3 Notification`), Lambda `Invoke` (`Event`), EventBridge `PutEvents` on the default bus in the deployment's region (source `roll.s3`, since `aws.s3` is reserved for AWS). Otherwise these fail destination validation. |

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
`Account`, `AccessControlTranslation`; `EncryptionConfiguration`, `ReplicationTime` and `Metrics`
are stored and returned), `ExistingObjectReplication` and `SourceSelectionCriteria` (stored). As
on S3, the source and every destination need versioning `Enabled` (`InvalidRequest` otherwise),
priorities must be unique, V2 rules need `DeleteMarkerReplication`, and the destination can't be
the source.

- **The destination is a roll bucket** of the same deployment, named by `arn:aws:s3:::<bucket>`.
  A name that isn't a roll bucket would be a real AWS bucket: that's `NotImplemented`.
- **Metadata-only.** A replica shares the source version's blob (one more reference): it's
  instant whatever the size, and copies no bytes. It keeps the source's version id and
  `Last-Modified`, and takes its place in the destination's history by version id. Deleting the
  source version later doesn't affect the replica.
- **Asynchronous,** through the event outbox: the source version is `PENDING`, then `COMPLETED`
  once every destination has it (or `FAILED`, with an `s3:Replication:OperationFailedReplication`
  event, if a destination can't take it: it was deleted, its versioning was suspended, or a
  replica would replace a locked null version). GET and HEAD return `x-amz-replication-status`;
  replicas say `REPLICA`. Replicas aren't replicated again (no chains, no loops).
- **Delete markers** replicate when `DeleteMarkerReplication` is `Enabled` (always, for legacy
  `Prefix` rules), as on S3. Lifecycle delete markers and deletes of a version by id never
  replicate.
- **`ExistingObjectReplication` `Enabled`** replicates the versions already in the bucket (S3
  needs a support case or Batch Replication for that): a background walk queues every version in
  scope that has no replication status yet.
- `AccessControlTranslation` `Owner=Destination` makes replicas private; otherwise they keep the
  source's ACL. Object Lock retention and legal holds replicate if the destination has Object
  Lock enabled, and are dropped otherwise. `StorageClass` is stored but has no effect.
- **Branches.** Replication *from* a branch replicates the branch's own writes (a branch doesn't
  inherit its parent's configuration). Replication *to* a branch writes the replica into the
  branch like any write: the branch's own branches keep their view (push-down), and the parent
  doesn't see it.
- `DeleteBucketReplication` stops new versions from replicating; ones already queued still go.

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
