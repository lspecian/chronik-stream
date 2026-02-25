#!/usr/bin/env bash
#
# RAG Chatbot Demo
# ================
#
# Demonstrates using Chronik as the retrieval backend for a
# Retrieval-Augmented Generation (RAG) chatbot.
#
# How it works:
#   1. Loads a knowledge base (50 articles) into Chronik via Kafka protocol
#   2. Chronik indexes them for full-text search (Tantivy/BM25)
#   3. Optionally: Chronik generates embeddings for vector search (OpenAI)
#   4. User asks questions interactively
#   5. Chronik retrieves relevant documents via /_query
#   6. Retrieved context + question sent to LLM for answer generation
#   7. Answer displayed with sources
#
# Prerequisites:
#   - chronik-server binary built (cargo build --release)
#   - kafka-python installed (pip3 install kafka-python)
#   - jq installed
#
# LLM backends (auto-detected in priority order):
#   1. Ollama (local, no API key) — if running at localhost:11434
#   2. OpenAI — if OPENAI_API_KEY is set
#   3. Anthropic — if ANTHROPIC_API_KEY is set
#   4. Retrieval-only — shows retrieved docs without LLM generation
#
# Works fully offline with Ollama (e.g., gemma3:4b for generation).
#
# Usage:
#   bash examples/rag-chatbot/demo.sh
#

set -euo pipefail

# ── Configuration ────────────────────────────────────────────────
DATA_DIR="/tmp/chronik-rag-demo"
SERVER_BIN="./target/release/chronik-server"
LOG_FILE="/tmp/chronik-rag-server.log"
API="http://localhost:6092"
KAFKA="localhost:9092"
SERVER_PID=""
TOPIC="knowledge-base"

# LLM settings
OLLAMA_URL="${OLLAMA_URL:-http://localhost:11434}"
OLLAMA_MODEL="${RAG_OLLAMA_MODEL:-gemma3:4b}"
OPENAI_MODEL="${RAG_LLM_MODEL:-gpt-4o-mini}"
ANTHROPIC_MODEL="${RAG_ANTHROPIC_MODEL:-claude-sonnet-4-20250514}"
MAX_CONTEXT_DOCS=5

# Colors
GREEN='\033[92m'
RED='\033[91m'
BLUE='\033[94m'
CYAN='\033[96m'
YELLOW='\033[93m'
MAGENTA='\033[95m'
BOLD='\033[1m'
DIM='\033[2m'
END='\033[0m'

# ── Helpers ──────────────────────────────────────────────────────

cleanup() {
    if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
        echo -e "\n${DIM}Stopping server (PID $SERVER_PID)...${END}"
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    rm -rf "$DATA_DIR"
    echo -e "${DIM}Cleaned up.${END}"
}
trap cleanup EXIT

banner() {
    echo ""
    echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${END}"
    echo -e "${BOLD}  $1${END}"
    echo -e "${BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${END}"
}

# ── Phase 1: Start Chronik Server ────────────────────────────────

banner "RAG Chatbot Demo — Powered by Chronik"

echo -e "${DIM}Data dir: $DATA_DIR${END}"
echo -e "${DIM}Server log: $LOG_FILE${END}"

# Check prerequisites
if [ ! -f "$SERVER_BIN" ]; then
    echo -e "${RED}Server binary not found. Run: cargo build --release --bin chronik-server${END}"
    exit 1
fi

if ! command -v jq &>/dev/null; then
    echo -e "${RED}jq not found. Install: sudo apt install jq${END}"
    exit 1
fi

# Detect available LLM (priority: Ollama > OpenAI > Anthropic > none)
HAS_OLLAMA=""
HAS_OPENAI=""
HAS_ANTHROPIC=""
LLM_BACKEND=""

if curl -sf "$OLLAMA_URL/api/tags" > /dev/null 2>&1; then
    # Check if the model is available
    if ollama list 2>/dev/null | grep -q "${OLLAMA_MODEL%%:*}"; then
        HAS_OLLAMA="true"
        LLM_BACKEND="ollama"
        echo -e "  ${GREEN}Ollama detected${END} — using ${OLLAMA_MODEL} for generation (local, no API key)"
    fi
fi
if [ -n "${OPENAI_API_KEY:-}" ]; then
    HAS_OPENAI="true"
    if [ -z "$LLM_BACKEND" ]; then
        LLM_BACKEND="openai"
        echo -e "  ${GREEN}OpenAI API key detected${END} — using ${OPENAI_MODEL} for generation"
    fi
fi
if [ -n "${ANTHROPIC_API_KEY:-}" ]; then
    HAS_ANTHROPIC="true"
    if [ -z "$LLM_BACKEND" ]; then
        LLM_BACKEND="anthropic"
        echo -e "  ${GREEN}Anthropic API key detected${END} — using ${ANTHROPIC_MODEL} for generation"
    fi
