#!/bin/bash
# Chronik Stream - AWS Deployment Script
# This script deploys Chronik Stream to AWS using EC2 and the all-in-one image

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

echo -e "${BLUE}================================================${NC}"
echo -e "${BLUE}  Chronik Stream - AWS EC2 Deployment${NC}"
echo -e "${BLUE}================================================${NC}"

# Configuration
PROJECT_NAME="${PROJECT_NAME:-chronik-stream}"
REGION="${AWS_REGION:-us-east-1}"
INSTANCE_TYPE="${INSTANCE_TYPE:-t3.medium}"  # 2 vCPU, 4GB RAM
KEY_NAME="${KEY_NAME:-chronik-deploy}"
SECURITY_GROUP="${SECURITY_GROUP:-chronik-sg}"
VPC_ID="${VPC_ID:-}"  # Will use default VPC if not specified

# Check for AWS CLI
if ! command -v aws &> /dev/null; then
    echo -e "${RED}Error: AWS CLI is not installed${NC}"
    echo "Install with: brew install awscli (macOS) or download from https://aws.amazon.com/cli/"
    exit 1
fi

# Check AWS credentials
if ! aws sts get-caller-identity &> /dev/null; then
    echo -e "${RED}Error: AWS credentials not configured${NC}"
    echo "Run: aws configure"
    exit 1
fi

# Get the latest Amazon Linux 2023 AMI
get_ami_id() {
    aws ec2 describe-images \
        --region "$REGION" \
        --owners amazon \
        --filters \
        "Name=name,Values=al2023-ami-*-x86_64" \
        "Name=state,Values=available" \
        --query 'Images | sort_by(@, &CreationDate) | [-1].ImageId' \
        --output text
}

# Create or get VPC
setup_vpc() {
    if [ -z "$VPC_ID" ]; then
        echo -e "${YELLOW}Using default VPC...${NC}"
        VPC_ID=$(aws ec2 describe-vpcs \
            --region "$REGION" \
            --filters "Name=is-default,Values=true" \
            --query 'Vpcs[0].VpcId' \
            --output text)
    fi
    
    if [ "$VPC_ID" == "None" ] || [ -z "$VPC_ID" ]; then
        echo -e "${RED}Error: No default VPC found${NC}"
        exit 1
    fi
    
    echo -e "${GREEN}Using VPC: $VPC_ID${NC}"
}

# Create key pair if needed
create_key_pair() {
    if ! aws ec2 describe-key-pairs \
        --region "$REGION" \
        --key-names "$KEY_NAME" &> /dev/null; then
        
        echo -e "${YELLOW}Creating SSH key pair...${NC}"
        
        # Generate local key if it doesn't exist
        if [ ! -f ~/.ssh/chronik_deploy_rsa ]; then
            ssh-keygen -t rsa -b 4096 -f ~/.ssh/chronik_deploy_rsa -N "" -C "chronik-deploy"
        fi
        
        # Import to AWS
        aws ec2 import-key-pair \
            --region "$REGION" \
            --key-name "$KEY_NAME" \
            --public-key-material fileb://~/.ssh/chronik_deploy_rsa.pub
        
        echo -e "${GREEN}Key pair created: $KEY_NAME${NC}"
    fi
}

