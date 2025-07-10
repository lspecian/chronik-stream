# Build stage
FROM rust:latest AS builder

# Install build dependencies including libclang for bindgen
RUN apt-get update && apt-get install -y \
    clang \
    libclang-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .

# Build only the controller binary
RUN cargo build --release --bin chronik-controller

# Runtime stage
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/chronik-controller /usr/local/bin/

# Create non-root user
RUN useradd -m -u 1000 chronik
USER chronik

EXPOSE 9090

ENTRYPOINT ["chronik-controller"]