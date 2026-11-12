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

## Image transformations

What we charge: **$5 per 1000 unique source images transformed in a month**, Supabase's price,
with **100 a month included per account**. A source image is an object's blob: it counts once
per bucket per month however many sizes and formats are made of it, and copies, branches and
versions sharing the blob are one source. Failed renders aren't counted. Nothing else is charged
for: the request is metered as a class B GET and the bytes as egress, as for any download, and
the cached derived images are **not billed as storage** (see below).

### What it costs us

CPU per transformation, measured with `cargo run --release -p s3-image --example bench` on the
dev server (AMD Ryzen 7 1700X, 2017, one core; thread CPU time, median of 3–5, taken while the
machine was shared with other builds, so these are upper bounds; a current AWS core is
1.5–2× faster):

| source | output | CPU ms | bytes |
|---|---|---|---|
| 8 MP JPEG (3840×2160 screenshot, 591 KiB) | 300 wide, JPEG | 69 | 12 KB |
| | 300 wide, WebP | 67 | 8 KB |
| | 300 wide, AVIF | 215 | 10 KB |
| | 1200 wide, JPEG | 91 | 133 KB |
| | 1200 wide, WebP | 174 | 73 KB |
| | 1200 wide, AVIF | 1161 | 110 KB |
| | 300×300 cover, WebP | 77 | 15 KB |
| 1 MP photo-like JPEG (1200×800) | 300 wide, WebP | 24 | 8 KB |
| | 1200 wide, WebP | 201 | 181 KB |
| 3 MP photo-like JPEG (2048×1536) | 300 wide, WebP | 56 | 13 KB |
| | 1200 wide, WebP | 219 | 128 KB |
| | 1200 wide, AVIF | 2413 | 158 KB |
| 12 MP photo-like JPEG (4000×3000) | 300 wide, WebP | 189 | 13 KB |
| | 1200 wide, WebP | 336 | 65 KB |
| | 1200 wide, AVIF | 2802 | 198 KB |

(The "photo-like" sources are synthetic with per-pixel noise, the hardest case for every
encoder; real photos compress better and AV1 encodes them faster.) Decoding dominates small
outputs (a 12 MP JPEG takes ~150 ms to decode); WebP and JPEG cost tens to a few hundred ms;
AVIF (rav1e at speed 10) costs seconds, so it's only made when asked for (`format=avif`), never
by `Accept` negotiation.

A compute-optimised vCPU on AWS costs about $0.04 an hour ($0.085/h for a c6i.large's two), so
$0.000011 per CPU-second. A typical source with three variants (a thumbnail and a large image in
WebP, one AVIF) is about 1.5–3.5 CPU-seconds on this machine: **$0.00002–0.00004** of CPU. Add a
GET of the source ($0.0000004), a PUT per variant ($0.000005 each) and a month of storage for
~300 KB of variants ($0.000007): about **$0.00006** per source image, against **$0.005** charged.
The margin is ~98%, and stays positive up to roughly 300 AVIF variants (or 2000 WebP ones) of
one source in a month. `image_usage.variants` counts the variants made per source per month
(returned as `variants` by the usage API, not billed) so that abuse is visible; per-node CPU is
bounded by `ROLL_S3_IMAGE_CONCURRENCY` (half the cores, at most 4, by default) whatever the
request rate, with requests waiting at most 10 s for a slot before `SlowDown`.

The dashboard's own previews (the object browser's 200 px thumbnails) are renditions like any
other, cached the same way, but aren't counted as transformations: browsing a bucket shouldn't
cost anything.

### A transformed image served from the CDN, end to end

The developer API's renditions (`/storage/v1/render/image/...`, `getPublicUrl(path, {transform})`)
go through the CDN like its plain public reads ([api.md](api.md#cdn)). Who pays what, for one
hero image (an 8 MP JPEG source, served as a 1200-wide WebP of **73 KB**) requested **1M times a
month** through the CDN:

| step | customer pays | our cost |
|---|---|---|
| make the rendition, once per source blob and transformation | $0.005 (one source image a month, after 100 free) | ~$0.00006 (0.2 CPU-s, a PUT, a month of 73 KB) |
| each edge miss (an origin pull) | a class B request, $0.40/M | origin egress 73 KB × $0.05–0.09/GB, plus a GET of the derived blob (gateway cache: often none) |
| each edge hit | 73 KB of cached egress at $0.03/GB | Cloudflare at ~$0.005/GB (assumed, above) + Workers requests |

At 1M requests that's 73 GB served: **$2.19 of cached egress + $0.005** for the transformation,
plus class B for the misses. Our cost is ~$0.37 of Cloudflare plus the misses' egress, so the
margin is the cached-egress margin above, driven by the byte hit rate. What sets the hit rate
for renditions:

- **Copies per edge location.** Each transformation is its own copy, and with no `format`
  (WebP by `Accept`) each has two (WebP and the source's format; the edge keys them apart, as
  Cloudflare's cache ignores `Vary`). A few hundred locations × 2 copies is ~600 misses for a
  hot image: 99.9%+ at 1M requests.
- **TTL.** Mutable URLs live `mutable_ttl_s` at the edge (60 s by default): a busy location
  refetches each copy every minute. Changes are purged by tag anyway, so raising it (an hour, a
  day) costs no freshness; at 60 s and 1M requests a month spread thinly, misses can reach
  tens of percent. **Immutable URLs** (`/storage/v1/v/<token>/…?width=`, from `roll.publicUrl(…,
  {immutable: true})`) stay a year and never need a purge: misses are just the first request per
  location and copy.
- **Signed renditions** are cached in a private namespace per transformation (tokens don't split
  copies, their bound transformation does), so many users' signed URLs for the same thumbnail
  share one copy.

Worked example at volume egress ($0.05/GB) for the 73 KB WebP: with immutable URLs (~600 misses,
44 MB pulled) our cost is $0.37 of Cloudflare + $0.002 of origin egress against $2.195 billed
(**~83%**); with 60 s mutable URLs at a 90% byte hit rate it's $0.37 + $0.37 against $2.19 + $0.04
of class B (**~67%**); a rendition requested once per location per minute (a 50% hit rate) is
break-even, as for any cached egress. The transformation itself is never the problem: $0.005
against $0.00006.

### Derived images aren't billed as storage

They're our cache, not the customer's data: we decide when to drop them (30 days unused,
`ROLL_S3_DERIVED_EXPIRY_DAYS`), they're shared by every bucket and branch that shares the source
blob (so there's no one bucket to bill), and they're small (the 1200-wide WebP of an 8 MP photo
is ~1/8 of the source). At $0.023/GB-month, 1000 sources with 300 KB of variants each cost us
$0.007 a month, 0.14% of the $5 they bring in. Charging for them would make bills depend on
cache behaviour customers can't see or control, for no meaningful revenue.
