#!/bin/bash
# Chronik Stream - Hetzner Cloud Deployment Script
# This script deploys Chronik Stream to Hetzner Cloud using the all-in-one image

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

echo -e "${BLUE}================================================${NC}"
echo -e "${BLUE}  Chronik Stream - Hetzner Cloud Deployment${NC}"
echo -e "${BLUE}================================================${NC}"

# Configuration
PROJECT_NAME="${PROJECT_NAME:-chronik-stream}"
LOCATION="${LOCATION:-fsn1}"  # Falkenstein, Germany
SERVER_TYPE="${SERVER_TYPE:-cpx21}"  # 3 vCPU, 4GB RAM shared
IMAGE="${IMAGE:-debian-12}"
SSH_KEY_NAME="${SSH_KEY_NAME:-chronik-deploy}"

# Check for hcloud CLI
if ! command -v hcloud &> /dev/null; then
    echo -e "${RED}Error: hcloud CLI is not installed${NC}"
    echo "Install with: brew install hcloud (macOS) or download from https://github.com/hetznercloud/cli"
    exit 1
fi

# Check if logged in
if ! hcloud context active &> /dev/null; then
    echo -e "${YELLOW}Please configure hcloud CLI first:${NC}"
    echo "Run: hcloud context create chronik"
    exit 1
fi

# Function to create SSH key if needed
create_ssh_key() {
    if ! hcloud ssh-key describe "$SSH_KEY_NAME" &> /dev/null; then
        echo -e "${YELLOW}Creating SSH key...${NC}"
        
        # Generate SSH key if it doesn't exist
        if [ ! -f ~/.ssh/chronik_deploy_rsa ]; then
            ssh-keygen -t rsa -b 4096 -f ~/.ssh/chronik_deploy_rsa -N "" -C "chronik-deploy"
        fi
        
        # Upload to Hetzner
        hcloud ssh-key create \
            --name "$SSH_KEY_NAME" \
            --public-key-from-file ~/.ssh/chronik_deploy_rsa.pub
    fi
}

# Function to create firewall rules
create_firewall() {
    local firewall_name="${PROJECT_NAME}-firewall"
    
    if ! hcloud firewall describe "$firewall_name" &> /dev/null; then
        echo -e "${YELLOW}Creating firewall...${NC}"
        
        hcloud firewall create \
            --name "$firewall_name" \
            --label "project=$PROJECT_NAME"
        
        # Add rules
        hcloud firewall add-rule "$firewall_name" \
            --direction in \
            --source-ips 0.0.0.0/0 \
            --source-ips ::/0 \
            --protocol tcp \
            --port 22 \
            --description "SSH"
        
        hcloud firewall add-rule "$firewall_name" \
            --direction in \
            --source-ips 0.0.0.0/0 \
            --source-ips ::/0 \
            --protocol tcp \
            --port 9092 \
            --description "Kafka API"
        
        hcloud firewall add-rule "$firewall_name" \
            --direction in \
            --source-ips 0.0.0.0/0 \
            --source-ips ::/0 \
            --protocol tcp \
            --port 3000 \
            --description "Admin API"
        
        hcloud firewall add-rule "$firewall_name" \
            --direction in \
            --source-ips 0.0.0.0/0 \
            --source-ips ::/0 \
            --protocol tcp \
            --port 9090 \
            --description "Metrics"
    fi
    
    echo "$firewall_name"
}

# Function to create server
create_server() {
    local server_name="${PROJECT_NAME}-${1}"
    local firewall_name="$2"
    
    if hcloud server describe "$server_name" &> /dev/null; then
        echo -e "${YELLOW}Server $server_name already exists${NC}"
        return
    fi
    
    echo -e "${GREEN}Creating server $server_name...${NC}"
    
    # Create the server
    hcloud server create \
        --name "$server_name" \
        --type "$SERVER_TYPE" \
        --image "$IMAGE" \
        --location "$LOCATION" \
        --ssh-key "$SSH_KEY_NAME" \
        --firewall "$firewall_name" \
        --label "project=$PROJECT_NAME" \
        --label "role=$1" \
        --user-data-from-file /dev/stdin <<'EOF'
#!/bin/bash
# Update system
apt-get update
apt-get upgrade -y

# Install Docker
apt-get install -y ca-certificates curl gnupg
install -m 0755 -d /etc/apt/keyrings
curl -fsSL https://download.docker.com/linux/debian/gpg | gpg --dearmor -o /etc/apt/keyrings/docker.gpg
chmod a+r /etc/apt/keyrings/docker.gpg

echo \
  "deb [arch="$(dpkg --print-architecture)" signed-by=/etc/apt/keyrings/docker.gpg] https://download.docker.com/linux/debian \
  "$(. /etc/os-release && echo "$VERSION_CODENAME")" stable" | \
  tee /etc/apt/sources.list.d/docker.list > /dev/null

apt-get update
apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin

# Start Docker
systemctl enable docker
systemctl start docker

# Create data directory
mkdir -p /data/chronik
chown -R 1000:1000 /data/chronik

# Pull and run Chronik Stream
docker pull ghcr.io/lspecian/chronik-stream:latest || true

# Create systemd service
cat > /etc/systemd/system/chronik.service <<'SYSTEMD'
[Unit]
Description=Chronik Stream
After=docker.service
Requires=docker.service

[Service]
Type=simple
Restart=always
RestartSec=10
ExecStartPre=-/usr/bin/docker stop chronik
ExecStartPre=-/usr/bin/docker rm chronik
ExecStart=/usr/bin/docker run \
  --name chronik \
  --rm \
  -p 9092:9092 \
  -p 3000:3000 \
  -p 9090:9090 \
  -v /data/chronik:/data \
  -e RUST_LOG=info \
  ghcr.io/lspecian/chronik-stream:latest

[Install]
WantedBy=multi-user.target
SYSTEMD

# Enable and start service
systemctl daemon-reload
systemctl enable chronik
systemctl start chronik

# Install monitoring
docker run -d \
  --name node-exporter \
  --restart always \
  -p 9100:9100 \
  prom/node-exporter:latest

echo "Chronik Stream deployment complete!"
EOF
    
    echo -e "${GREEN}Server $server_name created successfully!${NC}"
}

