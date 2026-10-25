# ===== Build stage =====
FROM rust:1.98-alpine3.24 AS builder

WORKDIR /app

# Mirror acceleration (optional). Defaults to official Alpine sources.
# China users can override at build time:
#   docker build --build-arg APK_MIRROR=mirrors.aliyun.com \
#                --build-arg CARGO_REGISTRY=rsproxy.cn .
ARG APK_MIRROR=""
ARG CARGO_REGISTRY=""

RUN if [ -n "$APK_MIRROR" ]; then \
      sed -i "s|dl-cdn.alpinelinux.org|${APK_MIRROR}|g" /etc/apk/repositories; \
    fi \
    && apk add --no-cache \
    ca-certificates \
    gcc \
    musl-dev

# Optional cargo registry override (e.g. rsproxy sparse index)
RUN if [ -n "$CARGO_REGISTRY" ]; then \
      mkdir -p /usr/local/cargo && printf '%s\n' \
      '[source.crates-io]' \
      'replace-with = "mirror-sparse"' \
      '[source.mirror]' \
      "registry = \"https://${CARGO_REGISTRY}/crates.io-index\"" \
      '[source.mirror-sparse]' \
      "registry = \"sparse+https://${CARGO_REGISTRY}/index/\"" \
      '[registries.mirror]' \
      "index = \"https://${CARGO_REGISTRY}/crates.io-index\"" \
      '[net]' \
      'git-fetch-with-cli = true' \
      > /usr/local/cargo/config.toml; \
    fi

# Cargo.lock is optional on first build; wildcard avoids failure if absent
COPY Cargo.toml Cargo.lock* ./

# Create dummy source to cache dependency builds
RUN mkdir -p src && echo "fn main() {}" > src/main.rs
RUN cargo build --release || true
RUN rm -rf src

# Copy real source
COPY src ./src
COPY migrations ./migrations
COPY build.rs ./
RUN touch src/main.rs

ENV SQLX_OFFLINE=true
# Disable incremental compilation: container builds once triggered a rustc ICE
ENV CARGO_INCREMENTAL=0
RUN cargo build --release --locked

# ===== Runtime stage =====
# Alpine/musl runtime; the builder emits a static musl binary.
FROM alpine:3.24

# Version label (passed by CI: --build-arg VERSION=x.y.z)
ARG VERSION=""
LABEL org.opencontainers.image.version="${VERSION}"

ARG APK_MIRROR=""
RUN if [ -n "$APK_MIRROR" ]; then \
      sed -i "s|dl-cdn.alpinelinux.org|${APK_MIRROR}|g" /etc/apk/repositories; \
    fi \
    && apk add --no-cache \
    ca-certificates \
    ffmpeg

# Runs as root (bind-mounted data/media/logs dirs are created by docker as root;
# a non-root user would lack write permission and panic; for a personal bot,
# container isolation is considered sufficient).

WORKDIR /app

COPY --from=builder /app/target/release/tgsrssr /usr/local/bin/tgsrssr

ENV DB_PATH=/app/data/rss_bot.db
ENV MEDIA_DIR=/app/media
ENV LOG_DIR=/app/logs

CMD ["tgsrssr"]
