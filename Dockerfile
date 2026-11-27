# syntax=docker/dockerfile:1

FROM rust:1-slim-trixie AS build
WORKDIR /src
# Build dependencies first so this layer stays cached when only our code changes.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src
COPY src ./src
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 radiant
COPY --from=build /src/target/release/radiant /usr/local/bin/radiant
USER radiant
ENV RADIANT_LISTEN=0.0.0.0:8080 RADIANT_LOG_FORMAT=json
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/radiant"]