fi
if [ -z "$LLM_BACKEND" ]; then
    echo -e "  ${YELLOW}No LLM available${END} — retrieval-only mode"
    echo -e "  ${DIM}Install Ollama (ollama.com) or set OPENAI_API_KEY / ANTHROPIC_API_KEY for full RAG${END}"
fi

rm -rf "$DATA_DIR"
mkdir -p "$DATA_DIR"

echo -e "\n${BLUE}Starting Chronik server...${END}"
CHRONIK_DEFAULT_SEARCHABLE=true \
CHRONIK_DATA_DIR="$DATA_DIR" \
CHRONIK_ADVERTISED_ADDR=localhost \
RUST_LOG=warn \
    "$SERVER_BIN" start > "$LOG_FILE" 2>&1 &
SERVER_PID=$!

# Wait for Kafka port
for i in $(seq 1 30); do
    if curl -sf "$API/health" > /dev/null 2>&1; then
        echo -e "  ${GREEN}Server ready${END} (PID $SERVER_PID)"
        break
    fi
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
        echo -e "${RED}Server failed to start. Check $LOG_FILE${END}"
        exit 1
    fi
    sleep 0.5
done

# ── Phase 2: Load Knowledge Base ─────────────────────────────────

banner "Loading Knowledge Base"

echo -e "  ${BLUE}Producing 50 articles to topic '${TOPIC}'...${END}"

# Produce knowledge base articles via Kafka protocol
python3 -c "
import json, time
from kafka import KafkaProducer

producer = KafkaProducer(
    bootstrap_servers='localhost:9092',
    value_serializer=lambda v: json.dumps(v).encode('utf-8'),
    api_version=(0, 10, 0)
)

topic = '${TOPIC}'
ts = int(time.time() * 1000)

