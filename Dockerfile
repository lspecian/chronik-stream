# Multi-stage build for Chronik Stream
# Supports both x86_64 and aarch64 architectures

# Build stage
FROM rust:1.75-slim as builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Set working directory
WORKDIR /usr/src/chronik

# Copy manifest files
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/

# Build in release mode
RUN cargo build --release --bin chronik

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -m -u 1000 chronik

# Copy binary from builder
COPY --from=builder /usr/src/chronik/target/release/chronik /usr/local/bin/chronik

# Create data directory
RUN mkdir -p /data && chown chronik:chronik /data

# Switch to non-root user
USER chronik

# Set data directory as volume
VOLUME ["/data"]

# Expose default Kafka port
EXPOSE 9092

# Health check
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD nc -z localhost 9092 || exit 1

# Default command
ENTRYPOINT ["chronik"]
CMD ["--bind-addr", "0.0.0.0", "--data-dir", "/data"]