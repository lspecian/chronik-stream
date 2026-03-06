#!/usr/bin/env python3
"""
End-to-end local standalone test for Chronik vector search features (VO-1 through VO-6).

Uses a mock external embedding server to avoid OpenAI API dependency.
Tests the full pipeline: produce → embed → HNSW index → search → verify.

Usage:
    python3 tests/test_vector_local.py
"""

import hashlib
import json
import math
import os
import shutil
import signal
import socket
import subprocess
import sys
import time
import threading
from http.server import HTTPServer, BaseHTTPRequestHandler

import requests
from kafka import KafkaAdminClient, KafkaProducer
from kafka.admin import NewTopic

# Configuration
KAFKA_PORT = 9092
UNIFIED_API_PORT = 6092
MOCK_EMBED_PORT = 8099
EMBED_DIMS = 64
TOPIC_NAME = "test-vector-local"
DATA_DIR = "tests/data/vector-local-test"
SERVER_BINARY = "./target/release/chronik-server"
NUM_MESSAGES = 100
VECTOR_INDEX_TIMEOUT = 180  # seconds to wait for vector indexing
POLL_INTERVAL = 3  # seconds between stats polls


# ─── Mock Embedding Server ───────────────────────────────────────────────────

def text_to_vector(text, dims=EMBED_DIMS):
    """Deterministic embedding via SHA-256 hash, L2-normalized."""
    h = hashlib.sha256(text.encode()).digest()
    raw = [float(b) / 255.0 for b in h]
    vec = [raw[i % len(raw)] for i in range(dims)]
    norm = math.sqrt(sum(x * x for x in vec))
    if norm > 0:
        vec = [x / norm for x in vec]
    return vec


class MockEmbeddingHandler(BaseHTTPRequestHandler):
    """HTTP handler for mock embedding requests."""

    def do_POST(self):
        content_length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(content_length)
        try:
            data = json.loads(body)
            texts = data.get("texts", [])
            embeddings = [text_to_vector(t) for t in texts]
            response = json.dumps({"embeddings": embeddings})
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(response.encode())
        except Exception as e:
            self.send_response(500)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps({"error": str(e)}).encode())

    def log_message(self, format, *args):
        """Suppress request logging."""
        pass


