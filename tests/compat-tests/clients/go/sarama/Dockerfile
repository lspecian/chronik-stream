FROM golang:1.21-alpine AS builder

WORKDIR /app

# Copy go files
COPY go.mod go.sum* ./
COPY test.go ./

# Download dependencies
RUN go mod init sarama-test 2>/dev/null || true
RUN go get github.com/IBM/sarama@v1.42.1
RUN go get github.com/google/uuid@v1.3.0

# Build
RUN go build -o test test.go

FROM alpine:latest

RUN apk --no-cache add ca-certificates

WORKDIR /app

COPY --from=builder /app/test /app/test

# Create results directory
RUN mkdir -p /results

ENV BOOTSTRAP_SERVERS=chronik:9092

CMD ["/app/test"]