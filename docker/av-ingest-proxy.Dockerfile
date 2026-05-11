FROM rust:1.86-bookworm AS build

WORKDIR /src
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates cmake git pkg-config \
  && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN cargo build --release -p av-ingest-proxy

FROM debian:bookworm-slim

RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates \
  && rm -rf /var/lib/apt/lists/* \
  && useradd --system --home-dir /var/lib/av-ingest-proxy --create-home --shell /usr/sbin/nologin av-ingest-proxy

COPY --from=build /src/target/release/av-ingest-proxy /usr/local/bin/av-ingest-proxy

USER av-ingest-proxy
EXPOSE 8444
ENTRYPOINT ["/usr/local/bin/av-ingest-proxy"]
