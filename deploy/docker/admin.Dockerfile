# Build stage
FROM rust:latest AS builder

# Install build dependencies including libclang for bindgen
RUN apt-get update && apt-get install -y \
    clang \
    libclang-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .

# Build only the admin binary
RUN cargo build --release --bin chronik-admin

# Runtime stage
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/chronik-admin /usr/local/bin/

# Create non-root user
RUN useradd -m -u 1000 chronik
USER chronik

EXPOSE 8080 8081

ENTRYPOINT ["chronik-admin"]