# Build stage
FROM rust:latest AS builder

# Install build dependencies including libclang for bindgen
RUN apt-get update && apt-get install -y \
    clang \
    libclang-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .

# Build only the ingest binary
RUN cargo build --release --bin chronik-ingest

# Runtime stage
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/chronik-ingest /usr/local/bin/

# Create non-root user
RUN useradd -m -u 1000 chronik
USER chronik

# Create data directory
RUN mkdir -p /home/chronik/data
VOLUME ["/home/chronik/data"]

EXPOSE 9092

ENTRYPOINT ["chronik-ingest"]