# Knowledge base: Acme Cloud Platform documentation
articles = [
    # Getting Started
    {'id': 'gs-001', 'category': 'getting-started', 'title': 'Quick Start Guide',
     'content': 'To get started with Acme Cloud, sign up at dashboard.acme.io and create your first project. You will receive an API key in your dashboard under Settings > API Keys. Use this key to authenticate all API requests. The free tier includes 10,000 API calls per month and 5GB of storage.'},
    {'id': 'gs-002', 'category': 'getting-started', 'title': 'Authentication',
     'content': 'All API requests require authentication via Bearer token in the Authorization header. Example: Authorization: Bearer your-api-key. API keys can be rotated from the dashboard. For service-to-service auth, use OAuth2 client credentials flow with your client ID and secret from Settings > Service Accounts.'},
    {'id': 'gs-003', 'category': 'getting-started', 'title': 'SDK Installation',
     'content': 'Install the Acme Cloud SDK: For Node.js run npm install @acme/cloud-sdk. For Python run pip install acme-cloud. For Go run go get github.com/acme/cloud-sdk-go. For Java add the Maven dependency com.acme:cloud-sdk:2.1.0. All SDKs support async operations and automatic retry with exponential backoff.'},

    # API Reference
    {'id': 'api-001', 'category': 'api', 'title': 'Create Resource API',
     'content': 'POST /api/v2/resources creates a new resource. Required fields: name (string, 3-128 chars), type (enum: compute, storage, network), region (enum: us-east-1, us-west-2, eu-west-1, ap-south-1). Optional: tags (object), description (string). Returns 201 with resource ID and metadata. Rate limit: 100 requests per minute per API key.'},
    {'id': 'api-002', 'category': 'api', 'title': 'List Resources API',
     'content': 'GET /api/v2/resources lists all resources in your project. Supports pagination with page and per_page parameters (default: 20 per page, max: 100). Filter by type, region, status, or tags. Sort by created_at, name, or cost (ascending or descending). Returns total count in X-Total-Count header.'},
    {'id': 'api-003', 'category': 'api', 'title': 'Delete Resource API',
     'content': 'DELETE /api/v2/resources/{id} deletes a resource. This action is irreversible. Active resources must be stopped first (400 error otherwise). Resources with dependencies cannot be deleted until dependencies are removed. Returns 204 on success. Deleted resources are retained in audit log for 90 days.'},
    {'id': 'api-004', 'category': 'api', 'title': 'Webhooks API',
     'content': 'Configure webhooks at POST /api/v2/webhooks. Events: resource.created, resource.deleted, resource.failed, billing.threshold, security.alert. Each webhook receives a JSON payload with event type, timestamp, and resource details. Webhooks must respond with 2xx within 30 seconds or will be retried 3 times with exponential backoff. Verify webhook signatures using the X-Acme-Signature header with your webhook secret.'},
    {'id': 'api-005', 'category': 'api', 'title': 'Rate Limits',
     'content': 'API rate limits vary by plan: Free tier 100 requests/minute, Pro 1000 requests/minute, Enterprise 10000 requests/minute. Rate limit headers included in every response: X-RateLimit-Limit, X-RateLimit-Remaining, X-RateLimit-Reset. When exceeded, returns 429 Too Many Requests. Use exponential backoff starting at 1 second. Burst allowance is 2x the per-minute rate for 10-second windows.'},

    # Compute
    {'id': 'compute-001', 'category': 'compute', 'title': 'Instance Types',
     'content': 'Acme Cloud offers 4 instance families: General (g-series) for balanced workloads with 2-64 vCPUs and 4-256GB RAM, Compute (c-series) for CPU-intensive work with high clock speed, Memory (m-series) for in-memory databases with up to 512GB RAM, and GPU (p-series) for ML training with NVIDIA A100 or H100 GPUs. All instances include NVMe SSD storage.'},
    {'id': 'compute-002', 'category': 'compute', 'title': 'Auto Scaling',
     'content': 'Auto scaling groups automatically adjust instance count based on metrics. Configure min/max instances, target CPU utilization (default 70%), scale-up cooldown (default 300s), and scale-down cooldown (default 600s). Custom metrics supported via CloudWatch integration. Predictive scaling available on Pro and Enterprise plans, which uses ML to anticipate traffic patterns.'},
    {'id': 'compute-003', 'category': 'compute', 'title': 'Container Service',
     'content': 'Acme Container Service (ACS) runs Docker containers without managing infrastructure. Deploy with acme deploy --image myapp:latest --port 8080 --replicas 3. Supports health checks, rolling deployments, blue-green deployments, and canary releases. Integrates with Acme Registry for private images. Minimum billing granularity is 1 second.'},

    # Storage
    {'id': 'storage-001', 'category': 'storage', 'title': 'Object Storage',
     'content': 'Acme Object Storage provides S3-compatible storage with 99.999999999% durability (11 nines). Storage classes: Standard (frequent access), Infrequent (30-day minimum, 40% cheaper), Archive (90-day minimum, 80% cheaper), Deep Archive (180-day minimum, 95% cheaper). Lifecycle rules can automatically transition objects between classes. Maximum object size is 5TB with multipart upload.'},
    {'id': 'storage-002', 'category': 'storage', 'title': 'Block Storage',
     'content': 'Block storage volumes attach to compute instances. Types: SSD (gp3, up to 16000 IOPS), Provisioned IOPS SSD (io2, up to 64000 IOPS), and Throughput Optimized HDD (st1, up to 500 MB/s). Volumes can be resized without downtime. Snapshots are incremental and stored in object storage. Encryption at rest is enabled by default using AES-256.'},
    {'id': 'storage-003', 'category': 'storage', 'title': 'Database Service',
     'content': 'Acme Database Service supports PostgreSQL 15, MySQL 8, and MongoDB 7. Automated backups with point-in-time recovery up to 35 days. Read replicas available in any region. Failover to standby completes within 60 seconds. Connection pooling via PgBouncer is built-in. Maximum storage per instance is 64TB. Encryption in transit and at rest included.'},

    # Networking
    {'id': 'net-001', 'category': 'networking', 'title': 'Virtual Networks',
     'content': 'Each project gets a default VPC with CIDR 10.0.0.0/16. Create subnets in multiple availability zones for high availability. Security groups control inbound and outbound traffic with stateful firewall rules. VPC peering connects VPCs across regions with no bandwidth charges. Network ACLs provide an additional stateless firewall layer.'},
    {'id': 'net-002', 'category': 'networking', 'title': 'Load Balancer',
     'content': 'Acme Load Balancer distributes traffic across instances. Types: Application (Layer 7, HTTP/HTTPS, path-based routing), Network (Layer 4, TCP/UDP, ultra-low latency), and Gateway (for third-party appliances). SSL termination included with free managed certificates. WebSocket support on Application load balancers. Health checks every 10 seconds with configurable thresholds.'},
    {'id': 'net-003', 'category': 'networking', 'title': 'CDN',
     'content': 'Acme CDN has 200+ edge locations worldwide. Supports static and dynamic content acceleration. Cache TTL configurable per path pattern. Origin shield reduces origin load by consolidating requests. Real-time analytics show hit ratio, bandwidth, and latency. Custom SSL certificates supported. DDoS protection included at no extra cost with automatic mitigation.'},

    # Security
    {'id': 'sec-001', 'category': 'security', 'title': 'IAM and Access Control',
     'content': 'Identity and Access Management (IAM) controls who can do what. Users are organized into groups. Policies are JSON documents specifying allowed or denied actions on resources. Best practice: use least privilege, create service accounts for applications, enable MFA for all human users. Role-based access control (RBAC) with predefined roles: Viewer, Editor, Admin, Owner.'},
    {'id': 'sec-002', 'category': 'security', 'title': 'Encryption',
     'content': 'All data encrypted at rest using AES-256 with customer-managed keys (CMK) or Acme-managed keys. In transit, TLS 1.3 is enforced on all API endpoints. Key Management Service (KMS) lets you create, rotate, and audit encryption keys. Automatic key rotation every 365 days. Envelope encryption used for large objects. FIPS 140-2 Level 3 validated HSMs available on Enterprise plan.'},
    {'id': 'sec-003', 'category': 'security', 'title': 'Compliance and Certifications',
     'content': 'Acme Cloud is certified SOC 2 Type II, ISO 27001, ISO 27017, ISO 27018, PCI DSS Level 1, HIPAA (with BAA), and GDPR compliant. FedRAMP Moderate authorized for US government workloads. Data residency controls ensure data stays in specified regions. Compliance reports available in the dashboard under Security > Compliance.'},

    # Billing
    {'id': 'bill-001', 'category': 'billing', 'title': 'Pricing Model',
     'content': 'Acme Cloud uses pay-as-you-go pricing with per-second billing for compute and per-GB-month for storage. Reserved instances offer up to 60% savings with 1 or 3 year commitments. Spot instances available at up to 90% discount for fault-tolerant workloads. Free tier includes: 750 hours of g-small compute, 5GB object storage, 1M API calls per month, for 12 months.'},
    {'id': 'bill-002', 'category': 'billing', 'title': 'Cost Management',
     'content': 'Set budget alerts at Settings > Billing > Budgets. Alerts trigger at 50%, 80%, and 100% of budget. Cost Explorer shows spending by service, region, tag, and time period. Resource tagging enables cost allocation to teams or projects. Anomaly detection identifies unusual spending patterns and alerts via email or webhook. Savings recommendations suggest reserved instance purchases based on usage.'},
    {'id': 'bill-003', 'category': 'billing', 'title': 'Invoicing and Payment',
     'content': 'Invoices generated monthly on the 1st. Payment methods: credit card, wire transfer (Enterprise only), or AWS/GCP marketplace. Past invoices downloadable from Billing > Invoices. Tax exemption certificates can be uploaded for applicable regions. Net-30 payment terms available for Enterprise customers. Multi-currency billing supported in USD, EUR, GBP, and JPY.'},

    # Troubleshooting
    {'id': 'ts-001', 'category': 'troubleshooting', 'title': 'Connection Timeout Errors',
     'content': 'If you get connection timeout errors, check: 1) Security group allows inbound traffic on the port, 2) Instance is in Running state, 3) VPC has an internet gateway attached, 4) Route table has a route to 0.0.0.0/0 via the internet gateway, 5) DNS resolution works (try using IP address directly). For intermittent timeouts, check if auto-scaling is removing instances too aggressively.'},
    {'id': 'ts-002', 'category': 'troubleshooting', 'title': 'High CPU or Memory Usage',
     'content': 'For high CPU: identify the process with top or htop, check for infinite loops or runaway queries, consider upgrading to a c-series instance. For high memory: check for memory leaks with valgrind or pprof, increase swap space temporarily, enable OOM killer logging. Auto-scaling can help: set target CPU to 70% and min instances to 2. Memory metrics require the monitoring agent to be installed.'},
    {'id': 'ts-003', 'category': 'troubleshooting', 'title': 'Database Connection Pool Exhaustion',
     'content': 'If you see connection pool exhausted errors: 1) Check active connections with SELECT count(*) FROM pg_stat_activity, 2) Increase max_connections in database config, 3) Use PgBouncer for connection pooling (built-in on Acme Database Service), 4) Ensure application properly closes connections, 5) Set connection timeout to 30s and idle timeout to 300s. Consider read replicas to distribute query load.'},
    {'id': 'ts-004', 'category': 'troubleshooting', 'title': 'SSL Certificate Errors',
     'content': 'SSL errors usually indicate: 1) Certificate expired — check expiry with openssl s_client -connect host:443, 2) Certificate mismatch — verify the domain matches the certificate CN or SAN, 3) Untrusted CA — ensure the CA certificate is in the trust store, 4) TLS version — Acme requires TLS 1.2 minimum. Managed certificates auto-renew 30 days before expiry. Custom certificates must be renewed manually.'},
    {'id': 'ts-005', 'category': 'troubleshooting', 'title': '502 Bad Gateway Errors',
     'content': 'A 502 error means the load balancer cannot reach your backend. Check: 1) Backend instances are healthy (check target group health), 2) Application is listening on the correct port, 3) Health check path returns 200 OK, 4) Security group allows traffic from load balancer, 5) Application startup time is within the health check grace period. If deploying, ensure rolling deployment has enough healthy instances.'},

    # Monitoring
    {'id': 'mon-001', 'category': 'monitoring', 'title': 'Metrics and Dashboards',
     'content': 'Acme Monitoring collects metrics from all services automatically. Default dashboards show CPU, memory, network, and disk for compute instances. Custom dashboards support time-series graphs, heat maps, and single-stat panels. Query language: SELECT avg(cpu_percent) FROM metrics WHERE instance_id = xxx GROUP BY 5m. Retention: 15 months at 1-minute resolution. Export to Prometheus or Grafana supported.'},
    {'id': 'mon-002', 'category': 'monitoring', 'title': 'Alerts and Notifications',
     'content': 'Configure alerts based on metric thresholds or anomaly detection. Notification channels: email, Slack, PagerDuty, OpsGenie, webhook. Alert states: OK, ALARM, INSUFFICIENT_DATA. Multi-condition alerts support AND/OR logic. Maintenance windows suppress alerts during planned downtime. Alert history retained for 1 year. Runbook URLs can be attached to alerts for quick remediation.'},
    {'id': 'mon-003', 'category': 'monitoring', 'title': 'Log Management',
     'content': 'Acme Log Management aggregates logs from all services. Structured logging in JSON format recommended. Full-text search with Lucene query syntax. Log insights automatically detect error patterns and anomalies. Retention configurable from 7 to 365 days. Log-based metrics: create custom metrics from log patterns (e.g., count of 5xx errors per minute). Export logs to S3 for long-term storage.'},

    # Deployment
    {'id': 'deploy-001', 'category': 'deployment', 'title': 'CI/CD Integration',
     'content': 'Acme Deploy integrates with GitHub Actions, GitLab CI, Jenkins, and CircleCI. Deploy with acme deploy --image myapp:v1.2.3 or use the deploy API. Deployment strategies: rolling (default, zero-downtime), blue-green (instant rollback), canary (gradual traffic shift). Deployment hooks: pre-deploy, post-deploy, rollback. Automatic rollback on health check failure within 5 minutes.'},
    {'id': 'deploy-002', 'category': 'deployment', 'title': 'Infrastructure as Code',
     'content': 'Acme supports Terraform with the official provider hashicorp/acme. All resources can be managed via Terraform. CloudFormation-compatible templates also supported. State management with remote backend at s3://your-bucket/terraform.tfstate. Drift detection runs daily and alerts on manual changes. Import existing resources with terraform import. Module registry at registry.acme.io.'},
    {'id': 'deploy-003', 'category': 'deployment', 'title': 'Environment Management',
     'content': 'Best practice: separate environments (dev, staging, prod) using different projects. Environment variables managed via Secrets Manager. Config files stored in Config Store with versioning. Promote deployments between environments with acme promote --from staging --to prod. Environment parity: use the same instance types and configurations across environments to avoid surprises.'},

    # Data & Analytics
    {'id': 'data-001', 'category': 'data', 'title': 'Data Pipeline Service',
     'content': 'Acme Data Pipelines process streaming and batch data. Source connectors: Kafka, PostgreSQL CDC, S3, HTTP. Sink connectors: Data Warehouse, S3, Elasticsearch. Transformations using SQL or custom functions. Exactly-once processing guarantee with checkpoint-based recovery. Auto-scaling from 1 to 1000 processing units based on throughput. Schema evolution handled automatically with Schema Registry integration.'},
    {'id': 'data-002', 'category': 'data', 'title': 'Data Warehouse',
     'content': 'Acme Data Warehouse is a serverless columnar analytics engine. Load data from S3, databases, or pipelines. Query with standard SQL. Automatic partitioning by date for time-series data. Supports JSON, Parquet, CSV, and Avro formats. Concurrency scaling handles unlimited concurrent queries. Materialized views auto-refresh on schedule. Cost: $5 per TB scanned. Caching reduces repeat query costs.'},
    {'id': 'data-003', 'category': 'data', 'title': 'Machine Learning Platform',
     'content': 'Acme ML Platform provides end-to-end ML lifecycle management. Notebook service with JupyterLab and GPU support. Training jobs support PyTorch, TensorFlow, XGBoost, and custom containers. Model registry with versioning and A/B testing. Deployment as REST API with auto-scaling. Built-in monitoring for data drift and model performance. Feature store for sharing features across models. MLOps pipelines with Kubeflow integration.'},

    # Support
    {'id': 'sup-001', 'category': 'support', 'title': 'Support Plans',
     'content': 'Support plans: Basic (community forums, documentation), Developer (email support, 24h response), Business (24/7 phone and chat, 4h response for critical, dedicated account manager), Enterprise (15-min response for critical, dedicated TAM, architecture reviews). All plans include status page at status.acme.io. Emergency support: call 1-800-ACME-911 for Business and Enterprise plans.'},
    {'id': 'sup-002', 'category': 'support', 'title': 'SLA and Uptime',
     'content': 'Acme Cloud SLA: 99.99% uptime for multi-AZ deployments, 99.95% for single-AZ. SLA credits: 10% credit for 99.9%-99.99%, 25% for 99.0%-99.9%, 100% for below 99.0%. Exclusions: scheduled maintenance (announced 72h in advance), force majeure, customer-caused outages. Uptime measured per service per region. Status page: status.acme.io with real-time and historical data.'},

    # Migration
    {'id': 'mig-001', 'category': 'migration', 'title': 'Migration from AWS',
     'content': 'Migrate from AWS to Acme Cloud: 1) Use the Migration Assessment Tool to analyze your AWS resources, 2) Export data from S3 to Acme Object Storage using the bulk transfer service (free for first 10TB), 3) Database migration with continuous replication using DMS-compatible tool, 4) Update DNS with low TTL before cutover, 5) Run in parallel for 1-2 weeks to validate. Migration support included for Business and Enterprise plans.'},
    {'id': 'mig-002', 'category': 'migration', 'title': 'Migration from GCP',
     'content': 'Migrate from GCP to Acme Cloud: 1) Export Compute Engine instances as images, import to Acme Compute, 2) Transfer Cloud Storage buckets with gsutil and acme-cli, 3) Cloud SQL databases migrate using pg_dump/pg_restore or MySQL dump with the Acme Database Migration Service, 4) Kubernetes workloads: export manifests, update container registry references to Acme Registry. IAM policies can be translated with the acme-iam-converter tool.'},

    # Edge Computing
    {'id': 'edge-001', 'category': 'edge', 'title': 'Edge Functions',
     'content': 'Acme Edge Functions run code at 200+ edge locations worldwide with sub-10ms cold start. Deploy JavaScript or WebAssembly functions. Use cases: A/B testing, request routing, header manipulation, authentication, personalization. 10M free invocations per month. Limits: 50ms CPU time, 128MB memory, 1MB response. Access to Edge KV store for stateful applications.'},

    # Integrations
    {'id': 'int-001', 'category': 'integrations', 'title': 'Third-Party Integrations',
     'content': 'Acme Cloud integrates with: Datadog (metrics and logs), PagerDuty (alerting), Slack (notifications), GitHub (CI/CD and IaC), Terraform (infrastructure), Vault (secrets management), Okta (SSO), Auth0 (authentication). Marketplace at marketplace.acme.io has 200+ verified integrations. Build custom integrations with the Events API and webhooks.'},

    # Kubernetes
    {'id': 'k8s-001', 'category': 'kubernetes', 'title': 'Managed Kubernetes',
     'content': 'Acme Kubernetes Service (AKS) provides managed Kubernetes clusters. Control plane is free and fully managed. Node pools support auto-scaling with mixed instance types. Cluster autoscaler adds or removes nodes based on pending pods. Built-in ingress controller with automatic SSL. Pod-level monitoring and logging. Supports Kubernetes 1.28 through 1.30. Upgrade clusters with zero downtime using rolling node replacement.'},

    # Disaster Recovery
    {'id': 'dr-001', 'category': 'disaster-recovery', 'title': 'Backup and Recovery',
     'content': 'Acme provides automated backup for all managed services. Database backups: daily snapshots retained 35 days, point-in-time recovery to any second within retention. Object storage: versioning and cross-region replication. Compute: AMI snapshots on schedule. Recovery Time Objective (RTO): 1 hour for databases, 15 minutes for compute from snapshots. Recovery Point Objective (RPO): 1 second for databases with continuous WAL archiving.'},
    {'id': 'dr-002', 'category': 'disaster-recovery', 'title': 'Multi-Region Architecture',
     'content': 'For maximum availability, deploy across multiple regions. Active-active: both regions serve traffic, DNS-based failover with health checks. Active-passive: standby region with database replication, failover in under 60 seconds. Cross-region data replication adds 50-200ms latency depending on regions. Global load balancer routes users to nearest region. Cost: approximately 2x single-region for active-active.'},
]

