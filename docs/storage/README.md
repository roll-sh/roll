# roll storage docs

roll storage is S3-compatible object storage. Any S3 SDK or tool works against it: `aws`,
boto3, the JS v3 SDK, rclone, `mc` and so on. It also has two things S3 doesn't:

- **branching buckets:** a copy-on-write copy of a bucket in one call, for previews and tests
- **a recovery window:** deleted and overwritten objects stay restorable for a few days

Underneath, every object is one object in Amazon S3 (or any S3-compatible store), so durability
is S3's own.

## Reference

| doc                                     |                                                                       |
| --------------------------------------- | --------------------------------------------------------------------- |
| [architecture.md](architecture.md)      | how it's built: one blob per object, the metadata log, branching, trash, GC, the control-plane API (`/v1/storage/...`) |
| [compatibility.md](compatibility.md)    | which S3 operations, headers and features are supported, and which aren't |
| [pricing.md](pricing.md)                | internal notes: what each line item costs us and where the margin comes from |

## Migration guides

| from                                      | path                                                         |
| ----------------------------------------- | ------------------------------------------------------------ |
| [Amazon S3](migrate-from-s3.md)           | `rclone sync` or `aws s3 sync`, then change the endpoint     |
| [Cloudflare R2](migrate-from-r2.md)       | `rclone sync` between two S3 remotes, then change the endpoint |

## Local endpoints

| listener                      | default            | flag / env                     |
| ----------------------------- | ------------------ | ------------------------------ |
| S3 API                        | `127.0.0.1:59000`  | `--s3` / `ROLL_S3`             |
| roll API (`/v1/storage`)      | `127.0.0.1:7070`   | `--api` / `ROLL_API`           |

Other settings:

- `ROLL_S3_DOMAIN`: turns on virtual-host style addressing (`<bucket>.<domain>`). Path style
  (`http://host:59000/<bucket>/<key>`) always works.
- `ROLL_S3_ACCESS_KEY` / `ROLL_S3_SECRET_KEY`: an `admin` key created at startup, like MinIO's
  root user. Use it for local development and tests.
- `ROLL_S3_STORAGE_CLASS`: the backing storage class. The default is `INTELLIGENT_TIERING` on
  AWS S3 and `STANDARD` when `ROLL_S3_ENDPOINT` points at another S3-compatible store (MinIO,
  SeaweedFS). It's ignored for a local directory.
- `ROLL_S3_INTERNAL_CIDRS`: peers counted as "inside roll" for egress pricing. The default is
  loopback, the private ranges and the Docker bridge. On AWS, set it to the VPC.
