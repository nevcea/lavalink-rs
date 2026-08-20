# syntax=docker/dockerfile:1

FROM rust:1.95-bookworm AS builder

ARG YT_DLP_VERSION=2026.07.04
ARG YT_DLP_SHA256=495be29ff4d9d4e9be7eabdfef225221e5d5282e77f2f505abc6dca80349f3fd

RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake clang curl libclang-dev \
    && rm -rf /var/lib/apt/lists/*

RUN curl -fsSL "https://github.com/yt-dlp/yt-dlp/releases/download/${YT_DLP_VERSION}/yt-dlp" \
        -o /tmp/yt-dlp \
    && echo "${YT_DLP_SHA256}  /tmp/yt-dlp" | sha256sum -c - \
    && chmod +x /tmp/yt-dlp

WORKDIR /build
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo build --locked --release -p lavalink-server \
    && cp target/release/lavalink-server /tmp/lavalink-server

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates python3 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 lavalink \
    && useradd --system --uid 10001 --gid lavalink --home-dir /app --no-create-home lavalink

WORKDIR /app
COPY --from=builder /tmp/lavalink-server /usr/local/bin/lavalink-server
# yt-dlp is optional, but including the verified release keeps its sources usable.
COPY --from=builder /tmp/yt-dlp /usr/local/bin/yt-dlp

EXPOSE 2333
USER lavalink
ENTRYPOINT ["lavalink-server"]
# No default config is baked in — mount your own application.yml at /app/application.yml
# (see application.yml.example in the repo; the node refuses to start with an empty password).
