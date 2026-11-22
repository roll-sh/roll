# Private networking and customer keys: design

`## Next` item 17 in [README.md](README.md). Status: design, nothing built.

## Why

A company putting search behind a production app asks two questions on a security review. Can the
traffic stay off the internet? Can we hold the key to our data? Both incumbents answer yes:
- **Elastic Cloud:** PrivateLink traffic filters, filtered by VPC endpoint id, and a customer key
  for deployments.
- **AWS OpenSearch Service:** VPC domains and KMS.

roll answers neither today:
- **Public data plane.** Each cluster's data plane is public. Its only network control is the
  `ip_allowlist` (README, "IP allowlists"), checked before credentials, at most 64 CIDRs.
- **Bucket-only encryption.** Segments and WAL objects are encrypted only by the bucket's own
  server-side encryption, under roll's keys.
- **Allowlists break roll's own products.** On roll cloud, roll apps and previews send from
  per-task public addresses. An owner who sets an allowlist cuts off their own roll apps unless
  they allow all of roll's egress, which admits everyone else on roll too.

## 1. Admitting roll's own products by identity, not address

- **The entry.** `ip_allowlist` takes a second kind of entry: `{"link": "apps:svc_x"}` or
  `{"link": "previews:<repo>"}`. It admits a request whose credential is a key roll's link provider
  issued for that service or repository (#677's provider; the key records its link in its
  metadata), from any address.
- **No new pre-auth cost.** The allowlist still runs before anything else. A request from outside
  the CIDRs is refused at once unless it carries an `Authorization` header. If it does, the node
  looks up only the key's id, a hash lookup in the cluster's security state with no password hash
  verified yet. It admits the request only if that key is a link key the list names, and then
  authenticates it as usual. Anything else gets the same 403 as today, so the list leaks nothing
  about which keys exist.
- **What it admits.** Only link keys. A person's own key or a browser key never passes by
  identity: allowlists stay a network control, and link keys are scoped and issued by roll.
  Revoking the link revokes the key, which closes the door too.
- **The dashboard** lists the cluster's links under the allowlist, as "roll apps service
  `svc_x`", with a switch each.

## 2. Private endpoints (AWS PrivateLink first)

- **The endpoint service.** roll cloud runs, per region, a VPC endpoint service on the search NLB.
  A customer creates an interface endpoint in their VPC to it, and their DNS name for the cluster
  resolves to it.
- **The endpoint id.** The NLB already puts PROXY protocol v2 in front of every connection, and
  `roll-edge`'s parser already expects the AWS TLV (`proxy_protocol.rs`). The parser keeps the
  VPC endpoint id: TLV type `0xEA`, subtype `0x01`, bounded by the existing 512-byte header cap.
  It hands the id to the traffic filter with the client's address.
- **Filtering by endpoint.** `ip_allowlist` takes `{"vpce": "vpce-0abc…"}`. With any `vpce` entry,
  a request that didn't arrive through a listed endpoint is refused, whatever its address. That's
  Elastic's PrivateLink traffic filter.
- **Private only.** `"private_only": true` on the cluster refuses everything that arrives over the
  public path, `/v1/search`'s console included. The console then works only from a browser inside
  the customer's network through the same endpoint. Making a cluster private takes `admin`.
- **The fleet.** A connection arriving at a node that doesn't own the cluster is forwarded with
  the client's address. The forwards are signed and already carry that address (architecture.md,
  "Client addresses and IP allowlists"). The endpoint id joins the signed fields, so the owner
  decides on what the edge saw.
- **Later:** GCP Private Service Connect and Azure Private Link use the same allowlist entry,
  given the provider's endpoint id from its own PROXY TLV.

## 3. A cluster key the customer holds

- **Envelope encryption.** Each cluster gets a data key (AES-256-GCM), and search-store encrypts
  every object it writes for the cluster: segments, live-docs, manifests, WAL batches, pins and
  state. Each object gets its own nonce, with the object's key as associated data, so an object
  moved or renamed doesn't decrypt.
- **Who wraps the data key.**
  - By default, roll's KMS key wraps it, so every cluster gets this whether or not it asks.
  - With `customer_key: "arn:aws:kms:…"`, the customer's KMS key wraps it instead, through a grant
    the customer gives roll's account.
  - roll keeps only the wrapped data key, in the cluster's catalog row. It never stores the
    customer key's material, which never leaves KMS.
- **Loading and revoking.** A load unwraps the data key once (one KMS call) and keeps it only in
  the loaded cluster's memory. Revoking the grant or disabling the key makes the next load fail
  with "the cluster's key isn't available". An already-loaded cluster unloads within the unwrap
  cache's lifetime (5 minutes), so a customer can crypto-shred a cluster by disabling their key.
- **The local disk cache.** It holds decrypted segments while a cluster is loaded, so search stays
  fast. It's encrypted with a per-node key that is wiped on restart, and a cluster's cache files
  are deleted when it unloads, as today.
- **Branches, snapshots and exports.**
  - A branch shares its parent's segments, so it shares the data key. A branch of a
    customer-keyed cluster is customer-keyed by the same key, and stops loading with its parent
    if the key is revoked.
  - Snapshot pins share segments in the same way.
  - Snapshots to the customer's own repository, and exports, are written decrypted into the
    customer's bucket, under that bucket's own encryption.
- **Cost.** Encrypting and decrypting is AES-GCM at several GB/s per core with AES-NI. The cost
  that matters is the first read of each segment into the cache, measured on the cold-start
  benchmark (item 16) before it ships. Branches, snapshots and GC still never copy data.
- **Migrating existing clusters.** A cluster with unencrypted objects keeps reading them. A
  background re-encryption (claimed per object, never rewriting a segment's id) moves it over,
  shown in the cluster's status, and GC deletes the old objects only once no manifest names them.

## Security notes

- **No new pre-auth parsing.** The identity entry adds a key-id lookup before authentication but
  never runs password hashing, so the cost before credentials stays as small as today.
- **Hiding a cluster.** A `vpce` entry or `private_only` makes the public endpoint answer exactly
  as for a cluster that doesn't exist, without even the traffic-filter 403.
- **Rights to the key.** Turning on `customer_key` needs `admin` and a working grant (tested at
  once). Turning it off re-wraps under roll's key and needs the customer key still to work.
- **Auditing.** Every unwrap is audited (the cluster's audit log, `customer_key_used`), and so is
  every refusal for a missing key.

## Phases

1. **Phase 1:** link identities in the allowlist. This is the cheapest and unblocks roll apps and
   previews on allowlisted clusters.
2. **Phase 2:** envelope encryption with roll's key, then customer keys. The cold-start numbers
   gate it.
3. **Phase 3:** AWS PrivateLink: the endpoint id from PROXY v2, `vpce` entries and `private_only`.
   GCP and Azure follow.

## Tests

- **Allowlist:** search-control `control.rs` tests:
  - an allowlisted cluster admits its apps link key from any address, and refuses a person's key
    from there;
  - a revoked link stops at once.
- **Endpoints:** a fleet test checks that a forwarded request keeps its endpoint id.
- **Keys:**
  - search-store tests cover object encryption: moving an object fails to decrypt, and a revoked
    key stops a load;
  - a fake KMS stands in for AWS. No test calls a real KMS.
- **PROXY parsing:** roll-edge unit tests read the AWS TLV and refuse a malformed one.
