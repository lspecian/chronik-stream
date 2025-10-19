#!/bin/bash
set -e

# Chronik Stream Kafka Compatibility Test Runner
# This script orchestrates the execution of compatibility tests

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
RESULTS_DIR="$PROJECT_DIR/results"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Default values
CLIENT=""
CLEAN=false
BUILD=false

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --client)
            CLIENT="$2"
            shift 2
            ;;
        --clean)
            CLEAN=true
            shift
            ;;
        --build)
            BUILD=true
            shift
            ;;
        --help)
            echo "Usage: $0 [OPTIONS]"
            echo ""
            echo "Options:"
            echo "  --client NAME   Run tests for specific client"
            echo "  --clean         Clean up environment before running"
            echo "  --build         Force rebuild of containers"
            echo "  --help          Show this help message"
            echo ""
            echo "Available clients:"
            echo "  kafka-python"
            echo "  confluent-kafka"
            echo "  sarama"
            echo "  confluent-kafka-go"
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

log_info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

log_success() {
    echo -e "${GREEN}[SUCCESS]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

log_warning() {
    echo -e "${YELLOW}[WARNING]${NC} $1"
}

check_requirements() {
    log_info "Checking requirements..."
    
    if ! command -v docker &> /dev/null; then
        log_error "Docker is not installed"
        exit 1
    fi
    
    if ! command -v docker-compose &> /dev/null && ! docker compose version &> /dev/null; then
        log_error "Docker Compose is not installed"
        exit 1
    fi
    
    log_success "All requirements met"
}

# Determine docker-compose command
if docker compose version &> /dev/null; then
    DOCKER_COMPOSE="docker compose"
else
    DOCKER_COMPOSE="docker-compose"
fi

setup_environment() {
    log_info "Setting up test environment..."
    
    # Create results directory with timestamp
    TIMESTAMP=$(date +%Y%m%d_%H%M%S)
    TEST_RUN_DIR="$RESULTS_DIR/$TIMESTAMP"
    mkdir -p "$TEST_RUN_DIR"
    
    # Create a symlink to latest results
    ln -sfn "$TIMESTAMP" "$RESULTS_DIR/latest"
    
    export RESULTS_DIR="$TEST_RUN_DIR"
    
    log_success "Environment ready (results: $TEST_RUN_DIR)"
}

clean_environment() {
    if [ "$CLEAN" = true ]; then
        log_info "Cleaning up previous test environment..."
        cd "$PROJECT_DIR"
        $DOCKER_COMPOSE down -v 2>/dev/null || true
        docker system prune -f
        log_success "Environment cleaned"
    fi
}

build_containers() {
    cd "$PROJECT_DIR"
    
    if [ "$BUILD" = true ]; then
        log_info "Building all containers..."
        $DOCKER_COMPOSE build --no-cache
    else
        log_info "Building containers (if needed)..."
        $DOCKER_COMPOSE build
    fi
}

start_chronik() {
    log_info "Starting Chronik server..."
    cd "$PROJECT_DIR"
    
    # Start Chronik
    $DOCKER_COMPOSE up -d chronik
    
    # Wait for Chronik to be healthy
    log_info "Waiting for Chronik to be ready..."
    for i in {1..30}; do
        if $DOCKER_COMPOSE ps chronik | grep -q "healthy"; then
            log_success "Chronik is ready"
            return 0
        fi
        sleep 2
    done
    
    log_error "Chronik failed to become healthy"
    $DOCKER_COMPOSE logs chronik
    exit 1
}

run_client_test() {
    local client=$1
    local service_name=""
    
    case $client in
        kafka-python)
            service_name="kafka-python-client"
            ;;
        confluent-kafka)
            service_name="confluent-kafka-client"
            ;;
        sarama)
            service_name="sarama-client"
            ;;
        confluent-kafka-go)
            service_name="confluent-kafka-go-client"
            ;;
        *)
            log_error "Unknown client: $client"
            return 1
            ;;
    esac
    
    log_info "Running $client tests..."
    
    # Run the test container
    cd "$PROJECT_DIR"
    if $DOCKER_COMPOSE run --rm "$service_name"; then
        log_success "$client tests completed successfully"
        return 0
    else
        log_warning "$client tests failed"
        return 1
    fi
}