# Create security group
create_security_group() {
    local sg_id
    
    # Check if security group exists
    sg_id=$(aws ec2 describe-security-groups \
        --region "$REGION" \
        --filters "Name=group-name,Values=$SECURITY_GROUP" "Name=vpc-id,Values=$VPC_ID" \
        --query 'SecurityGroups[0].GroupId' \
        --output text 2>/dev/null)
    
    if [ "$sg_id" == "None" ] || [ -z "$sg_id" ]; then
        echo -e "${YELLOW}Creating security group...${NC}"
        
        sg_id=$(aws ec2 create-security-group \
            --region "$REGION" \
            --group-name "$SECURITY_GROUP" \
            --description "Security group for Chronik Stream" \
            --vpc-id "$VPC_ID" \
            --query 'GroupId' \
            --output text)
        
        # Add ingress rules
        aws ec2 authorize-security-group-ingress \
            --region "$REGION" \
            --group-id "$sg_id" \
            --protocol tcp \
            --port 22 \
            --cidr 0.0.0.0/0 \
            --output text > /dev/null
        
        aws ec2 authorize-security-group-ingress \
            --region "$REGION" \
            --group-id "$sg_id" \
            --protocol tcp \
            --port 9092 \
            --cidr 0.0.0.0/0 \
            --output text > /dev/null
        
        aws ec2 authorize-security-group-ingress \
            --region "$REGION" \
            --group-id "$sg_id" \
            --protocol tcp \
            --port 3000 \
            --cidr 0.0.0.0/0 \
            --output text > /dev/null
        
        aws ec2 authorize-security-group-ingress \
            --region "$REGION" \
            --group-id "$sg_id" \
            --protocol tcp \
            --port 9090 \
            --cidr 0.0.0.0/0 \
            --output text > /dev/null
        
        echo -e "${GREEN}Security group created: $sg_id${NC}"
    else
        echo -e "${GREEN}Using existing security group: $sg_id${NC}"
    fi
    
    echo "$sg_id"
}

# Create user data script
create_user_data() {
    cat <<'EOF'
#!/bin/bash
# Update system
dnf update -y

# Install Docker
dnf install -y docker
systemctl enable docker
systemctl start docker

# Add ec2-user to docker group
usermod -aG docker ec2-user

# Create data directory
mkdir -p /data/chronik
chown -R 1000:1000 /data/chronik

# Pull Chronik Stream image
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

# Install CloudWatch agent for monitoring
dnf install -y amazon-cloudwatch-agent
/opt/aws/amazon-cloudwatch-agent/bin/amazon-cloudwatch-agent-ctl \
    -a fetch-config \
    -m ec2 \
    -s -c ssm:AmazonCloudWatch-DefaultConfig

echo "Chronik Stream deployment complete!"
EOF
}

# Launch EC2 instance
launch_instance() {
    local ami_id="$1"
    local sg_id="$2"
    local user_data="$3"
    local instance_name="${PROJECT_NAME}-${4}"
    
    echo -e "${YELLOW}Launching EC2 instance: $instance_name...${NC}"
    
    # Launch instance
    local instance_id=$(aws ec2 run-instances \
        --region "$REGION" \
        --image-id "$ami_id" \
        --instance-type "$INSTANCE_TYPE" \
        --key-name "$KEY_NAME" \
        --security-group-ids "$sg_id" \
        --user-data "$user_data" \
        --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=$instance_name},{Key=Project,Value=$PROJECT_NAME}]" \
        --block-device-mappings "DeviceName=/dev/xvda,Ebs={VolumeSize=20,VolumeType=gp3}" \
        --query 'Instances[0].InstanceId' \
        --output text)
    
    echo -e "${GREEN}Instance launched: $instance_id${NC}"
    
    # Wait for instance to be running
    echo -e "${YELLOW}Waiting for instance to start...${NC}"
    aws ec2 wait instance-running \
        --region "$REGION" \
        --instance-ids "$instance_id"
    
    # Get public IP
    local public_ip=$(aws ec2 describe-instances \
        --region "$REGION" \
        --instance-ids "$instance_id" \
        --query 'Reservations[0].Instances[0].PublicIpAddress' \
        --output text)
    
    echo -e "${GREEN}Instance ready: $public_ip${NC}"
    echo "$instance_id:$public_ip"
}

