# roll nosql docs

roll nosql is a wide-column database that behaves like ScyllaDB 2026 and Apache Cassandra 5.0.
Drivers, `cqlsh`, the DynamoDB SDKs (through Alternator) and ScyllaDB's `nodetool` all work
against it unchanged. It stores data on S3, scales to zero, and branches instantly.

| doc | |
|---|---|
| [architecture.md](architecture.md) | how it is built: dialects, the virtual cluster, tenancy, LSM storage on S3, engine, Alternator |
| [compatibility.md](compatibility.md) | what 1:1 covers, reference images, how it is verified, what is out of scope |
| [preview-environments.md](preview-environments.md) | branches per pull request, imports from production, masking, local dev and CI |
| [local-dev.md](local-dev.md) | the `roll/nosql` Docker image: configuration mapping, compose, Testcontainers, CI recipes |
| [contracts.md](contracts.md) | ports, ids, crate boundaries, limits, tests, the `/v1/nosql` API |
| [plan.md](plan.md) | phases and conformance status |
| [coverage.md](coverage.md) | the coverage audit: what each crate's tests cover, known gaps and why |
