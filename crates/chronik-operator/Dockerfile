# Build stage
FROM rust:1.75-alpine AS builder

# Install build dependencies
RUN apk add --no-cache musl-dev openssl-dev

# Create app directory
WORKDIR /app

# Copy workspace files
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# Build the operator in release mode
RUN cargo build --release --bin chronik-operator

# Runtime stage
FROM alpine:3.19

# Install runtime dependencies
RUN apk add --no-cache ca-certificates

# Create non-root user
RUN adduser -D -u 65534 nonroot

# Copy binary from builder
COPY --from=builder /app/target/release/chronik-operator /usr/local/bin/chronik-operator

# Use non-root user
USER nonroot

# Expose ports
EXPOSE 8080 8081

# Set entrypoint
ENTRYPOINT ["/usr/local/bin/chronik-operator"]