# Create EBS volume for persistent storage
create_storage_volume() {
    local instance_id="$1"
    local size="${2:-100}"  # Default 100GB
    
    echo -e "${YELLOW}Creating EBS volume for persistent storage...${NC}"
    
    # Get instance availability zone
    local az=$(aws ec2 describe-instances \
        --region "$REGION" \
        --instance-ids "$instance_id" \
        --query 'Reservations[0].Instances[0].Placement.AvailabilityZone' \
        --output text)
    
    # Create volume
    local volume_id=$(aws ec2 create-volume \
        --region "$REGION" \
        --availability-zone "$az" \
        --size "$size" \
        --volume-type gp3 \
        --tag-specifications "ResourceType=volume,Tags=[{Key=Name,Value=${PROJECT_NAME}-data},{Key=Project,Value=$PROJECT_NAME}]" \
        --query 'VolumeId' \
        --output text)
    
    # Wait for volume to be available
    aws ec2 wait volume-available \
        --region "$REGION" \
        --volume-ids "$volume_id"
    
    # Attach volume
    aws ec2 attach-volume \
        --region "$REGION" \
        --volume-id "$volume_id" \
        --instance-id "$instance_id" \
        --device /dev/sdf
    
    echo -e "${GREEN}Storage volume attached: $volume_id${NC}"
}

# Main deployment
echo -e "${BLUE}Starting AWS deployment...${NC}"

# Setup VPC
setup_vpc

# Create key pair
create_key_pair

# Get AMI ID
AMI_ID=$(get_ami_id)
echo -e "${GREEN}Using AMI: $AMI_ID${NC}"

# Create security group
SG_ID=$(create_security_group)

# Create user data
USER_DATA=$(create_user_data)

# Deploy single instance or cluster
if [ "$1" == "cluster" ]; then
    echo -e "${BLUE}Deploying Chronik Stream cluster...${NC}"
    
    # Launch controller
    CONTROLLER_INFO=$(launch_instance "$AMI_ID" "$SG_ID" "$USER_DATA" "controller")
    CONTROLLER_ID=$(echo "$CONTROLLER_INFO" | cut -d: -f1)
    CONTROLLER_IP=$(echo "$CONTROLLER_INFO" | cut -d: -f2)
    
    # Launch ingest nodes
    INGEST1_INFO=$(launch_instance "$AMI_ID" "$SG_ID" "$USER_DATA" "ingest-1")
    INGEST1_ID=$(echo "$INGEST1_INFO" | cut -d: -f1)
    INGEST1_IP=$(echo "$INGEST1_INFO" | cut -d: -f2)
    
    INGEST2_INFO=$(launch_instance "$AMI_ID" "$SG_ID" "$USER_DATA" "ingest-2")
    INGEST2_ID=$(echo "$INGEST2_INFO" | cut -d: -f1)
    INGEST2_IP=$(echo "$INGEST2_INFO" | cut -d: -f2)
    
    echo -e "${GREEN}Cluster deployed successfully!${NC}"
    echo -e "Controller: ${BLUE}$CONTROLLER_IP${NC}"
    echo -e "Ingest 1: ${BLUE}$INGEST1_IP${NC}"
    echo -e "Ingest 2: ${BLUE}$INGEST2_IP${NC}"
    
