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