success = 0
for i, article in enumerate(articles):
    try:
        future = producer.send(topic, value=article)
        future.get(timeout=10)
        success += 1
    except Exception as e:
        print(f'  Failed to produce article {i}: {e}')

producer.flush()
producer.close()
print(f'  Produced {success}/{len(articles)} articles')
"

echo -e "  ${GREEN}Knowledge base loaded${END}"

# ── Phase 3: Wait for Indexing ────────────────────────────────────

banner "Waiting for Search Index"

echo -e "  ${DIM}Chronik indexes text in background (Tantivy BM25)...${END}"

MAX_WAIT=90
SEARCH_READY=false
for i in $(seq 1 $MAX_WAIT); do
    PROBE=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
        "sources": [{"topic":"'"$TOPIC"'","modes":["text"]}],
        "q": {"text":"authentication"},
        "k": 1, "result_format": "merged"
    }' 2>/dev/null || echo '{}')
    PROBE_COUNT=$(echo "$PROBE" | jq '.results | length' 2>/dev/null || echo "0")
    if [ "$PROBE_COUNT" -gt 0 ]; then
        SEARCH_READY=true
        echo -e "  ${GREEN}Text search ready${END} (${i}s)"
        break
    fi
    printf "  Indexing... %ds / %ds\r" "$i" "$MAX_WAIT"
    sleep 1
