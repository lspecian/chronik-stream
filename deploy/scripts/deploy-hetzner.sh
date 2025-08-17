#!/bin/bash
# Deploy Chronik Stream to Hetzner Cloud

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo -e "${GREEN}Chronik Stream - Hetzner Cloud Deployment${NC}"
echo "========================================="

# Check requirements
check_requirements() {
    echo -e "${YELLOW}Checking requirements...${NC}"
    
    if ! command -v terraform &> /dev/null; then
        echo -e "${RED}Error: Terraform is not installed${NC}"
        echo "Please install Terraform: https://www.terraform.io/downloads"
        exit 1
    fi
    
    if ! command -v hcloud &> /dev/null; then
        echo -e "${YELLOW}Warning: hcloud CLI is not installed${NC}"
        echo "Install with: brew install hcloud (macOS) or check https://github.com/hetznercloud/cli"
    fi
    
    if [ -z "$HCLOUD_TOKEN" ]; then
        echo -e "${RED}Error: HCLOUD_TOKEN environment variable is not set${NC}"
        echo "Please set your Hetzner Cloud API token:"
        echo "export HCLOUD_TOKEN=your-token-here"
        exit 1
    fi
    
    echo -e "${GREEN}✓ All requirements met${NC}"
}

# Initialize Terraform
init_terraform() {
    echo -e "${YELLOW}Initializing Terraform...${NC}"
    cd deploy/terraform/hetzner
    terraform init
    echo -e "${GREEN}✓ Terraform initialized${NC}"
}

# Plan deployment
plan_deployment() {
    echo -e "${YELLOW}Planning deployment...${NC}"
    terraform plan -var="hcloud_token=$HCLOUD_TOKEN" -out=tfplan
    
    echo -e "${YELLOW}Review the plan above. Continue with deployment? (yes/no)${NC}"
    read -r response
    if [[ ! "$response" =~ ^([yY][eE][sS]|[yY])$ ]]; then
        echo -e "${RED}Deployment cancelled${NC}"
        exit 0
    fi
}

# Apply deployment
apply_deployment() {
    echo -e "${YELLOW}Deploying to Hetzner Cloud...${NC}"
    terraform apply tfplan
    
    echo -e "${GREEN}✓ Deployment complete!${NC}"
    echo ""
    echo "Deployment Information:"
    echo "======================="
    terraform output
}

# Test deployment
test_deployment() {
    echo -e "${YELLOW}Testing deployment...${NC}"
    
    KAFKA_ENDPOINT=$(terraform output -raw load_balancer_ip):9092
    
    # Wait for services to be ready
    echo "Waiting for services to be ready..."
    sleep 30
    
    # Run deployment tests
    cd ../../../
    DEPLOYMENT_PROVIDER=hetzner \
    HETZNER_KAFKA_ENDPOINT=$KAFKA_ENDPOINT \
    python3 tests/integration/test_deployment.py
    
    echo -e "${GREEN}✓ Deployment tests passed${NC}"
}

# Main execution
main() {
    check_requirements
    init_terraform
    plan_deployment
    apply_deployment
    
    echo -e "${YELLOW}Run deployment tests? (yes/no)${NC}"
    read -r response
    if [[ "$response" =~ ^([yY][eE][sS]|[yY])$ ]]; then
        test_deployment
    fi
    
    echo ""
    echo -e "${GREEN}Deployment successful!${NC}"
    echo "To connect to your Chronik Stream cluster:"
    echo "  export KAFKA_ENDPOINT=$(terraform output -raw load_balancer_ip):9092"
    echo "  kafkactl --brokers \$KAFKA_ENDPOINT get brokers"
}

# Run main function
main "$@"