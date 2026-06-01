FROM rust:1.91-bookworm AS builder

WORKDIR /app

# Install build dependencies for RocksDB and protobuf
RUN apt-get update && apt-get install -y \
    protobuf-compiler \
    clang \
    libclang-dev \
    build-essential \
    && rm -rf /var/lib/apt/lists/*

# Inject corporate CA certificates (needed for SSL-inspecting proxies)
COPY corporate_ca.pem /usr/local/share/ca-certificates/corporate_ca.crt
RUN update-ca-certificates

# Install sccache for faster incremental builds
RUN ARCH=$(uname -m) && \
    if [ "$ARCH" = "aarch64" ]; then SCCACHE_ARCH="aarch64-unknown-linux-musl"; else SCCACHE_ARCH="x86_64-unknown-linux-musl"; fi && \
    curl -L "https://github.com/mozilla/sccache/releases/download/v0.9.1/sccache-v0.9.1-${SCCACHE_ARCH}.tar.gz" \
    | tar xz --strip-components=1 -C /usr/local/bin "sccache-v0.9.1-${SCCACHE_ARCH}/sccache" && \
    chmod +x /usr/local/bin/sccache

ENV RUSTC_WRAPPER=sccache
ENV SCCACHE_DIR=/sccache
# Allow cargo to work behind SSL-inspecting corporate proxies
ENV CARGO_HTTP_SSL_NO_VERIFY=true
ENV CARGO_UNSTABLE_DISABLE_TLS=0

# Optimize build by caching dependencies
COPY Cargo.toml Cargo.lock ./
# Note: This is a common trick to cache dependencies, but since we have multiple members,
# we'd need to dummy them all out. For now, let's stick to a simpler build but keep the deps layer if possible.
# COPY dfs/common/Cargo.toml dfs/common/Cargo.toml
# ... (omitted for brevity, will copy all first)

# Copy the entire project
COPY . .

# Build the project with optimizations, using sccache mount cache for faster rebuilds
RUN --mount=type=cache,target=/sccache \
    cargo build --release && \
    strip target/release/master && \
    strip target/release/chunkserver && \
    strip target/release/config_server && \
    strip target/release/dfs_cli && \
    strip target/release/s3-server

# Runtime stage
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    libstdc++6 \
    curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy binaries from builder
COPY --from=builder /app/target/release/master /app/master
COPY --from=builder /app/target/release/chunkserver /app/chunkserver
COPY --from=builder /app/target/release/config_server /app/config_server
COPY --from=builder /app/target/release/dfs_cli /app/dfs_cli
COPY --from=builder /app/target/release/s3-server /app/s3-server

# Create storage directory
RUN mkdir -p /data

# Default port documentation
EXPOSE 50051 50052 8080 9000

CMD ["/app/master"]
