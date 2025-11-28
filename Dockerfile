FROM rust:latest as builder

WORKDIR /app

# Install protobuf compiler
RUN apt-get update && apt-get install -y protobuf-compiler && rm -rf /var/lib/apt/lists/*

# Copy the entire project
COPY . .

# Build the project
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy binaries from builder
COPY --from=builder /app/target/release/master /app/master
COPY --from=builder /app/target/release/chunkserver /app/chunkserver
COPY --from=builder /app/target/release/dfs_cli /app/dfs_cli

# Create storage directory
RUN mkdir -p /data

CMD ["/bin/bash"]