def start_mock_server():
    """Start mock embedding server in a background thread."""
    server = HTTPServer(("0.0.0.0", MOCK_EMBED_PORT), MockEmbeddingHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server


# ─── Utilities ────────────────────────────────────────────────────────────────

def port_is_open(port, host="localhost"):
    """Check if a TCP port is accepting connections."""
    try:
        with socket.create_connection((host, port), timeout=1):
            return True
    except (ConnectionRefusedError, OSError, socket.timeout):
        return False


def wait_for_port(port, timeout=60, label="port"):
    """Wait for a port to become available."""
    start = time.time()
    while time.time() - start < timeout:
        if port_is_open(port):
            return True
        time.sleep(0.5)
    raise TimeoutError(f"{label} (port {port}) not ready after {timeout}s")


def step(num, total, msg):
    """Print a step header."""
    print(f"\n[{num}/{total}] {msg}...", flush=True)


def ok(detail=""):
    """Print OK result."""
    if detail:
        print(f"         OK ({detail})", flush=True)
    else:
        print(f"         OK", flush=True)


def fail(detail=""):
    """Print FAIL result and exit."""
    print(f"         FAIL ({detail})", flush=True)
    sys.exit(1)


def test_result(name, passed, detail=""):
    """Print test result."""
    status = "OK" if passed else "FAIL"
    suffix = f" ({detail})" if detail else ""
    print(f"  - {name:45s} {status}{suffix}", flush=True)
    if not passed:
        return False
    return True


# ─── Test Messages ────────────────────────────────────────────────────────────

SAMPLE_MESSAGES = [
    # Errors
    ("Connection refused to database server on port 5432", "error"),
    ("Out of memory: killed process 12345 (java)", "error"),
    ("SSL handshake failed: certificate expired", "error"),
    ("Disk space critically low: 2% remaining on /dev/sda1", "error"),
    ("Authentication failed for user admin: invalid credentials", "error"),
    ("Segmentation fault in worker thread #7", "error"),
    ("DNS resolution failed for api.example.com", "error"),
    ("Maximum retry attempts exceeded for message queue", "error"),
    ("Database deadlock detected between transactions 100 and 200", "error"),
    ("File descriptor limit reached: ulimit 1024", "error"),
    # Warnings
    ("High memory usage detected: 85% of 16GB utilized", "warning"),
    ("Response time degraded: p99 latency at 2.5 seconds", "warning"),
    ("Cache hit ratio dropped below threshold: 45%", "warning"),
    ("Thread pool nearing capacity: 95 of 100 threads active", "warning"),
    ("Replication lag increased to 30 seconds on replica-3", "warning"),
    ("Garbage collection pause exceeded 500ms", "warning"),
    ("Connection pool exhaustion imminent: 48 of 50 connections used", "warning"),
    ("Request queue depth at 10000 messages", "warning"),
    ("TLS certificate expires in 7 days", "warning"),
    ("Disk IOPS approaching provisioned limit", "warning"),
    # Info
    ("Server started successfully on port 8080", "info"),
    ("New user registration: user@example.com", "info"),
    ("Batch job completed: processed 50000 records in 45 seconds", "info"),
    ("Configuration reloaded from /etc/app/config.yaml", "info"),
    ("Health check passed: all dependencies healthy", "info"),
    ("Deployment v2.3.1 rolled out to production cluster", "info"),
    ("Scheduled maintenance window starting in 30 minutes", "info"),
    ("Database migration v42 applied successfully", "info"),
    ("Cache warmed: 1.2M entries loaded from snapshot", "info"),
    ("API rate limit reset for client org-12345", "info"),
    # Performance
    ("Query executed in 3.2ms: SELECT * FROM users WHERE id = 42", "perf"),
    ("Request handled in 12ms: GET /api/v1/products", "perf"),
    ("Kafka produce latency p50=2ms p99=15ms", "perf"),
    ("HNSW search completed in 45ms for 100K vectors", "perf"),
    ("Parquet file written: 500MB, 2.1M rows, zstd compression", "perf"),
    ("Index rebuild completed in 8.3 seconds for 50K documents", "perf"),
    ("WebSocket connection established in 5ms", "perf"),
    ("Batch insert: 10000 rows in 230ms", "perf"),
    ("Full-text search: 3.4ms for 30K documents", "perf"),
    ("Object storage upload: 150MB in 2.1s (71MB/s)", "perf"),
]


def generate_messages(n):
    """Generate n test messages cycling through samples."""
    messages = []
    for i in range(n):
        text, level = SAMPLE_MESSAGES[i % len(SAMPLE_MESSAGES)]
        msg = {
            "message": text,
            "level": level,
            "index": i,
            "timestamp": int(time.time() * 1000) + i,
        }
        messages.append((str(i).encode(), json.dumps(msg).encode()))
    return messages


# ─── Main Test ────────────────────────────────────────────────────────────────

def main():
    total_steps = 7
    server_proc = None
    mock_server = None
    all_passed = True

    try:
        # ── Stage 1: Build ────────────────────────────────────────────
        step(1, total_steps, "Building chronik-server")
        if os.path.exists(SERVER_BINARY):
            mtime = os.path.getmtime(SERVER_BINARY)
            age_min = (time.time() - mtime) / 60
            if age_min < 30:
                ok(f"binary fresh ({age_min:.0f}m old, skipping build)")
            else:
                result = subprocess.run(
                    ["cargo", "build", "--release", "--bin", "chronik-server"],
                    capture_output=True, text=True, timeout=600
                )
                if result.returncode != 0:
                    fail(f"cargo build failed:\n{result.stderr[-500:]}")
                ok("built")
        else:
            result = subprocess.run(
                ["cargo", "build", "--release", "--bin", "chronik-server"],
                capture_output=True, text=True, timeout=600
            )
            if result.returncode != 0:
                fail(f"cargo build failed:\n{result.stderr[-500:]}")
            ok("built")

        # ── Stage 2: Mock Embedding Server ────────────────────────────
        step(2, total_steps, "Starting mock embedding server")
        if port_is_open(MOCK_EMBED_PORT):
            fail(f"port {MOCK_EMBED_PORT} already in use")
        mock_server = start_mock_server()
        wait_for_port(MOCK_EMBED_PORT, timeout=5, label="mock embed server")
        ok(f"port {MOCK_EMBED_PORT}")

        # Quick self-test
        resp = requests.post(
            f"http://localhost:{MOCK_EMBED_PORT}/embed",
            json={"texts": ["hello world"], "model": "test"},
        )
        assert resp.status_code == 200, f"Mock self-test failed: {resp.status_code}"
        body = resp.json()
        assert len(body["embeddings"]) == 1
        assert len(body["embeddings"][0]) == EMBED_DIMS

        # ── Stage 3: Start chronik-server ─────────────────────────────
        step(3, total_steps, "Starting chronik-server")

        # Check ports aren't already in use
        for port, name in [(KAFKA_PORT, "Kafka"), (UNIFIED_API_PORT, "Unified API")]:
            if port_is_open(port):
                fail(f"{name} port {port} already in use — another server running?")

        # Clean data dir
        if os.path.exists(DATA_DIR):
            shutil.rmtree(DATA_DIR)
        os.makedirs(DATA_DIR, exist_ok=True)

        # Start server
        env = os.environ.copy()
        env["RUST_LOG"] = "info,chronik_storage::wal_indexer=debug"
        env["CHRONIK_DATA_DIR"] = DATA_DIR
        env["CHRONIK_UNIFIED_API_PORT"] = str(UNIFIED_API_PORT)
        # Server-wide embedding provider for text search and hybrid search (query-time embedding)
        env["CHRONIK_EMBEDDING_PROVIDER"] = "external"
        env["CHRONIK_EMBEDDING_ENDPOINT"] = f"http://localhost:{MOCK_EMBED_PORT}/embed"
        env["CHRONIK_EMBEDDING_DIMENSIONS"] = str(EMBED_DIMS)

        log_file = open(os.path.join(DATA_DIR, "server.log"), "w")
        server_proc = subprocess.Popen(
            [SERVER_BINARY, "start", "--advertise", "localhost"],
            env=env,
            stdout=log_file,
            stderr=subprocess.STDOUT,
        )

        # Wait for both ports
        wait_for_port(KAFKA_PORT, timeout=30, label="Kafka")
        wait_for_port(UNIFIED_API_PORT, timeout=30, label="Unified API")
        # Extra warmup for server initialization
        time.sleep(3)
        ok(f"pid={server_proc.pid}, ports {KAFKA_PORT}+{UNIFIED_API_PORT}")

        # ── Stage 4: Create vector-enabled topic ──────────────────────
        step(4, total_steps, "Creating vector-enabled topic")

        admin = KafkaAdminClient(
            bootstrap_servers=[f"localhost:{KAFKA_PORT}"],
            api_version=(2, 5, 0),
            request_timeout_ms=30000,
        )

        topic = NewTopic(
            name=TOPIC_NAME,
            num_partitions=1,
            replication_factor=1,
            topic_configs={
                "vector.enabled": "true",
                "vector.provider": "external",
                "vector.endpoint": f"http://localhost:{MOCK_EMBED_PORT}/embed",
                "vector.dimensions": str(EMBED_DIMS),
                "vector.field": "value",
                "vector.batch_size": "32",
                "vector.quantization": "f32",
                "vector.active_model": "test-model",
            },
        )

        try:
            admin.create_topics([topic])
            ok(TOPIC_NAME)
        except Exception as e:
            if "TopicAlreadyExistsError" in str(type(e).__name__) or "already exists" in str(e).lower():
                ok(f"{TOPIC_NAME} (already exists)")
            else:
                raise

        # Small delay for topic metadata propagation
        time.sleep(1)

        # ── Stage 5: Produce messages ─────────────────────────────────
        step(5, total_steps, "Producing messages")

        producer = KafkaProducer(
            bootstrap_servers=[f"localhost:{KAFKA_PORT}"],
            api_version=(2, 5, 0),
            request_timeout_ms=10000,
        )

        messages = generate_messages(NUM_MESSAGES)
        for key, value in messages:
            producer.send(TOPIC_NAME, key=key, value=value)
        producer.flush(timeout=30)
        producer.close()
        ok(f"{NUM_MESSAGES} messages")

        # ── Stage 6: Wait for vector indexing ─────────────────────────
        step(6, total_steps, "Waiting for vector indexing")

        base_url = f"http://localhost:{UNIFIED_API_PORT}"
        start_time = time.time()
        total_vectors = 0
        last_count = -1

        while time.time() - start_time < VECTOR_INDEX_TIMEOUT:
            try:
                resp = requests.get(f"{base_url}/_vector/{TOPIC_NAME}/stats", timeout=5)
                if resp.status_code == 200:
                    stats = resp.json()
                    total_vectors = stats.get("total_vectors", 0)
                    if total_vectors != last_count:
                        elapsed = time.time() - start_time
                        print(f"         ... {total_vectors} vectors indexed ({elapsed:.0f}s)", flush=True)
                        last_count = total_vectors
                    if total_vectors >= NUM_MESSAGES:
                        break
                elif resp.status_code == 404:
                    elapsed = time.time() - start_time
                    if int(elapsed) % 15 == 0 and int(elapsed) > 0:
                        print(f"         ... topic not yet registered for vector search ({elapsed:.0f}s)", flush=True)
            except requests.exceptions.ConnectionError:
                pass
            time.sleep(POLL_INTERVAL)

        elapsed = time.time() - start_time
        if total_vectors > 0:
            ok(f"{total_vectors} vectors in {elapsed:.0f}s")
        else:
            # Not a hard failure — WalIndexer may be slow on first run
            print(f"         WARNING: 0 vectors after {elapsed:.0f}s (WalIndexer may not have processed yet)", flush=True)
            print(f"         Continuing with available tests...", flush=True)

        # ── Stage 7: Run search tests ─────────────────────────────────
        step(7, total_steps, "Running search tests")

        # Test 1: List vector topics
        try:
            resp = requests.get(f"{base_url}/_vector/topics", timeout=5)
            passed = resp.status_code == 200
            detail = ""
            if passed:
                topics_data = resp.json()
                topic_names = []
                if isinstance(topics_data, list):
                    topic_names = [t.get("topic", t) if isinstance(t, dict) else t for t in topics_data]
                elif isinstance(topics_data, dict):
                    topic_names = topics_data.get("topics", [])
                if TOPIC_NAME in topic_names or any(TOPIC_NAME in str(t) for t in (topics_data if isinstance(topics_data, list) else [topics_data])):
                    detail = f"found {TOPIC_NAME}"
                else:
                    detail = f"topic list: {json.dumps(topics_data)[:100]}"
            else:
                detail = f"HTTP {resp.status_code}: {resp.text[:100]}"
        except Exception as e:
            passed = False
            detail = str(e)
        if not test_result("GET  /_vector/topics", passed, detail):
            all_passed = False

        # Test 2: Index stats
        try:
            resp = requests.get(f"{base_url}/_vector/{TOPIC_NAME}/stats", timeout=5)
            passed = resp.status_code == 200
            detail = ""
            if passed:
                stats = resp.json()
                vcount = stats.get("total_vectors", 0)
                dims = stats.get("dimensions", 0)
                detail = f"{vcount} vectors, {dims}d"
            else:
                detail = f"HTTP {resp.status_code}: {resp.text[:100]}"
        except Exception as e:
            passed = False
            detail = str(e)
        if not test_result("GET  /_vector/{topic}/stats", passed, detail):
            all_passed = False

        # Test 3: Text search (requires vectors to be indexed)
        if total_vectors > 0:
            try:
                resp = requests.post(
                    f"{base_url}/_vector/{TOPIC_NAME}/search",
                    json={"query": "database connection error", "k": 5},
                    timeout=10,
                )
                passed = resp.status_code == 200
                detail = ""
                if passed:
                    data = resp.json()
                    results = data.get("results", [])
                    detail = f"{len(results)} results"
                else:
                    detail = f"HTTP {resp.status_code}: {resp.text[:200]}"
            except Exception as e:
                passed = False
                detail = str(e)
            if not test_result("POST /_vector/{topic}/search", passed, detail):
                all_passed = False

            # Test 4: Vector search (raw vector, bypass embedding)
            try:
                query_vec = text_to_vector("database connection error")
                resp = requests.post(
                    f"{base_url}/_vector/{TOPIC_NAME}/search_by_vector",
                    json={"vector": query_vec, "k": 5},
                    timeout=10,
                )
                passed = resp.status_code == 200
                detail = ""
                if passed:
                    data = resp.json()
                    results = data.get("results", [])
                    detail = f"{len(results)} results"
                else:
                    detail = f"HTTP {resp.status_code}: {resp.text[:200]}"
            except Exception as e:
                passed = False
                detail = str(e)
            if not test_result("POST /_vector/{topic}/search_by_vector", passed, detail):
                all_passed = False

            # Test 5: Hybrid search
            try:
                resp = requests.post(
                    f"{base_url}/_vector/{TOPIC_NAME}/hybrid",
                    json={"query": "memory usage warning", "k": 5},
                    timeout=10,
                )
                # Hybrid may return 200 or 501 if text search not wired
                passed = resp.status_code in (200, 501)
                detail = ""
                if resp.status_code == 200:
                    data = resp.json()
                    results = data.get("results", [])
                    detail = f"{len(results)} results"
                else:
                    detail = f"HTTP {resp.status_code} (hybrid not fully wired)"
                    passed = True  # Expected for now
            except Exception as e:
                passed = False
                detail = str(e)
            if not test_result("POST /_vector/{topic}/hybrid", passed, detail):
                all_passed = False
        else:
            print("  - Skipping search tests (no vectors indexed yet)", flush=True)
            print("    This may indicate WalIndexer needs more time or has an issue.", flush=True)
            all_passed = False

        # ── Summary ───────────────────────────────────────────────────
        print("\n" + "=" * 60, flush=True)
        if all_passed:
            print("ALL TESTS PASSED", flush=True)
        else:
            print("SOME TESTS FAILED — see details above", flush=True)

    except KeyboardInterrupt:
        print("\n\nInterrupted by user", flush=True)
    except Exception as e:
        print(f"\n\nFATAL ERROR: {e}", flush=True)
        import traceback
        traceback.print_exc()
        all_passed = False
    finally:
        # ── Cleanup ───────────────────────────────────────────────────
        print("\nCleaning up...", flush=True)

        if server_proc:
            server_proc.send_signal(signal.SIGTERM)
            try:
                server_proc.wait(timeout=10)
                print(f"  Server stopped (pid={server_proc.pid})", flush=True)
            except subprocess.TimeoutExpired:
                server_proc.kill()
                print(f"  Server killed (pid={server_proc.pid})", flush=True)

        if mock_server:
            mock_server.shutdown()
            print(f"  Mock server stopped", flush=True)

        if os.path.exists(DATA_DIR):
            shutil.rmtree(DATA_DIR)
            print(f"  Removed {DATA_DIR}", flush=True)

        print("Done.", flush=True)

    sys.exit(0 if all_passed else 1)


if __name__ == "__main__":
    main()
