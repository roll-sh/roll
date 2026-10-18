FROM rust:1-slim AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/src/target \
    cargo build --release && rm -f target/release/roll-qtest-lapin
COPY src src
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/src/target \
    touch src/main.rs && cargo build --release && cp target/release/roll-qtest-lapin /usr/local/bin/

FROM debian:trixie-slim
COPY --from=build /usr/local/bin/roll-qtest-lapin /usr/local/bin/
ENTRYPOINT ["roll-qtest-lapin"]