- `ROLL_S3_KMS`: pass SSE-KMS (`aws:kms`, `aws:kms:dsse`) through to the backing bucket. The
  default is on for AWS S3 without `ROLL_S3_ENDPOINT` and off otherwise (requests then get
  `NotImplemented`). SSE-C works on any backend that supports it; see
  [compatibility](compatibility.md#server-side-encryption).
- `ROLL_S3_BUCKET`, `ROLL_S3_PREFIX`, `ROLL_S3_ENDPOINT`: the backing store, shared with the
  other roll products. Without a bucket, a directory under `.roll/` stands in.
- `ROLL_S3_NOTIFY_AWS`: deliver event notifications to real AWS destinations (SQS, SNS, Lambda,
  EventBridge) with this host's AWS credentials. Off by default: roll queues, roll streams and
  webhooks work without it.
- `ROLL_S3_WEBHOOK_ALLOW_PRIVATE`: let notification webhooks reach loopback and private
  addresses (single-tenant installs, local development).
- `--no-storage` / `ROLL_NO_STORAGE`: don't run roll storage.

The roll API on `:7070` has no authentication. Keep it private.

## Quickstart

```sh
cargo run -p rolld                                   # S3 API on :59000, roll API on :7070
roll storage create assets
roll storage keys create laptop --permission write   # prints the secret once
```

`keys create` prints ready-to-paste credentials:

```sh
export AWS_ACCESS_KEY_ID=RK…
export AWS_SECRET_ACCESS_KEY=…
export AWS_ENDPOINT_URL=http://127.0.0.1:59000
export AWS_REGION=us-east-1
```

Any region works: roll accepts the region in the request's signature as it is.

### aws cli

```sh
aws s3 cp ./logo.png s3://assets/img/logo.png
aws s3 ls s3://assets/img/
aws s3 sync ./public s3://assets/public
aws s3 presign s3://assets/img/logo.png --expires-in 3600
```

### boto3

```python
import boto3

s3 = boto3.client("s3", endpoint_url="http://127.0.0.1:59000")   # credentials from the env
s3.put_object(Bucket="assets", Key="hello.txt", Body=b"hi", ContentType="text/plain")
print(s3.get_object(Bucket="assets", Key="hello.txt")["Body"].read())
```

### JS (AWS SDK v3)

```ts
import { S3Client, PutObjectCommand, GetObjectCommand } from "@aws-sdk/client-s3";

const s3 = new S3Client({
  endpoint: "http://127.0.0.1:59000",
  region: "us-east-1",
  forcePathStyle: true, // unless ROLL_S3_DOMAIN is set
});
await s3.send(new PutObjectCommand({ Bucket: "assets", Key: "hello.txt", Body: "hi" }));
const res = await s3.send(new GetObjectCommand({ Bucket: "assets", Key: "hello.txt" }));
console.log(await res.Body?.transformToString());
```

The default checksums of current SDKs (CRC32 or CRC64NVME, sent as trailers) are verified;
there's no need to turn them off.

### rclone

```ini
# ~/.config/rclone/rclone.conf
[roll]
type = s3
provider = Other
endpoint = http://127.0.0.1:59000
access_key_id = RK…
secret_access_key = …
force_path_style = true
```

```sh
rclone copy ./backups roll:assets/backups
rclone ls roll:assets
```

### roll CLI

`roll storage` (alias `roll s3`) talks to the roll API and needs no S3 keys:

```sh
roll storage ls assets/img/
roll storage cp ./logo.png assets/img/logo.png
roll storage cp assets/img/logo.png -
roll storage stat assets/img/logo.png
roll storage presign assets/img/logo.png --expires 1h
roll storage rm assets/tmp/ -r
roll storage usage assets
```

The dashboard is at `/dashboard/storage`. It has buckets, an object browser with drag-and-drop
upload, recently deleted objects, branches, settings, access keys and usage.

## Branching buckets

```sh
roll storage branch assets assets-pr-42
```

A branch is a new bucket that starts with exactly the parent's objects at the moment it was
created:

- **It's instant.** No data is copied; the branch shares the parent's blobs.
- **It's isolated in both directions.** Writes to the branch don't touch the parent. Writes to
  the parent after the branch was created don't show up in the branch.
- **It's billed for what it adds.** That's the objects it writes, plus parent objects it keeps
  alive after the parent overwrote or deleted them. Inherited objects stay billed to the parent.
- **The parent can't be deleted first.** A bucket with branches can't be deleted until its
  branches are.

Use it the way you'd branch a database: one bucket per preview deployment, a scratch copy for a
migration, a fixture for a test run.

## Recovery window (trash)

Deleting or overwriting an object moves the old version to the bucket's trash. It can be
restored for `recover_days` (default 7, from 0 to 30):

```sh
roll storage trash assets --prefix img/
roll storage restore assets/img/logo.png --at <deleted_at>
```

- Restoring puts the old version back. Whatever is at that key now goes to the trash.
- Trashed objects are billed as storage until they expire. Set `recover_days` to 0 for buckets
  with heavy churn.
- `roll storage delete <bucket> --force` deletes a bucket with everything in it. Those objects
  don't go to a trash, because the bucket is gone.
- **Versioned buckets** (`PutBucketVersioning`) keep overwritten and deleted objects as
  noncurrent versions instead, as on S3. Deleting a version for good (`DELETE ?versionId=`, or
  a lifecycle rule) still moves it to the trash, so even that is recoverable. Restoring it puts
  it back in the key's history. Object Lock (retention and legal holds) works as on S3, and a
  locked version is never deleted by anything.

```sh
roll storage update assets --versioning on      # off = Suspended; never back to unversioned
roll storage ls assets/img/ --versions          # version ids, latest, delete markers
roll storage cp assets/img/logo.png old.png --version-id <id>
roll storage rm assets/img/logo.png --version-id <id>   # for good (to the trash)
```

Below the trash, roll keeps unreferenced data for another 24 hours before it really deletes it.
That's a safety net for roll's own bugs, not something you can use.

## Public buckets, CORS, presigned URLs

- **Public.** `roll storage update assets --public` allows anonymous `GET` and `HEAD` of
  objects. Listing still needs a key.
- **CORS.** Browsers need a CORS rule to call the S3 API directly.
  `roll storage update assets --cors-origin https://app.example.com` adds one, and the dashboard
  has an editor. `PutBucketCors` works as on S3.
- **Presigned URLs** last up to 7 days:
  - `aws s3 presign`, any SDK's presigner, `roll storage presign` and the dashboard can all make
    them.
  - Presigned `PUT` URLs from the roll API sign only `host`, so the uploader can send any
    `Content-Type`.
  - Browser form uploads with a presigned POST policy (`createPresignedPost`) work too.

## Events and replication

- **Event notifications** go to roll queues, roll streams, webhooks (signed with the bucket's
  secret, `X-Roll-Signature`) or AWS. `PutBucketNotificationConfiguration`, the dashboard's
  Events panel and the CLI all set them.
- **Replication** to another roll bucket is instant and free: a replica shares the source
  version's data, so no bytes are copied or transferred. Both buckets must be versioned.

```sh
roll storage notify add assets --to queue:uploads --prefix img/       # or stream:<s>/<topic>, https://…, an ARN
roll storage notify assets                                            # list, with what's still pending
roll storage notify secret assets [--rotate]
roll storage replicate assets --to assets-mirror --delete-markers [--existing]
roll storage replicate rm assets [<rule id>]
```

## Lifecycle

`PutBucketLifecycleConfiguration`, or the bucket settings in the dashboard, supports S3's rules,
filtered by prefix, tags and object size:

- **Expiration** (`Days` or `Date`): objects are deleted. They go to the trash like any other
  delete (or get a delete marker in a versioned bucket), so they're still recoverable.
- **NoncurrentVersionExpiration:** noncurrent versions older than N days (beyond the newest
  `NewerNoncurrentVersions`) are deleted for good, to the trash.
- **ExpiredObjectDeleteMarker:** delete markers with nothing left behind them are removed.
- **AbortIncompleteMultipartUpload.DaysAfterInitiation:** unfinished multipart uploads are
  aborted. With no rule, they're aborted after 7 days.

Transitions are accepted and kept, but never needed: roll stores everything in S3
Intelligent-Tiering, which moves cold data to cheaper tiers by itself.

## Durability

What's guaranteed:

- **An acknowledged write is in S3.** A `PUT` is acknowledged only after the object's bytes and
  the metadata pointing at them are both in S3, which is multi-AZ. A crash or the loss of the
  server's disk loses nothing that was acknowledged.
- **One object is one S3 object.** roll never packs objects together or rewrites stored data, so
  every object has S3's own durability (11 nines).
- **Stored bytes are never modified.** An overwrite writes a new object and points the key at it.
- **Checksums are verified at every hop.**
  - client → roll: `Content-MD5` and the `x-amz-checksum-*` headers and trailers
  - roll → S3: a CRC32C that S3 checks
- **Deletes are delayed twice:** the trash you can see, then a 24 h quarantine you can't. A bug
  that drops a reference can be repaired before any data is gone.
- **Metadata can be rebuilt from S3 alone.** The server's local database is a cache, rebuilt at
  start from snapshots and a log in S3.
- **There's an integrity check.** A weekly scrub compares what metadata references with what's
  in S3. It reports any differences and never deletes anything.

What isn't (yet):

- **One node owns the metadata.** If that node goes down, requests fail until it's back, and
  data and acknowledged writes are safe meanwhile. Stateless gateways and standby takeover are
  designed but not built ([architecture](architecture.md#scaling-beyond-one-node)).
- **Writes take longer than on S3.** Every write waits for a metadata group commit, which is
  roughly one extra S3 PUT of latency.

## Pricing

The same as S3 Standard list prices (us-east-1):

| | price |
|---|---|
| storage | $0.023 / GB-month |
| class A: PUT, COPY, POST, LIST, multipart | $5.00 / million |
| class B: GET, HEAD, everything else | $0.40 / million |
| DELETE, AbortMultipartUpload | free |
| egress to the internet | $0.09 / GB |
| egress inside roll (same region) | free |

- Copies and branches cost one class A request, and no extra storage until something changes.
- `roll storage usage`, `GET /v1/storage/usage` and the dashboard price your real usage.

## Limits

These are the same as S3:

| limit | value |
|---|---|
| bucket name | 3–63 characters: lowercase letters, digits, `-` and `.` |
| key | up to 1024 bytes (UTF-8) |
| user metadata (`x-amz-meta-*`) | 2 KB |
| single `PUT` | 5 GiB |
| object size | 5 TiB (multipart) |
| multipart parts | 1–10 000, each at least 5 MiB except the last |
| keys per list page, per `DeleteObjects` | 1000 |
| presigned URL lifetime | 7 days |
| request clock skew | 15 minutes |

## Events into roll queues and streams

A bucket can send its S3 event notifications straight into roll queues, roll streams or a
webhook, with the S3 API (any SDK) or the control plane:

```sh
aws --endpoint-url http://127.0.0.1:59000 s3api put-bucket-notification-configuration --bucket assets \
  --notification-configuration '{"QueueConfigurations": [{"QueueArn": "arn:roll:queues:%2F:uploads",
    "Events": ["s3:ObjectCreated:*"], "Filter": {"Key": {"FilterRules": [{"Name": "prefix", "Value": "img/"}]}}}]}'
```

The queue (`uploads` in vhost `/`) must exist; it receives an `s3:TestEvent` right away, then one
message (S3's event JSON) per upload under `img/`. Use `arn:roll:streams:<stream>:<topic>` for a
Kafka topic (keyed by the object key) and `arn:roll:webhook:https://…` for an HTTP endpoint
(signed with the bucket's webhook secret, shown by `GET /v1/storage/buckets/assets`). See
[compatibility](compatibility.md#event-notifications), and for replication and inventory reports
the sections after it.

Manual check against a running `rolld`: create the queue (`roll queues` or the management API),
put the configuration above, upload `img/a.png`, and read the queue: the second message's
`Records[0].eventName` is `ObjectCreated:Put` (the first is the test event). The rolld unit test
`storage_events::tests::a_roll_queue_receives_storage_events` does the same in process.
