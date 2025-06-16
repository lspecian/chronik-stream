# Makefile for Chronik Stream Operator

# Variables
OPERATOR_IMAGE ?= chronik/operator
OPERATOR_TAG ?= latest
NAMESPACE ?= chronik-operator-system

# Build targets
.PHONY: build
build:
	cargo build --release --bin chronik-operator

.PHONY: test
test:
	cargo test -p chronik-operator

.PHONY: test-integration
test-integration:
	cargo test -p chronik-operator --test integration -- --ignored

# Docker targets
.PHONY: docker-build
docker-build:
	docker build -f crates/chronik-operator/Dockerfile -t $(OPERATOR_IMAGE):$(OPERATOR_TAG) .

.PHONY: docker-push
docker-push: docker-build
	docker push $(OPERATOR_IMAGE):$(OPERATOR_TAG)

# Kubernetes targets
.PHONY: install-crds
install-crds:
	kubectl apply -f deploy/operator/crd.yaml

.PHONY: deploy
deploy: install-crds
	kubectl apply -f deploy/operator/operator.yaml

.PHONY: undeploy
undeploy:
	kubectl delete -f deploy/operator/operator.yaml
	kubectl delete -f deploy/operator/crd.yaml

.PHONY: logs
logs:
	kubectl -n $(NAMESPACE) logs -f deployment/chronik-operator

# Development targets
.PHONY: run-local
run-local:
	RUST_LOG=debug cargo run --bin chronik-operator

.PHONY: fmt
fmt:
	cargo fmt --package chronik-operator

.PHONY: lint
lint:
	cargo clippy --package chronik-operator -- -D warnings

.PHONY: clean
clean:
	cargo clean

# Kind cluster for testing
.PHONY: kind-create
kind-create:
	kind create cluster --name chronik-test

.PHONY: kind-delete
kind-delete:
	kind delete cluster --name chronik-test

.PHONY: kind-load
kind-load: docker-build
	kind load docker-image $(OPERATOR_IMAGE):$(OPERATOR_TAG) --name chronik-test

# Help target
.PHONY: help
help:
	@echo "Chronik Stream Operator Makefile"
	@echo ""
	@echo "Build targets:"
	@echo "  build              Build the operator binary"
	@echo "  test               Run unit tests"
	@echo "  test-integration   Run integration tests (requires k8s cluster)"
	@echo ""
	@echo "Docker targets:"
	@echo "  docker-build       Build the operator Docker image"
	@echo "  docker-push        Push the operator Docker image"
	@echo ""
	@echo "Kubernetes targets:"
	@echo "  install-crds       Install CRDs only"
	@echo "  deploy             Deploy the operator"
	@echo "  undeploy           Remove the operator"
	@echo "  logs               Show operator logs"
	@echo ""
	@echo "Development targets:"
	@echo "  run-local          Run operator locally"
	@echo "  fmt                Format code"
	@echo "  lint               Run clippy lints"
	@echo "  clean              Clean build artifacts"
	@echo ""
	@echo "Kind targets:"
	@echo "  kind-create        Create a kind cluster for testing"
	@echo "  kind-delete        Delete the kind test cluster"
	@echo "  kind-load          Load operator image into kind"