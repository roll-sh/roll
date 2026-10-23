# roll storage: pricing and margin

These are internal notes behind the prices in [architecture](architecture.md#pricing). The
figures are AWS us-east-1 list prices; re-check them before changing any price.

## What we charge

We charge S3 Standard list prices so that "same price as S3" is literally true:

- storage $0.023/GB-month
- class A $5/M
- class B $0.40/M
- DELETE free
- internet egress $0.09/GB
- **cached egress $0.03/GB**: bytes Cloudflare serves from its cache for a bucket with the CDN
  on (Supabase's rate)

Traffic inside roll is free: S3 doesn't charge us for same-region transfer either.

## What it costs us

| line item | our cost | margin |
|---|---|---|
| storage, hot (read in the last 30 days) | $0.023 (INT frequent tier) | 0% |
| storage, 30–90 days cold | $0.0125 (INT infrequent) | 46% |
| storage, 90+ days cold | $0.004 (INT archive instant) | 83% |
| INT monitoring, objects ≥ 128 KB | $0.0025 per 1000 objects-month | small |
| class A / class B requests | same as price, plus ~1 PUT per metadata group commit | ~0% |
| egress, internet | $0.09 → $0.05/GB with volume, less through CloudFront | 0% → 40%+ |
| egress, cached (CDN) | Cloudflare + misses × origin egress (below) | ~50–80% at 90–99% hit rates |
| class B served by the gateway cache | nothing | 100% |
| S3 volume tiers (> 50 TB, > 500 TB) | $0.022, $0.021 | 4–9% on storage |

The metadata log costs at most one PUT per `commit_ms` (20 ms) while busy, which is
50 PUTs/s, about $650/month. Every write in that window shares it, and an idle node costs
nothing. Snapshots are a few PUTs an hour.

## Expected margin

Object storage is mostly cold. With 40% hot, 30% infrequent and 30% archive-instant data,
storage costs $0.0142/GB against a $0.023 price, a **38%** margin on storage. Blended with
pass-through requests, the margin is **20–40%** for typical workloads.

- Month one is about 0%: everything is hot until Intelligent-Tiering moves it.
- A workload that reads all of its data every month stays around 0%. It never goes negative.

## The gateway cache: saved S3 GETs

The gateway's hot-object cache ([architecture](architecture.md#gateway-cache-s3-serversrccachers))
serves hot objects from memory or local disk. Customers are still billed class B for every GET,
exactly as S3 would bill it; a hit just costs us nothing. So:

- a hit's class B ($0.40/M) is **100% margin** instead of 0%: S3 GETs cost us $0.40/M too
  (`gets_avoided` in `GET /v1/storage` is the count, `s3_gets` what misses cost);
- a miss costs one S3 GET per run of up to 8 missing 1 MiB chunks, so an object the cache
  takes in costs at most one GET more than S3 for objects over 8 MiB, and none for smaller ones;
- the cache is local RAM and NVMe we already pay for with the instance; there's no transfer
  cost (S3 to EC2 in-region is free either way).

For a read-heavy bucket whose hot set fits (256 MiB memory, 10 GiB disk by default), class B goes
from break-even to nearly all margin: at 100M GETs/month with a 90% hit rate, $36 of the $40 of
class B is margin.

## Cached egress (CDN)

Buckets with the CDN on are served through Cloudflare ([architecture](architecture.md#cdn-cloudflare)).
The customer pays $0.03/GB for what Cloudflare serves (`egress_cached`, reported by the edge
Worker per bucket). Origin pulls (Cloudflare fetching from roll on a cache miss,
`egress_cdn_origin`) aren't billed as egress: they're our cost. Each origin pull is a GET and
is billed class B like any GET, as S3 bills CloudFront's origin fetches.

**What it costs us** per GB served:

`cost = cloudflare_per_gb + (1 − hit_rate) × origin_egress_per_gb`

- `origin_egress_per_gb`: roll's EC2 egress to the internet (Cloudflare) on a miss: AWS lists
  $0.09/GB, falling to $0.05/GB with volume (and the gateway cache means the miss rarely costs
  an S3 GET too).
- `cloudflare_per_gb`: **assumptions, check current Cloudflare pricing and terms before relying
  on them.** Self-serve plans (Free, Pro ~$20/month, Business ~$200/month per zone) don't meter
  bandwidth, but Cloudflare's terms restrict serving large volumes of non-web content (video,
  large files) without a paid service or an Enterprise agreement. We assume an Enterprise
  contract with committed bandwidth at about **$0.005/GB** blended at volume, plus Workers
  requests (Workers Paid: $5/month including 10M requests, then $0.30/M: ~$0.0003/GB at 1 MB
  per request). Purge by URL, tag and prefix are assumed available (purge by tag and prefix
  came to every plan in 2025; per-request limits and rate limits differ by plan:
  `purge_batch` and `purge_per_minute` are set to match).

**Hit-rate sensitivity** (cached egress at $0.03/GB, Cloudflare at $0.005/GB):

| byte hit rate | cost at $0.09/GB origin egress | margin | cost at $0.05/GB | margin |
|---|---|---|---|---|
| 99% | $0.0059 | 80% | $0.0055 | 82% |
| 95% | $0.0095 | 68% | $0.0075 | 75% |
| 90% | $0.0140 | 53% | $0.0100 | 67% |
| 80% | $0.0230 | 23% | $0.0150 | 50% |
| 72% | $0.0302 | 0% | $0.0190 | 37% |
| 50% | $0.0500 | −67% | $0.0300 | 0% |

Break-even is a 72% byte hit rate at list egress and 50% at volume egress. Static assets and
media served many times sit well above 90%; long-tail content served once (backups, one-off
downloads) is below break-even, and is cheaper for us as plain internet egress at $0.09/GB, which
is why the CDN is opt-in per bucket. `GET /v1/storage/cdn` shows the `margin` view: bytes
served, bytes pulled and the byte hit ratio (`1 − pulled / served`) over 30 days.

Ways to raise the margin, none built: Cloudflare's Tiered Cache (fewer origin pulls per edge
location), Cache Reserve, and a private interconnect to Cloudflare (AWS Direct Connect via
Cloudflare's CNI) to cut origin egress well below internet rates.

## What we deliberately don't do

- Packing small objects into shared blobs: cheaper requests, but GC rewrites live data.
- Compression: margin depends on the data, and it breaks ranges and ETags.
- Moving bytes off AWS: egress to get there, extra latency, and a second durability story.

Each of these would add margin at the cost of the durability guarantee, which comes first.