else
    echo -e "${BLUE}Deploying single all-in-one instance...${NC}"
    
    # Launch instance
    INSTANCE_INFO=$(launch_instance "$AMI_ID" "$SG_ID" "$USER_DATA" "all-in-one")
    INSTANCE_ID=$(echo "$INSTANCE_INFO" | cut -d: -f1)
    INSTANCE_IP=$(echo "$INSTANCE_INFO" | cut -d: -f2)
    
    # Create storage volume
    if [ "$2" == "with-storage" ]; then
        create_storage_volume "$INSTANCE_ID" "${3:-100}"
    fi
    
    # Wait for services to start
    echo -e "${YELLOW}Waiting for services to initialize...${NC}"
    sleep 60
    
    echo -e "${GREEN}================================================${NC}"
    echo -e "${GREEN}Deployment successful!${NC}"
    echo -e "${GREEN}================================================${NC}"
    echo
    echo -e "Instance ID: ${BLUE}$INSTANCE_ID${NC}"
    echo -e "Public IP: ${BLUE}$INSTANCE_IP${NC}"
    echo
    echo -e "${YELLOW}Test your deployment:${NC}"
    echo -e "  Kafka API: ${BLUE}kafkactl --brokers $INSTANCE_IP:9092 get brokers${NC}"
    echo -e "  Admin API: ${BLUE}curl http://$INSTANCE_IP:3000/health${NC}"
    echo -e "  Metrics:   ${BLUE}curl http://$INSTANCE_IP:9090/metrics${NC}"
    echo
    echo -e "${YELLOW}SSH access:${NC}"
    echo -e "  ${BLUE}ssh -i ~/.ssh/chronik_deploy_rsa ec2-user@$INSTANCE_IP${NC}"
    echo
    echo -e "${YELLOW}View logs:${NC}"
    echo -e "  ${BLUE}ssh -i ~/.ssh/chronik_deploy_rsa ec2-user@$INSTANCE_IP 'sudo docker logs chronik'${NC}"
    echo
    echo -e "${YELLOW}CloudWatch Logs:${NC}"
    echo -e "  View in AWS Console: CloudWatch > Log groups > /aws/ec2/chronik"
fi

# Save deployment info
cat > aws-deployment-info.json <<EOF
{
  "provider": "aws",
  "region": "$REGION",
  "instance_type": "$INSTANCE_TYPE",
  "instance_id": "${INSTANCE_ID:-$CONTROLLER_ID}",
  "public_ip": "${INSTANCE_IP:-$CONTROLLER_IP}",
  "kafka_endpoint": "${INSTANCE_IP:-$CONTROLLER_IP}:9092",
  "admin_endpoint": "http://${INSTANCE_IP:-$CONTROLLER_IP}:3000",
  "metrics_endpoint": "http://${INSTANCE_IP:-$CONTROLLER_IP}:9090/metrics",
  "ssh_key": "~/.ssh/chronik_deploy_rsa",
  "security_group": "$SG_ID",
  "deployed_at": "$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
}
EOF

echo -e "${GREEN}Deployment info saved to aws-deployment-info.json${NC}"

# Function to destroy deployment
if [ "$1" == "destroy" ]; then
    echo -e "${RED}Destroying deployment...${NC}"
    
    # Get all instances with our project tag
    INSTANCE_IDS=$(aws ec2 describe-instances \
        --region "$REGION" \
        --filters "Name=tag:Project,Values=$PROJECT_NAME" "Name=instance-state-name,Values=running" \
        --query 'Reservations[].Instances[].InstanceId' \
        --output text)
    
    if [ -n "$INSTANCE_IDS" ]; then
        echo -e "${YELLOW}Terminating instances: $INSTANCE_IDS${NC}"
        aws ec2 terminate-instances \
            --region "$REGION" \
            --instance-ids $INSTANCE_IDS
        
        # Wait for termination
        aws ec2 wait instance-terminated \
            --region "$REGION" \
            --instance-ids $INSTANCE_IDS
    fi
    
    # Delete volumes
    VOLUME_IDS=$(aws ec2 describe-volumes \
        --region "$REGION" \
        --filters "Name=tag:Project,Values=$PROJECT_NAME" \
        --query 'Volumes[].VolumeId' \
        --output text)
    
    if [ -n "$VOLUME_IDS" ]; then
        echo -e "${YELLOW}Deleting volumes: $VOLUME_IDS${NC}"
        for volume in $VOLUME_IDS; do
            aws ec2 delete-volume \
                --region "$REGION" \
                --volume-id "$volume"
        done
    fi
    
    echo -e "${GREEN}Deployment destroyed${NC}"
    exit 0
fi

echo -e "${GREEN}Deployment script completed!${NC}"