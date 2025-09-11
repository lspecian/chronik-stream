# Optimized multi-stage Dockerfile with dependency caching
# This version is much faster than the original

# Cache dependencies in a separate stage
FROM rust:slim AS dependencies

WORKDIR /usr/src/chronik

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy only Cargo files to cache dependencies
COPY Cargo.toml Cargo.lock ./
COPY crates/chronik-admin/Cargo.toml crates/chronik-admin/
COPY crates/chronik-auth/Cargo.toml crates/chronik-auth/
COPY crates/chronik-backup/Cargo.toml crates/chronik-backup/
COPY crates/chronik-benchmarks/Cargo.toml crates/chronik-benchmarks/
COPY crates/chronik-cli/Cargo.toml crates/chronik-cli/
COPY crates/chronik-common/Cargo.toml crates/chronik-common/
COPY crates/chronik-config/Cargo.toml crates/chronik-config/
COPY crates/chronik-controller/Cargo.toml crates/chronik-controller/
COPY crates/chronik-janitor/Cargo.toml crates/chronik-janitor/
COPY crates/chronik-monitoring/Cargo.toml crates/chronik-monitoring/
COPY crates/chronik-operator/Cargo.toml crates/chronik-operator/
COPY crates/chronik-protocol/Cargo.toml crates/chronik-protocol/
COPY crates/chronik-query/Cargo.toml crates/chronik-query/
COPY crates/chronik-search/Cargo.toml crates/chronik-search/
COPY crates/chronik-server/Cargo.toml crates/chronik-server/
COPY crates/chronik-storage/Cargo.toml crates/chronik-storage/

# Create dummy source files to build dependencies
RUN for dir in crates/*/; do \
        mkdir -p "$dir/src" && \
        echo "fn main() {}" > "$dir/src/main.rs" && \
        echo "#![allow(unused)]" > "$dir/src/lib.rs"; \
    done

# Build dependencies only (this layer is cached)
RUN cargo build --release --bin chronik-server || true

# Build stage
FROM dependencies AS builder

# Copy actual source code
COPY crates/ ./crates/

# Touch main.rs to force rebuild of our code only
RUN touch crates/chronik-server/src/main.rs

# Build with optimizations
ENV CARGO_BUILD_JOBS=4 \
    CARGO_INCREMENTAL=0 \
    RUSTFLAGS="-C opt-level=2 -C codegen-units=16"

RUN cargo build --release --bin chronik-server

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates netcat-traditional && \
    rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -m -u 1000 chronik

# Copy binary from builder
COPY --from=builder /usr/src/chronik/target/release/chronik-server /usr/local/bin/chronik-server

# Create data directory
RUN mkdir -p /data && chown chronik:chronik /data

# Switch to non-root user
USER chronik

# Data volume
VOLUME ["/data"]

# Expose ports
EXPOSE 9092 9093

# Health check
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD nc -z localhost 9092 || exit 1

# Run server
ENTRYPOINT ["chronik-server"]
CMD ["--bind-addr", "0.0.0.0", "--data-dir", "/data", "standalone"]