#!/bin/bash
# Deploy Chronik Stream to AWS

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo -e "${GREEN}Chronik Stream - AWS Deployment${NC}"
echo "================================="

# Check requirements
check_requirements() {
    echo -e "${YELLOW}Checking requirements...${NC}"
    
    if ! command -v terraform &> /dev/null; then
        echo -e "${RED}Error: Terraform is not installed${NC}"
        echo "Please install Terraform: https://www.terraform.io/downloads"
        exit 1
    fi
    
    if ! command -v aws &> /dev/null; then
        echo -e "${RED}Error: AWS CLI is not installed${NC}"
        echo "Please install AWS CLI: https://aws.amazon.com/cli/"
        exit 1
    fi
    
    # Check AWS credentials
    if ! aws sts get-caller-identity &> /dev/null; then
        echo -e "${RED}Error: AWS credentials not configured${NC}"
        echo "Please run: aws configure"
        exit 1
    fi
    
    echo -e "${GREEN}✓ All requirements met${NC}"
}

# Select AWS region
select_region() {
    echo -e "${YELLOW}Select AWS region:${NC}"
    echo "1) us-east-1 (N. Virginia)"
    echo "2) us-west-2 (Oregon)"
    echo "3) eu-west-1 (Ireland)"
    echo "4) eu-central-1 (Frankfurt)"
    echo "5) ap-southeast-1 (Singapore)"
    echo "6) Custom"
    
    read -p "Enter choice [1-6]: " choice
    
    case $choice in
        1) AWS_REGION="us-east-1" ;;
        2) AWS_REGION="us-west-2" ;;
        3) AWS_REGION="eu-west-1" ;;
        4) AWS_REGION="eu-central-1" ;;
        5) AWS_REGION="ap-southeast-1" ;;
        6) read -p "Enter AWS region: " AWS_REGION ;;
        *) AWS_REGION="us-east-1" ;;
    esac
    
    export AWS_REGION
    echo -e "${GREEN}✓ Using region: $AWS_REGION${NC}"
}

# Check or create key pair
setup_keypair() {
    echo -e "${YELLOW}Setting up EC2 key pair...${NC}"
    
    read -p "Enter key pair name [chronik-stream-key]: " KEY_PAIR_NAME
    KEY_PAIR_NAME=${KEY_PAIR_NAME:-chronik-stream-key}
    
    # Check if key pair exists
    if aws ec2 describe-key-pairs --key-names "$KEY_PAIR_NAME" --region "$AWS_REGION" &> /dev/null; then
        echo -e "${GREEN}✓ Using existing key pair: $KEY_PAIR_NAME${NC}"
    else
        echo -e "${YELLOW}Creating new key pair...${NC}"
        aws ec2 create-key-pair \
            --key-name "$KEY_PAIR_NAME" \
            --region "$AWS_REGION" \
            --query 'KeyMaterial' \
            --output text > "${KEY_PAIR_NAME}.pem"
        chmod 400 "${KEY_PAIR_NAME}.pem"
        echo -e "${GREEN}✓ Created key pair: $KEY_PAIR_NAME${NC}"
        echo -e "${YELLOW}Private key saved to: ${KEY_PAIR_NAME}.pem${NC}"
    fi
    
    export TF_VAR_key_pair_name="$KEY_PAIR_NAME"
}

# Initialize Terraform
init_terraform() {
    echo -e "${YELLOW}Initializing Terraform...${NC}"
    cd deploy/terraform/aws
    terraform init
    echo -e "${GREEN}✓ Terraform initialized${NC}"
}

# Plan deployment
plan_deployment() {
    echo -e "${YELLOW}Planning deployment...${NC}"
    terraform plan \
        -var="aws_region=$AWS_REGION" \
        -var="key_pair_name=$KEY_PAIR_NAME" \
        -out=tfplan
    
    echo -e "${YELLOW}Review the plan above. Continue with deployment? (yes/no)${NC}"
    read -r response
    if [[ ! "$response" =~ ^([yY][eE][sS]|[yY])$ ]]; then
        echo -e "${RED}Deployment cancelled${NC}"
        exit 0
    fi
}

# Apply deployment
apply_deployment() {
    echo -e "${YELLOW}Deploying to AWS...${NC}"
    terraform apply tfplan
    
    echo -e "${GREEN}✓ Deployment complete!${NC}"
    echo ""
    echo "Deployment Information:"
    echo "======================="
    terraform output
}

