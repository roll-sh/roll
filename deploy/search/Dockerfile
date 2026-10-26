# roll search in one small image: search node (`rolld --search-only`) and front proxy
# (`search-proxy`, also the Lambda function URL runtime). Build from the repository root:
#   docker build -f deploy/search/Dockerfile -t roll-search .
# For AWS (Graviton): arm64 first. ECS takes a multi-arch image; Lambda needs a single-arch one
# without attestations:
#   docker buildx build --platform linux/arm64 --provenance=false -f deploy/search/Dockerfile -t roll-search .
#   docker buildx build --platform linux/arm64,linux/amd64 -f deploy/search/Dockerfile -t roll-search .
# The build stage runs on the builder's own platform and cross-compiles for the other (gcc cross
# toolchains for the C in SQLite, zstd, lz4, ring, aws-lc and mimalloc), so an x86 machine builds
# the arm64 image at native speed, without QEMU.
FROM --platform=$BUILDPLATFORM rust:1-bookworm AS build
ARG TARGETARCH
ARG BUILDARCH
# `dev` builds faster for tests; images you deploy use `release`.
ARG PROFILE=release
ARG CARGO_BUILD_JOBS=4
# rolld also links roll tasks, whose SQLite session support runs bindgen (libclang) at build time.
RUN set -eu; \
    case "$TARGETARCH" in \
      arm64) triple=aarch64-unknown-linux-gnu; gnu=aarch64-linux-gnu; deb=arm64 ;; \
      amd64) triple=x86_64-unknown-linux-gnu; gnu=x86-64-linux-gnu; deb=amd64 ;; \
      *) echo "unsupported TARGETARCH $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    pkgs=libclang-dev; \
    if [ "$TARGETARCH" != "$BUILDARCH" ]; then pkgs="$pkgs gcc-$gnu g++-$gnu libc6-dev-$deb-cross"; fi; \
    apt-get update && apt-get install -y --no-install-recommends $pkgs && rm -rf /var/lib/apt/lists/*; \
    rustup target add "$triple"; \
    echo "$triple" > /rust-target; \
    # The runtime image runs no commands (that would need the target CPU or QEMU): its files are
    # gathered here. tini from the target's Debian package, the system CA roots, a home for the user.
    dpkg --add-architecture "$deb"; apt-get update; \
    cd /tmp && apt-get download "tini:$deb" && mkdir -p /rootfs/data /rootfs/etc/ssl/certs && dpkg-deb -x tini_*.deb /rootfs \
    && rm -rf /rootfs/usr/share tini_*.deb /var/lib/apt/lists/* \
    && cp /etc/ssl/certs/ca-certificates.crt /rootfs/etc/ssl/certs/
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
# Patched dependencies of other workspace crates; Cargo resolves the whole workspace.
COPY vendor vendor
ENV CARGO_PROFILE_RELEASE_LTO=thin CARGO_PROFILE_RELEASE_CODEGEN_UNITS=4 CARGO_PROFILE_RELEASE_STRIP=true \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ \
    AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar \
    BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_linux_gnu=--sysroot=/usr/aarch64-linux-gnu \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc \
    CXX_x86_64_unknown_linux_gnu=x86_64-linux-gnu-g++ \
    AR_x86_64_unknown_linux_gnu=x86_64-linux-gnu-ar \
    BINDGEN_EXTRA_CLANG_ARGS_x86_64_unknown_linux_gnu=--sysroot=/usr/x86_64-linux-gnu
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,id=roll-search-target-${TARGETARCH} \
    set -eu; triple=$(cat /rust-target); \
    # A native build uses the platform's own compilers (the cross names above don't exist).
    if [ "$TARGETARCH" = "$BUILDARCH" ]; then \
      unset CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER CC_aarch64_unknown_linux_gnu CXX_aarch64_unknown_linux_gnu AR_aarch64_unknown_linux_gnu BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_linux_gnu \
            CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER CC_x86_64_unknown_linux_gnu CXX_x86_64_unknown_linux_gnu AR_x86_64_unknown_linux_gnu BINDGEN_EXTRA_CLANG_ARGS_x86_64_unknown_linux_gnu; \
    fi; \
    CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS} cargo build --locked --target "$triple" --profile "$PROFILE" -p rolld -p search-proxy --bin rolld --bin search-proxy; \
    dir=$([ "$PROFILE" = dev ] && echo debug || echo "$PROFILE"); \
    mkdir -p /rootfs/usr/local/bin; \
    cp "target/$triple/$dir/rolld" "target/$triple/$dir/search-proxy" /rootfs/usr/local/bin/

FROM debian:bookworm-slim
# Amazon RDS's CA bundle, so metadata on RDS works with sslmode=require (added to the system roots).
ADD --chmod=644 https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem /etc/roll/rds-global-bundle.pem
COPY --from=build --chown=10001:10001 /rootfs/data /data
COPY --from=build /rootfs/usr /usr
COPY --from=build /rootfs/etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
# An unprivileged user without a passwd entry (nothing here looks one up).
USER 10001:10001
WORKDIR /data
ENV HOME=/data \
    ROLL_HOME=/data \
    ROLL_SEARCH_ONLY=1 \
    ROLL_API=0.0.0.0:7070 \
    ROLL_SEARCH_HTTP=0.0.0.0:9200 \
    ROLL_SEARCH_META_CA=/etc/roll/rds-global-bundle.pem \
    RUST_LOG=info
EXPOSE 9200 7070 8081
VOLUME /data
# tini forwards SIGTERM so a stopping node drains: it publishes `draining`, flushes every cluster
# and hands its lease back; a stopping proxy finishes requests in progress.
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["rolld"]
