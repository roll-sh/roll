# kafka-protocol 0.18.0, patched

This is upstream `kafka-protocol` 0.18.0 from crates.io (git `d4032ec9fd5a9f432543683948b38d7a07436590`),
used through `[patch.crates-io]` in the root `Cargo.toml`. Only the decoders changed, each marked
`roll:`:

- `src/protocol/types.rs`: `Array` and `CompactArray` reserved capacity for the element count the
  peer sent, and `Bytes` allocated the declared length before checking it was there. One small
  request could make the broker allocate tens of GB and abort. Capacity is now bounded by the bytes
  left and a fixed cap, and `Bytes` checks the length first.
- `src/records.rs`: the same for the record count of a batch and the header count of a record.

Drop this copy once upstream bounds these allocations.