done

if [ "$SEARCH_READY" != "true" ]; then
    echo -e "  ${RED}Text search not ready after ${MAX_WAIT}s. Check server logs.${END}"
    exit 1
fi

# Check how many documents are indexed
DOC_COUNT=$(curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
    "sources": [{"topic":"'"$TOPIC"'","modes":["text"]}],
    "q": {"text":"acme cloud"},
    "k": 100, "result_format": "merged"
}' 2>/dev/null | jq '.results | length' 2>/dev/null || echo "0")
echo -e "  ${DIM}Documents searchable: ~${DOC_COUNT}${END}"

# ── Phase 4: Interactive RAG Loop ─────────────────────────────────

banner "RAG Chatbot Ready"

echo -e "  Ask questions about Acme Cloud Platform."
echo -e "  The chatbot retrieves relevant documentation and generates answers."
echo ""
echo -e "  ${DIM}Example questions:${END}"
echo -e "  ${DIM}  - How do I authenticate with the API?${END}"
echo -e "  ${DIM}  - What instance types are available?${END}"
echo -e "  ${DIM}  - How do I fix a 502 error?${END}"
echo -e "  ${DIM}  - What is the SLA uptime guarantee?${END}"
echo -e "  ${DIM}  - How do I migrate from AWS?${END}"
echo -e "  ${DIM}  - What are the rate limits?${END}"
echo ""
echo -e "  ${DIM}Type 'quit' or Ctrl+C to exit.${END}"

