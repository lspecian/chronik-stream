FROM golang:1.21-alpine AS builder

# Install build dependencies for librdkafka
RUN apk add --no-cache \
    gcc \
    g++ \
    musl-dev \
    librdkafka-dev \
    pkgconf

WORKDIR /app

# Copy go files
COPY go.mod go.sum* ./
COPY test.go ./

# Initialize module and get dependencies
RUN go mod init confluent-test 2>/dev/null || true
RUN go get github.com/confluentinc/confluent-kafka-go/v2/kafka
RUN go get github.com/google/uuid@v1.3.0

# Build with CGO for librdkafka
RUN CGO_ENABLED=1 go build -tags musl -o test test.go

FROM alpine:latest

# Install runtime dependencies
RUN apk add --no-cache librdkafka

WORKDIR /app

COPY --from=builder /app/test /app/test

# Create results directory
RUN mkdir -p /results

ENV BOOTSTRAP_SERVERS=chronik:9092

CMD ["/app/test"]