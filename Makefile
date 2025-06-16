.PHONY: help build test lint fmt clean docker-build docker-up docker-down release

# Default target
help:
	@echo "Chronik Stream Development Commands:"
	@echo "  make build       - Build all components"
	@echo "  make test        - Run all tests"
	@echo "  make lint        - Run linters (fmt + clippy)"
	@echo "  make fmt         - Format code"
	@echo "  make clean       - Clean build artifacts"
	@echo "  make docker-build - Build Docker images"
	@echo "  make docker-up   - Start services with docker-compose"
	@echo "  make docker-down - Stop services"
	@echo "  make release     - Build release binaries"
	@echo "  make bench       - Run benchmarks"
	@echo "  make docs        - Generate documentation"

# Build all components
build:
	cargo build --workspace

# Run all tests
test:
	cargo test --workspace

# Run linters
lint: fmt
	cargo clippy --workspace --all-targets -- -D warnings

# Format code
fmt:
	cargo fmt --all

# Clean build artifacts
clean:
	cargo clean
	rm -rf target/

# Build Docker images
docker-build:
	docker-compose build

# Start services with docker-compose
docker-up:
	docker-compose up -d

# Stop services
docker-down:
	docker-compose down

# Build release binaries
release:
	cargo build --workspace --release

# Run benchmarks
bench:
	cargo bench

# Generate documentation
docs:
	cargo doc --workspace --no-deps --open

# Install development tools
dev-setup:
	cargo install cargo-audit
	cargo install cargo-tarpaulin
	cargo install cargo-watch

# Watch for changes and rebuild
watch:
	cargo watch -x build

# Run security audit
audit:
	cargo audit

# Generate test coverage
coverage:
	cargo tarpaulin --out Html --all-features --workspace

# Start local development environment
dev: docker-up
	@echo "Development environment ready!"
	@echo "  Controller: localhost:9090"
	@echo "  Ingest:     localhost:9092"
	@echo "  Admin API:  localhost:8080"
	@echo "  Jaeger UI:  localhost:16686"
	@echo "  Prometheus: localhost:9091"
	@echo "  Grafana:    localhost:3001 (admin/admin)"

# Run integration tests
integration-test:
	@echo "Running integration tests..."
	@echo "Starting Docker daemon if needed..."
	@which docker > /dev/null || (echo "Docker not found. Please install Docker." && exit 1)
	@docker ps > /dev/null 2>&1 || (echo "Docker daemon not running. Please start Docker." && exit 1)
	RUST_LOG=chronik=debug,integration=info cargo test --test integration -- --test-threads=1

# Run specific integration test suite
integration-test-suite:
	@echo "Available test suites:"
	@echo "  make test-kafka        - Kafka compatibility tests"
	@echo "  make test-search       - Search integration tests"
	@echo "  make test-failure      - Failure recovery tests"
	@echo "  make test-performance  - Performance tests"
	@echo "  make test-multilang    - Multi-language client tests"

test-kafka:
	RUST_LOG=chronik=debug cargo test --test integration kafka_compatibility -- --nocapture

test-search:
	RUST_LOG=chronik=debug cargo test --test integration search_integration -- --nocapture

test-failure:
	RUST_LOG=chronik=debug cargo test --test integration failure_recovery -- --nocapture

test-performance:
	RUST_LOG=chronik=info cargo test --test integration performance -- --nocapture

test-multilang:
	RUST_LOG=chronik=debug cargo test --test integration multi_language_client -- --nocapture

# Clean up test containers
test-cleanup:
	@echo "Cleaning up test containers..."
	@docker ps -a | grep chronik-test | awk '{print $$1}' | xargs -r docker rm -f
	@docker ps -a | grep testcontainers | awk '{print $$1}' | xargs -r docker rm -f
	@docker volume prune -f

# Check code before committing
pre-commit: fmt lint test
	@echo "All checks passed!"

# Tag a new release
tag-release:
	@read -p "Version (e.g., v0.1.0): " version; \
	git tag -a $$version -m "Release $$version"; \
	echo "Tagged $$version - don't forget to push: git push origin $$version"