# Function: search Chronik for relevant documents
search_chronik() {
    local query="$1"
    local k="${2:-$MAX_CONTEXT_DOCS}"

    curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
        "sources": [{"topic":"'"$TOPIC"'","modes":["text"]}],
        "q": {"text":"'"$(echo "$query" | sed 's/"/\\"/g')"'"},
        "k": '"$k"',
        "rank": {"profile": "relevance"},
        "result_format": "merged"
    }' 2>/dev/null
}

# Function: fetch a document by offset
fetch_doc() {
    local offset="$1"
    local partition="${2:-0}"

    curl -sf "$API/_query" -H 'Content-Type: application/json' -d '{
        "sources": [{"topic":"'"$TOPIC"'","modes":["fetch"]}],
        "q": {"fetch": {"offset": '"$offset"', "partition": '"$partition"', "max_bytes": 524288}},
        "k": 1,
        "result_format": "merged"
    }' 2>/dev/null
}

# Function: call Ollama for answer generation (local, no API key)
call_ollama() {
    local question="$1"
    local context="$2"

    local prompt="Based on the following documentation, answer the question concisely and accurately. Only use information from the documentation. If the documentation does not contain the answer, say so.

Documentation:
${context}

Question: ${question}

Answer:"

    local response
    response=$(curl -sf "$OLLAMA_URL/api/generate" \
        -d "$(jq -n \
            --arg model "$OLLAMA_MODEL" \
            --arg prompt "$prompt" \
            '{model: $model, prompt: $prompt, stream: false, options: {temperature: 0.3, num_predict: 300}}'
        )" 2>/dev/null)

    echo "$response" | jq -r '.response // "Error: could not generate response"' 2>/dev/null
}

