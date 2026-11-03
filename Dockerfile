# syntax=docker/dockerfile:1
FROM rust:1.99-slim-trixie AS build
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends cmake clang && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && cp target/release/rail /rail

FROM debian:trixie-slim
LABEL org.opencontainers.image.source="https://github.com/HelgeSverre/rail-rs" \
      org.opencontainers.image.description="RAIL: Redundant Array of Inexpensive Links" \
      org.opencontainers.image.licenses="MIT"
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 rail && mkdir /data && chown rail /data
COPY --from=build /rail /usr/local/bin/rail
USER rail
# The HEALTHCHECK reads the port from RAIL_LISTEN, so it must always be set.
ENV RAIL_DB=/data/rail.db \
    RAIL_LISTEN=0.0.0.0:8080
VOLUME /data
HEALTHCHECK --interval=10s --timeout=3s CMD curl -fsS "http://127.0.0.1:${RAIL_LISTEN##*:}/healthz" || exit 1
ENTRYPOINT ["rail"]
CMD ["serve"]