# Configure security groups
configure_security() {
    echo -e "${YELLOW}Configuring security groups...${NC}"
    
    # Get your IP address
    MY_IP=$(curl -s https://api.ipify.org)
    echo "Your IP address: $MY_IP"
    
    # Update security group to allow access from your IP
    SECURITY_GROUP_ID=$(aws ec2 describe-security-groups \
        --filters "Name=group-name,Values=chronik-stream-ingest-*" \
        --region "$AWS_REGION" \
        --query 'SecurityGroups[0].GroupId' \
        --output text)
    
    if [ "$SECURITY_GROUP_ID" != "None" ] && [ -n "$SECURITY_GROUP_ID" ]; then
        aws ec2 authorize-security-group-ingress \
            --group-id "$SECURITY_GROUP_ID" \
            --protocol tcp \
            --port 9092 \
            --cidr "${MY_IP}/32" \
            --region "$AWS_REGION" 2>/dev/null || true
        
        echo -e "${GREEN}✓ Security group configured for your IP${NC}"
    fi
}

# Test deployment
test_deployment() {
    echo -e "${YELLOW}Testing deployment...${NC}"
    
    KAFKA_ENDPOINT=$(terraform output -raw kafka_endpoint)
    
    # Wait for services to be ready
    echo "Waiting for services to be ready..."
    sleep 60
    
    # Run deployment tests
    cd ../../../
    DEPLOYMENT_PROVIDER=aws \
    AWS_KAFKA_ENDPOINT=$KAFKA_ENDPOINT \
    python3 tests/integration/test_deployment.py
    
    echo -e "${GREEN}✓ Deployment tests passed${NC}"
}

# Cost estimation
estimate_costs() {
    echo -e "${YELLOW}Estimated Monthly Costs:${NC}"
    echo "========================"
    echo "3x t3.large (PD):        ~\$180"
    echo "3x m5.2xlarge (TiKV):    ~\$825"
    echo "3x t3.xlarge (Ingest):   ~\$360"
    echo "Network Load Balancer:    ~\$25"
    echo "EBS Storage (2TB):       ~\$200"
    echo "Data Transfer:           Variable"
    echo "------------------------"
    echo "Total (estimated):       ~\$1,590/month"
    echo ""
    echo -e "${YELLOW}Note: Actual costs may vary based on usage${NC}"
}

# Cleanup function
cleanup() {
    echo -e "${YELLOW}Destroy deployment? (yes/no)${NC}"
    read -r response
    if [[ "$response" =~ ^([yY][eE][sS]|[yY])$ ]]; then
        cd deploy/terraform/aws
        terraform destroy -var="aws_region=$AWS_REGION" -var="key_pair_name=$KEY_PAIR_NAME"
        echo -e "${GREEN}✓ Resources destroyed${NC}"
    fi
}

# Main execution
main() {
    check_requirements
    select_region
    setup_keypair
    
    echo ""
    estimate_costs
    echo ""
    
    echo -e "${YELLOW}Continue with deployment? (yes/no)${NC}"
    read -r response
    if [[ ! "$response" =~ ^([yY][eE][sS]|[yY])$ ]]; then
        echo -e "${RED}Deployment cancelled${NC}"
        exit 0
    fi
    
    init_terraform
    plan_deployment
    apply_deployment
    configure_security
    
    echo -e "${YELLOW}Run deployment tests? (yes/no)${NC}"
    read -r response
    if [[ "$response" =~ ^([yY][eE][sS]|[yY])$ ]]; then
        test_deployment
    fi
    
    echo ""
    echo -e "${GREEN}Deployment successful!${NC}"
    echo "To connect to your Chronik Stream cluster:"
    echo "  export KAFKA_ENDPOINT=$(terraform output -raw kafka_endpoint)"
    echo "  kafkactl --brokers \$KAFKA_ENDPOINT get brokers"
    echo ""
    echo "To destroy the deployment later, run:"
    echo "  cd deploy/terraform/aws && terraform destroy"
}

# Set trap for cleanup
trap 'echo -e "\n${YELLOW}Interrupted. Run cleanup? (yes/no)${NC}"; read -r response; [[ "$response" =~ ^([yY][eE][sS]|[yY])$ ]] && cleanup' INT

# Run main function
main "$@"