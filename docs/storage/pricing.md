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

### Derived images aren't billed as storage

They're our cache, not the customer's data: we decide when to drop them (30 days unused,
`ROLL_S3_DERIVED_EXPIRY_DAYS`), they're shared by every bucket and branch that shares the source
blob (so there's no one bucket to bill), and they're small (the 1200-wide WebP of an 8 MP photo
is ~1/8 of the source). At $0.023/GB-month, 1000 sources with 300 KB of variants each cost us
$0.007 a month, 0.14% of the $5 they bring in. Charging for them would make bills depend on
cache behaviour customers can't see or control, for no meaningful revenue.