# Function: call OpenAI for answer generation
call_openai() {
    local question="$1"
    local context="$2"

    local response
    response=$(curl -sf "https://api.openai.com/v1/chat/completions" \
        -H "Content-Type: application/json" \
        -H "Authorization: Bearer $OPENAI_API_KEY" \
        -d '{
            "model": "'"$OPENAI_MODEL"'",
            "messages": [
                {"role": "system", "content": "You are a helpful support assistant for Acme Cloud Platform. Answer questions based ONLY on the provided documentation context. If the context does not contain enough information, say so. Be concise and practical. Include specific details like commands, URLs, or config values when available."},
                {"role": "user", "content": "Documentation context:\n\n'"$(echo "$context" | sed 's/"/\\"/g' | sed ':a;N;$!ba;s/\n/\\n/g')"'\n\nQuestion: '"$(echo "$question" | sed 's/"/\\"/g')"'"}
            ],
            "temperature": 0.3,
            "max_tokens": 500
        }' 2>/dev/null)

    echo "$response" | jq -r '.choices[0].message.content // "Error: could not generate response"' 2>/dev/null
}

# Function: call Anthropic for answer generation
call_anthropic() {
    local question="$1"
    local context="$2"

    local response
    response=$(curl -sf "https://api.anthropic.com/v1/messages" \
        -H "Content-Type: application/json" \
        -H "x-api-key: $ANTHROPIC_API_KEY" \
        -H "anthropic-version: 2023-06-01" \
        -d '{
            "model": "'"$ANTHROPIC_MODEL"'",
            "max_tokens": 500,
            "system": "You are a helpful support assistant for Acme Cloud Platform. Answer questions based ONLY on the provided documentation context. If the context does not contain enough information, say so. Be concise and practical. Include specific details like commands, URLs, or config values when available.",
            "messages": [
                {"role": "user", "content": "Documentation context:\n\n'"$(echo "$context" | sed 's/"/\\"/g' | sed ':a;N;$!ba;s/\n/\\n/g')"'\n\nQuestion: '"$(echo "$question" | sed 's/"/\\"/g')"'"}
            ]
        }' 2>/dev/null)

    echo "$response" | jq -r '.content[0].text // "Error: could not generate response"' 2>/dev/null
}