# Function to get server IP
get_server_ip() {
    local server_name="${PROJECT_NAME}-${1}"
    hcloud server describe "$server_name" -o json | jq -r '.public_net.ipv4.ip'
}

# Main deployment
echo -e "${BLUE}Starting deployment...${NC}"

# Create SSH key
create_ssh_key

# Create firewall
FIREWALL_NAME=$(create_firewall)

# Deploy single all-in-one server or cluster
if [ "$1" == "cluster" ]; then
    echo -e "${BLUE}Deploying Chronik Stream cluster...${NC}"
    
    # Create multiple servers
    create_server "controller" "$FIREWALL_NAME"
    create_server "ingest-1" "$FIREWALL_NAME"
    create_server "ingest-2" "$FIREWALL_NAME"
    
    # Wait for servers to be ready
    echo -e "${YELLOW}Waiting for servers to initialize...${NC}"
    sleep 60
    
    # Get IPs
    CONTROLLER_IP=$(get_server_ip "controller")
    INGEST1_IP=$(get_server_ip "ingest-1")
    INGEST2_IP=$(get_server_ip "ingest-2")
    
    echo -e "${GREEN}Cluster deployed successfully!${NC}"
    echo -e "Controller: ${BLUE}$CONTROLLER_IP${NC}"
    echo -e "Ingest 1: ${BLUE}$INGEST1_IP${NC}"
    echo -e "Ingest 2: ${BLUE}$INGEST2_IP${NC}"
else
    echo -e "${BLUE}Deploying single all-in-one server...${NC}"
    
    # Create single server
    create_server "all-in-one" "$FIREWALL_NAME"
    
    # Wait for server to be ready
    echo -e "${YELLOW}Waiting for server to initialize...${NC}"
    sleep 60
    
    # Get IP
    SERVER_IP=$(get_server_ip "all-in-one")
    
    echo -e "${GREEN}================================================${NC}"
    echo -e "${GREEN}Deployment successful!${NC}"
    echo -e "${GREEN}================================================${NC}"
    echo
    echo -e "Server IP: ${BLUE}$SERVER_IP${NC}"
    echo
    echo -e "${YELLOW}Test your deployment:${NC}"
    echo -e "  Kafka API: ${BLUE}kafkactl --brokers $SERVER_IP:9092 get brokers${NC}"
    echo -e "  Admin API: ${BLUE}curl http://$SERVER_IP:3000/health${NC}"
    echo -e "  Metrics:   ${BLUE}curl http://$SERVER_IP:9090/metrics${NC}"
    echo
    echo -e "${YELLOW}SSH access:${NC}"
    echo -e "  ${BLUE}ssh -i ~/.ssh/chronik_deploy_rsa root@$SERVER_IP${NC}"
    echo
    echo -e "${YELLOW}View logs:${NC}"
    echo -e "  ${BLUE}ssh -i ~/.ssh/chronik_deploy_rsa root@$SERVER_IP 'docker logs chronik'${NC}"
fi

# Save deployment info
cat > deployment-info.json <<EOF
{
  "provider": "hetzner",
  "location": "$LOCATION",
  "server_type": "$SERVER_TYPE",
  "server_ip": "${SERVER_IP:-$CONTROLLER_IP}",
  "kafka_endpoint": "${SERVER_IP:-$CONTROLLER_IP}:9092",
  "admin_endpoint": "http://${SERVER_IP:-$CONTROLLER_IP}:3000",
  "metrics_endpoint": "http://${SERVER_IP:-$CONTROLLER_IP}:9090/metrics",
  "ssh_key": "~/.ssh/chronik_deploy_rsa",
  "deployed_at": "$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
}
EOF

echo -e "${GREEN}Deployment info saved to deployment-info.json${NC}"

# Function to destroy deployment
if [ "$1" == "destroy" ]; then
    echo -e "${RED}Destroying deployment...${NC}"
    
    # Delete servers
    for server in $(hcloud server list -o json | jq -r ".[] | select(.labels.project==\"$PROJECT_NAME\") | .name"); do
        echo -e "${YELLOW}Deleting server $server...${NC}"
        hcloud server delete "$server"
    done
    
    # Delete firewall
    hcloud firewall delete "${PROJECT_NAME}-firewall"
    
    echo -e "${GREEN}Deployment destroyed${NC}"
    exit 0
fi

echo -e "${GREEN}Deployment script completed!${NC}"