run_all_tests() {
    log_info "Running all compatibility tests..."
    
    local clients=("kafka-python" "confluent-kafka" "sarama" "confluent-kafka-go")
    local failed_clients=()
    
    for client in "${clients[@]}"; do
        if ! run_client_test "$client"; then
            failed_clients+=("$client")
        fi
        echo ""
    done
    
    if [ ${#failed_clients[@]} -gt 0 ]; then
        log_warning "The following clients had test failures:"
        for client in "${failed_clients[@]}"; do
            echo "  - $client"
        done
        return 1
    else
        log_success "All client tests passed!"
        return 0
    fi
}

collect_results() {
    log_info "Collecting test results..."
    
    cd "$PROJECT_DIR"
    
    # Copy results from container volumes
    if [ -d "$PROJECT_DIR/results" ]; then
        # Results should already be in the mounted volume
        ls -la "$TEST_RUN_DIR"/*.json 2>/dev/null || log_warning "No JSON results found"
    fi
}

generate_matrix() {
    log_info "Generating compatibility matrix..."
    
    cd "$PROJECT_DIR"
    
    if [ -f "scripts/gen-matrix.py" ]; then
        python3 scripts/gen-matrix.py \
            --input "$TEST_RUN_DIR" \
            --output "$PROJECT_DIR/docs/compatibility-matrix.md"
        
        log_success "Compatibility matrix generated: docs/compatibility-matrix.md"
    else
        log_warning "Matrix generator script not found"
    fi
}

show_summary() {
    echo ""
    echo "========================================="
    echo "Test Run Complete"
    echo "========================================="
    echo "Results: $TEST_RUN_DIR"
    
    if [ -f "$PROJECT_DIR/docs/compatibility-matrix.md" ]; then
        echo "Matrix: $PROJECT_DIR/docs/compatibility-matrix.md"
        echo ""
        echo "Quick Summary:"
        # Show a brief summary from the results
        for result in "$TEST_RUN_DIR"/*.json; do
            if [ -f "$result" ]; then
                client=$(basename "$result" .json | sed 's/-results//')
                passed=$(grep -o '"passed": true' "$result" | wc -l)
                failed=$(grep -o '"passed": false' "$result" | wc -l)
                echo "  $client: $passed passed, $failed failed"
            fi
        done
    fi
}

cleanup() {
    log_info "Cleaning up..."
    cd "$PROJECT_DIR"
    # Keep Chronik running for debugging if tests fail
    # $DOCKER_COMPOSE down
}

main() {
    echo "========================================="
    echo "Chronik Kafka Compatibility Test Runner"
    echo "========================================="
    echo ""
    
    # Change to project directory
    cd "$PROJECT_DIR"
    
    # Run steps
    check_requirements
    setup_environment
    clean_environment
    build_containers
    start_chronik
    
    # Run tests
    if [ -n "$CLIENT" ]; then
        run_client_test "$CLIENT"
    else
        run_all_tests
    fi
    
    # Collect and process results
    collect_results
    generate_matrix
    
    # Show summary
    show_summary
    
    # Determine exit code
    if [ -n "$CLIENT" ]; then
        # Single client mode - check if it passed
        result_file="$TEST_RUN_DIR/${CLIENT}-results.json"
        if [ -f "$result_file" ] && grep -q '"passed": false' "$result_file"; then
            exit 1
        fi
    else
        # All clients mode - check if any failed
        for result in "$TEST_RUN_DIR"/*.json; do
            if [ -f "$result" ] && grep -q '"passed": false' "$result"; then
                log_warning "Some tests failed. Check results for details."
                exit 1
            fi
        done
    fi
    
    log_success "All tests passed!"
    exit 0
}

# Trap cleanup on exit
trap cleanup EXIT

# Run main function
main "$@"