# Interactive loop
while true; do
    echo ""
    echo -ne "${BOLD}You: ${END}"
    read -r question

    if [ -z "$question" ]; then
        continue
    fi

    if [ "$question" = "quit" ] || [ "$question" = "exit" ] || [ "$question" = "q" ]; then
        echo -e "\n${DIM}Goodbye!${END}"
        break
    fi

    # Step 1: Retrieve relevant documents from Chronik
    echo -e "\n  ${DIM}Searching knowledge base...${END}"
    SEARCH_RESULT=$(search_chronik "$question")

    RESULT_COUNT=$(echo "$SEARCH_RESULT" | jq '.results | length' 2>/dev/null || echo "0")
    CANDIDATES=$(echo "$SEARCH_RESULT" | jq '.stats.candidates // 0' 2>/dev/null || echo "0")
    LATENCY=$(echo "$SEARCH_RESULT" | jq '.stats.latency_ms // 0' 2>/dev/null || echo "0")

    if [ "$RESULT_COUNT" -eq 0 ]; then
        echo -e "  ${YELLOW}No relevant documents found.${END}"
        continue
    fi

    echo -e "  ${GREEN}Found $RESULT_COUNT documents${END} ${DIM}(from $CANDIDATES candidates, ${LATENCY}ms)${END}"

    # Step 2: Fetch full document content for top results
    CONTEXT=""
    SOURCES=""
    for idx in $(seq 0 $((RESULT_COUNT - 1))); do
        OFFSET=$(echo "$SEARCH_RESULT" | jq -r ".results[$idx].offset" 2>/dev/null)
        PARTITION=$(echo "$SEARCH_RESULT" | jq -r ".results[$idx].partition" 2>/dev/null)
        SCORE=$(echo "$SEARCH_RESULT" | jq -r ".results[$idx].final_score" 2>/dev/null)
        TEXT_SCORE=$(echo "$SEARCH_RESULT" | jq -r ".results[$idx].features.text_score // 0" 2>/dev/null)

        # Fetch the full document
        DOC_RAW=$(fetch_doc "$OFFSET" "$PARTITION")
        DOC_VALUE=$(echo "$DOC_RAW" | jq -r '.results[0].value // empty' 2>/dev/null)

        if [ -z "$DOC_VALUE" ]; then
            continue
        fi

        # Parse the document
        TITLE=$(echo "$DOC_VALUE" | jq -r '.title // "Untitled"' 2>/dev/null)
        CATEGORY=$(echo "$DOC_VALUE" | jq -r '.category // "general"' 2>/dev/null)
        CONTENT=$(echo "$DOC_VALUE" | jq -r '.content // ""' 2>/dev/null)
        DOC_ID=$(echo "$DOC_VALUE" | jq -r '.id // "unknown"' 2>/dev/null)

        if [ -n "$CONTENT" ]; then
            CONTEXT="${CONTEXT}[${TITLE}] (${CATEGORY}): ${CONTENT}\n\n"
            SOURCES="${SOURCES}  ${DIM}[${idx}]${END} ${CYAN}${TITLE}${END} ${DIM}(${CATEGORY}, score: ${SCORE})${END}\n"
        fi
    done

    # Show retrieved sources
    echo -e "\n  ${BOLD}Sources:${END}"
    echo -e "$SOURCES"

    # Step 3: Generate answer with LLM (if available)
    if [ "$LLM_BACKEND" = "ollama" ]; then
        echo -e "  ${DIM}Generating answer with ${OLLAMA_MODEL} (local)...${END}"
        ANSWER=$(call_ollama "$question" "$CONTEXT")
        echo -e "\n  ${BOLD}${MAGENTA}Assistant:${END} $ANSWER"
    elif [ "$LLM_BACKEND" = "openai" ]; then
        echo -e "  ${DIM}Generating answer with ${OPENAI_MODEL}...${END}"
        ANSWER=$(call_openai "$question" "$CONTEXT")
        echo -e "\n  ${BOLD}${MAGENTA}Assistant:${END} $ANSWER"
    elif [ "$LLM_BACKEND" = "anthropic" ]; then
        echo -e "  ${DIM}Generating answer with ${ANTHROPIC_MODEL}...${END}"
        ANSWER=$(call_anthropic "$question" "$CONTEXT")
        echo -e "\n  ${BOLD}${MAGENTA}Assistant:${END} $ANSWER"
    else
        # No LLM — show retrieved context directly
        echo -e "  ${BOLD}Retrieved Context:${END}"
        echo -e "$CONTEXT" | while IFS= read -r line; do
            if [ -n "$line" ]; then
                echo -e "  ${DIM}$line${END}"
            fi
        done
        echo -e "\n  ${YELLOW}Install Ollama or set OPENAI_API_KEY / ANTHROPIC_API_KEY for LLM-generated answers.${END}"
    fi
done

echo -e "\n${GREEN}Demo complete.${END}"
