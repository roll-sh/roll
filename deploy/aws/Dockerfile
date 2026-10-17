# roll tasks: one image for every role (ROLL_TASKS_ROLE): node and proxy for the log store
# (deploy/aws/tasks.yaml), api and worker for DynamoDB (tasks-dynamodb.yaml). On Lambda it
# serves HTTP (it sees AWS_LAMBDA_RUNTIME_API).
#
#   docker buildx build --platform linux/arm64 -f deploy/aws/Dockerfile -t roll-tasks .
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
ENV CARGO_PROFILE_RELEASE_LTO=thin CARGO_PROFILE_RELEASE_CODEGEN_UNITS=4 CARGO_PROFILE_RELEASE_STRIP=true
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p tasks --features lambda --bin roll-tasks \
    && cp target/release/roll-tasks /roll-tasks

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /roll-tasks /roll-tasks
ENV ROLL_TASKS_LISTEN=0.0.0.0:8080 ROLL_TASKS_DATA=/tmp/roll-tasks RUST_LOG=info
ENTRYPOINT ["/roll-tasks"]
