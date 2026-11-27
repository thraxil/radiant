# syntax=docker/dockerfile:1

FROM rust:1-slim-trixie AS build
WORKDIR /src
# Build dependencies first so this layer stays cached when only our code changes.
# rust-toolchain.toml is left out on purpose: it would pull in dev-only components (rust-analyzer
# etc.). The image's stable toolchain is used instead.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src
COPY src ./src
COPY migrations ./migrations
# Release tag (e.g. r12) baked into the binary: `radiant --version` and radiant_build_info.
# Declared after the dependency build so changing it doesn't invalidate that layer.
ARG RADIANT_RELEASE=""
RUN touch src/main.rs src/lib.rs && RADIANT_RELEASE="$RADIANT_RELEASE" cargo build --release --locked

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 radiant \
    && mkdir /data && chown radiant /data
COPY --from=build /src/target/release/radiant /usr/local/bin/radiant
USER radiant
ENV RADIANT_LISTEN=0.0.0.0:8080 \
    RADIANT_LOG_FORMAT=json \
    RADIANT_DB_PATH=/data/radiant.db \
    RADIANT_FS_ROOT=/data/blobs
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/radiant"]
