FROM rust:1.91-bookworm AS builder

WORKDIR /app

# Install build dependencies for RocksDB and protobuf
RUN apt-get update && apt-get install -y \
    protobuf-compiler \
    clang \
    libclang-dev \
    build-essential \
    && rm -rf /var/lib/apt/lists/*

# Optimize build by caching dependencies
COPY Cargo.toml Cargo.lock ./
# Note: This is a common trick to cache dependencies, but since we have multiple members,
# we'd need to dummy them all out. For now, let's stick to a simpler build but keep the deps layer if possible.
# COPY dfs/common/Cargo.toml dfs/common/Cargo.toml
# ... (omitted for brevity, will copy all first)

# Copy the entire project
COPY . .

# Build the project with optimizations
RUN cargo build --release && \
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
