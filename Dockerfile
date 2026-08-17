FROM rust:1-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake clang libclang-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .
RUN cargo build --release -p lavalink-server

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl python3 \
    && rm -rf /var/lib/apt/lists/* \
    # yt-dlp is optional (crates/server auto-disables its sources if missing at startup)
    # but installing it here keeps youtube/soundcloud/bandcamp/deezer usable out of the box.
    && curl -fsSL https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp -o /usr/local/bin/yt-dlp \
    && chmod +x /usr/local/bin/yt-dlp

WORKDIR /app
COPY --from=builder /build/target/release/lavalink-server /usr/local/bin/lavalink-server

EXPOSE 2333
ENTRYPOINT ["lavalink-server"]
# No default config is baked in — mount your own application.yml at /app/application.yml
# (see application.yml.example in the repo; the node refuses to start with an empty password).
