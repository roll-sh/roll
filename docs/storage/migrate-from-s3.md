# Migrate from Amazon S3

roll storage speaks the S3 API, so code moves with a new endpoint and new keys. Data moves with
`rclone sync` or `aws s3 sync`. Check [compatibility.md](compatibility.md) first for the features
you use: bucket policies, ACLs and SSE-KMS aren't supported.

## Concept mapping

| Amazon S3 | roll storage |
|---|---|
| bucket | bucket; names are unique per roll deployment, not across all of AWS |
| endpoint `https://s3.<region>.amazonaws.com` | `http://127.0.0.1:59000` locally, or your roll endpoint |
| region | any value works; `us-east-1` is a safe default |
| IAM user or role and its access keys | roll access key: `read`, `write` or `admin`, optionally scoped to buckets |
| bucket policy, ACLs | the bucket's `public` flag plus scoped keys |
| versioning, Object Lock | supported, as on S3; permanently deleted versions also go to the trash for `recover_days` (0–30) |
| a bucket copy for staging or tests | `roll storage branch <bucket> <name>`: instant, copy-on-write |
| lifecycle transitions to IA or Glacier | not needed: everything is in Intelligent-Tiering |
| lifecycle rules | supported: expiration (days, date, delete markers), noncurrent versions, prefix/tag/size filters; transitions are kept but not needed |
| CORS | supported (`PutBucketCors`, or the dashboard) |
| presigned URLs, presigned POST | supported |

## 1. Create buckets and a key

```sh
roll storage create assets
roll storage keys create migration --permission write --bucket assets
```

Save the secret; it's shown once.

## 2. Copy the data

### rclone (recommended)

rclone copies straight between the two endpoints, and compares sizes and checksums.

```ini
# ~/.config/rclone/rclone.conf
[aws]
type = s3
provider = AWS
region = us-east-1
env_auth = true

[roll]
type = s3
provider = Other
endpoint = http://127.0.0.1:59000
access_key_id = RK…
secret_access_key = …
force_path_style = true
```

```sh
rclone sync aws:my-bucket roll:assets --checksum --transfers 32 --progress
rclone check aws:my-bucket roll:assets           # verify; run again right before cutover
```

- **Order.** Run `sync` once in bulk, then again just before cutover to pick up recent changes.
- **Metadata.** `--metadata` (rclone ≥ 1.66) also copies `Content-Type`, `Cache-Control` and
  `x-amz-meta-*`.

### aws s3 sync

`aws s3 sync` can't read from one endpoint and write to another, so it has to go through local
disk:

```sh
aws s3 sync s3://my-bucket ./staging
AWS_ENDPOINT_URL=http://127.0.0.1:59000 AWS_ACCESS_KEY_ID=RK… AWS_SECRET_ACCESS_KEY=… \
  aws s3 sync ./staging s3://assets
```

## 3. Point the code at roll

The only changes are the endpoint, the keys and path-style addressing:

```python
s3 = boto3.client("s3", endpoint_url=os.environ["AWS_ENDPOINT_URL"])
```

```ts
new S3Client({ endpoint: process.env.AWS_ENDPOINT_URL, region: "us-east-1", forcePathStyle: true });
```

Most SDKs read `AWS_ENDPOINT_URL` from the environment by themselves: the JS v3 SDK, boto3,
the Go v2 SDK, the Java v2 SDK and the aws cli. Setting it plus the two keys is often all the
change you need.

- **Path style.** Use `forcePathStyle` / `s3ForcePathStyle` / `addressing_style = path` unless
  `ROLL_S3_DOMAIN` is set for virtual-host style.
- **Checksums.** Keep your SDK's default checksum setting; trailers are verified.
- **Presigned URLs.** Generate them with the new endpoint, because URLs signed for AWS don't
  work against roll.

## 4. Cut over

1. Run a final `rclone sync`, then `rclone check`.
2. Deploy with the new endpoint and keys.
3. Keep the S3 bucket read-only for a while, so you can fall back.

## What differs

- **Recoverable deletes.** Deletes and overwrites go to the trash for 7 days by default, and are
  billed as storage meanwhile. Set `recover_days` to 0 on high-churn buckets.
- **Free copies.** `CopyObject` is instant and free at any size, because no data is copied.
- **Write latency.** Writes take roughly one S3 PUT longer, because metadata is made durable
  before the reply.
- **Versioning.** Supported; permanently deleted versions stay recoverable in the trash for `recover_days`.
