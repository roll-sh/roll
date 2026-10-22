# Migrate from Cloudflare R2

R2 and roll storage both speak the S3 API, so code moves with a new endpoint and new keys, and
data moves with `rclone sync`. Check [compatibility.md](compatibility.md) for the features you
use.

## Concept mapping

| Cloudflare R2 | roll storage |
|---|---|
| endpoint `https://<account>.r2.cloudflarestorage.com` | `http://127.0.0.1:59000` locally, or your roll endpoint |
| region `auto` | any value works, `auto` included |
| R2 API token (Object Read, Object Read & Write, Admin), optionally per bucket | roll access key: `read`, `write` or `admin`, optionally scoped to buckets |
| public bucket (`r2.dev` or a custom domain) | the bucket's `public` flag: anonymous GET and HEAD on the S3 endpoint |
| CORS policy | `PutBucketCors`, or the dashboard |
| object lifecycle rules | S3's full lifecycle model: expiration, noncurrent versions, aborts, prefix/tag/size filters |
| Infrequent Access storage class | not needed: Intelligent-Tiering moves cold data down by itself |
| bucket locks | S3 Object Lock (retention and legal holds) |
| event notifications, Workers bindings | not supported; use the S3 API from your code |
| — | branches: an instant copy-on-write copy of a bucket |
| — | the trash: deleted and overwritten objects are recoverable for `recover_days` |

## 1. Create buckets and a key

```sh
roll storage create assets
roll storage keys create migration --permission write --bucket assets
```

## 2. Copy the data with rclone

Create an R2 API token with Object Read access, then:

```ini
# ~/.config/rclone/rclone.conf
[r2]
type = s3
provider = Cloudflare
endpoint = https://<account>.r2.cloudflarestorage.com
access_key_id = …
secret_access_key = …

[roll]
type = s3
provider = Other
endpoint = http://127.0.0.1:59000
access_key_id = RK…
secret_access_key = …
force_path_style = true
```

```sh
rclone sync r2:my-bucket roll:assets --checksum --transfers 32 --metadata --progress
rclone check r2:my-bucket roll:assets
```

Reading data out of R2 has no egress fee, so the copy itself costs only R2's class B reads.

## 3. Point the code at roll

Change the endpoint and keys. Keep `region: "auto"` if you like, and set path style unless
`ROLL_S3_DOMAIN` is set:

```ts
new S3Client({
  endpoint: process.env.AWS_ENDPOINT_URL,
  region: "auto",
  forcePathStyle: true,
});
```

Public URLs change from `https://pub-….r2.dev/<key>` to `<endpoint>/<bucket>/<key>`, or to
`https://<bucket>.<domain>/<key>` with virtual-host style.

## What differs

- **Egress pricing.** R2 has no egress fees. roll charges S3's internet egress price ($0.09/GB),
  while traffic inside roll is free. A bucket that mainly serves public downloads to the
  internet costs more on roll than on R2. Put a CDN in front, or keep that bucket on R2.
- **Storage price.** roll charges S3 Standard's $0.023/GB-month; R2 charges $0.015.
- **Recoverable deletes.** Deletes and overwrites go to the trash for 7 days by default, and are
  billed as storage meanwhile.
- **Free copies.** `CopyObject` and branches are instant and copy no data.
- **No R2-specific APIs.** Workers bindings, event notifications and bucket locks aren't
  available.
