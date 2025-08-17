terraform {
  required_version = ">= 1.0"
  
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
    tls = {
      source  = "hashicorp/tls"
      version = "~> 4.0"
    }
  }
}

# Configure AWS Provider
provider "aws" {
  region = var.aws_region
  
  default_tags {
    tags = {
      Project    = var.project_name
      ManagedBy  = "terraform"
      Environment = var.environment
    }
  }
}

# Data sources
data "aws_availability_zones" "available" {
  state = "available"
}

data "aws_ami" "amazon_linux" {
  most_recent = true
  owners      = ["amazon"]
  
  filter {
    name   = "name"
    values = ["al2023-ami-*-x86_64"]
  }
  
  filter {
    name   = "virtualization-type"
    values = ["hvm"]
  }
}

# Generate SSH key pair
resource "tls_private_key" "chronik" {
  algorithm = "RSA"
  rsa_bits  = 4096
}

# Create key pair in AWS
resource "aws_key_pair" "chronik" {
  key_name   = "${var.project_name}-deploy"
  public_key = tls_private_key.chronik.public_key_openssh
}

# Create VPC
resource "aws_vpc" "chronik" {
  cidr_block           = var.vpc_cidr
  enable_dns_hostnames = true
  enable_dns_support   = true
  
  tags = {
    Name = "${var.project_name}-vpc"
  }
}

# Create Internet Gateway
resource "aws_internet_gateway" "chronik" {
  vpc_id = aws_vpc.chronik.id
  
  tags = {
    Name = "${var.project_name}-igw"
  }
}

# Create public subnets
resource "aws_subnet" "public" {
  count = min(length(data.aws_availability_zones.available.names), var.availability_zones_count)
  
  vpc_id                  = aws_vpc.chronik.id
  cidr_block              = cidrsubnet(var.vpc_cidr, 8, count.index)
  availability_zone       = data.aws_availability_zones.available.names[count.index]
  map_public_ip_on_launch = true
  
  tags = {
    Name = "${var.project_name}-public-${count.index + 1}"
    Type = "public"
  }
}

# Create private subnets (for cluster deployment)
resource "aws_subnet" "private" {
  count = var.deployment_type == "cluster" ? min(length(data.aws_availability_zones.available.names), var.availability_zones_count) : 0
  
  vpc_id            = aws_vpc.chronik.id
  cidr_block        = cidrsubnet(var.vpc_cidr, 8, count.index + 100)
  availability_zone = data.aws_availability_zones.available.names[count.index]
  
  tags = {
    Name = "${var.project_name}-private-${count.index + 1}"
    Type = "private"
  }
}

# Create route table for public subnets
resource "aws_route_table" "public" {
  vpc_id = aws_vpc.chronik.id
  
  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.chronik.id
  }
  
  tags = {
    Name = "${var.project_name}-public-rt"
  }
}

# Associate route table with public subnets
resource "aws_route_table_association" "public" {
  count = length(aws_subnet.public)
  
  subnet_id      = aws_subnet.public[count.index].id
  route_table_id = aws_route_table.public.id
}

# Create NAT Gateway for private subnets (cluster mode)
resource "aws_eip" "nat" {
  count = var.deployment_type == "cluster" && var.enable_nat_gateway ? 1 : 0
  
  domain = "vpc"
  
  tags = {
    Name = "${var.project_name}-nat-eip"
  }
}

resource "aws_nat_gateway" "chronik" {
  count = var.deployment_type == "cluster" && var.enable_nat_gateway ? 1 : 0
  
  allocation_id = aws_eip.nat[0].id
  subnet_id     = aws_subnet.public[0].id
  
  tags = {
    Name = "${var.project_name}-nat"
  }
  
  depends_on = [aws_internet_gateway.chronik]
}

# Create route table for private subnets
resource "aws_route_table" "private" {
  count = var.deployment_type == "cluster" ? 1 : 0
  
  vpc_id = aws_vpc.chronik.id
  
  dynamic "route" {
    for_each = var.enable_nat_gateway ? [1] : []
    content {
      cidr_block     = "0.0.0.0/0"
      nat_gateway_id = aws_nat_gateway.chronik[0].id
    }
  }
  
  tags = {
    Name = "${var.project_name}-private-rt"
  }
}

# Associate route table with private subnets
resource "aws_route_table_association" "private" {
  count = length(aws_subnet.private)
  
  subnet_id      = aws_subnet.private[count.index].id
  route_table_id = aws_route_table.private[0].id
}

# Security Groups
resource "aws_security_group" "chronik" {
  name        = "${var.project_name}-sg"
  description = "Security group for Chronik Stream"
  vpc_id      = aws_vpc.chronik.id
  
  # SSH access
  ingress {
    from_port   = 22
    to_port     = 22
    protocol    = "tcp"
    cidr_blocks = var.ssh_allowed_cidrs
  }
  
  # Kafka API
  ingress {
    from_port   = 9092
    to_port     = 9092
    protocol    = "tcp"
    cidr_blocks = var.kafka_allowed_cidrs
  }
  
  # Admin API
  ingress {
    from_port   = 3000
    to_port     = 3000
    protocol    = "tcp"
    cidr_blocks = var.admin_allowed_cidrs
  }
  
  # Metrics
  ingress {
    from_port   = 9090
    to_port     = 9090
    protocol    = "tcp"
    cidr_blocks = var.metrics_allowed_cidrs
  }
  
  # Search API (if enabled)
  dynamic "ingress" {
    for_each = var.enable_search ? [1] : []
    content {
      from_port   = 9200
      to_port     = 9200
      protocol    = "tcp"
      cidr_blocks = var.search_allowed_cidrs
    }
  }
  
  # Internal communication (cluster mode)
  ingress {
    from_port = 0
    to_port   = 65535
    protocol  = "tcp"
    self      = true
  }
  
  # Outbound traffic
  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
  
  tags = {
    Name = "${var.project_name}-sg"
  }
}

# User data script for instances
locals {
  user_data = templatefile("${path.module}/user-data.sh", {
    docker_version     = var.docker_version
    chronik_image      = var.chronik_image
    chronik_version    = var.chronik_version
    storage_backend    = var.storage_backend
    s3_bucket          = var.s3_bucket
    s3_region          = var.aws_region
    rust_log           = var.rust_log
    use_instance_store = var.use_instance_store
  })
}

# All-in-one instance (single deployment)
resource "aws_instance" "chronik_all_in_one" {
  count = var.deployment_type == "single" ? 1 : 0
  
  ami           = data.aws_ami.amazon_linux.id
  instance_type = var.instance_type
  key_name      = aws_key_pair.chronik.key_name
  
  subnet_id                   = aws_subnet.public[0].id
  vpc_security_group_ids      = [aws_security_group.chronik.id]
  associate_public_ip_address = true
  
  user_data = local.user_data
  
  root_block_device {
    volume_type = var.root_volume_type
    volume_size = var.root_volume_size
    encrypted   = var.enable_encryption
  }
  
  tags = {
    Name = "${var.project_name}-all-in-one"
    Role = "all-in-one"
  }
}

# EBS volume for persistent storage (single deployment)
resource "aws_ebs_volume" "chronik_data" {
  count = var.deployment_type == "single" && var.data_volume_size > 0 ? 1 : 0
  
  availability_zone = aws_instance.chronik_all_in_one[0].availability_zone
  size              = var.data_volume_size
  type              = var.data_volume_type
  encrypted         = var.enable_encryption
  
  tags = {
    Name = "${var.project_name}-data"
  }
}

resource "aws_volume_attachment" "chronik_data" {
  count = var.deployment_type == "single" && var.data_volume_size > 0 ? 1 : 0
  
  device_name = "/dev/sdf"
  volume_id   = aws_ebs_volume.chronik_data[0].id
  instance_id = aws_instance.chronik_all_in_one[0].id
}

# Launch template for cluster nodes
resource "aws_launch_template" "controller" {
  count = var.deployment_type == "cluster" ? 1 : 0
  
  name_prefix   = "${var.project_name}-controller-"
  image_id      = data.aws_ami.amazon_linux.id
  instance_type = var.controller_instance_type
  key_name      = aws_key_pair.chronik.key_name
  
  vpc_security_group_ids = [aws_security_group.chronik.id]
  
  user_data = base64encode(templatefile("${path.module}/user-data-controller.sh", {
    docker_version  = var.docker_version
    chronik_image   = var.chronik_image
    chronik_version = var.chronik_version
    rust_log        = var.rust_log
  }))
  
  block_device_mappings {
    device_name = "/dev/xvda"
    
    ebs {
      volume_type = var.root_volume_type
      volume_size = var.root_volume_size
      encrypted   = var.enable_encryption
    }
  }
  
  tag_specifications {
    resource_type = "instance"
    
    tags = {
      Name = "${var.project_name}-controller"
      Role = "controller"
    }
  }
}

# Auto Scaling Group for controllers
resource "aws_autoscaling_group" "controller" {
  count = var.deployment_type == "cluster" ? 1 : 0
  
  name               = "${var.project_name}-controller-asg"
  min_size           = var.controller_count
  max_size           = var.controller_count
  desired_capacity   = var.controller_count
  vpc_zone_identifier = aws_subnet.private[*].id
  
  launch_template {
    id      = aws_launch_template.controller[0].id
    version = "$Latest"
  }
  
  tag {
    key                 = "Name"
    value               = "${var.project_name}-controller"
    propagate_at_launch = true
  }
  
  tag {
    key                 = "Role"
    value               = "controller"
    propagate_at_launch = true
  }
}

# Application Load Balancer (cluster mode)
resource "aws_lb" "chronik" {
  count = var.deployment_type == "cluster" && var.enable_load_balancer ? 1 : 0
  
  name               = "${var.project_name}-alb"
  internal           = false
  load_balancer_type = "application"
  security_groups    = [aws_security_group.chronik.id]
  subnets            = aws_subnet.public[*].id
  
  enable_deletion_protection = false
  enable_http2              = true
  
  tags = {
    Name = "${var.project_name}-alb"
  }
}

# Network Load Balancer for Kafka (cluster mode)
resource "aws_lb" "kafka" {
  count = var.deployment_type == "cluster" && var.enable_load_balancer ? 1 : 0
  
  name               = "${var.project_name}-kafka-nlb"
  internal           = false
  load_balancer_type = "network"
  subnets            = aws_subnet.public[*].id
  
  enable_deletion_protection = false
  
  tags = {
    Name = "${var.project_name}-kafka-nlb"
  }
}

# Target groups and listeners
resource "aws_lb_target_group" "admin" {
  count = var.deployment_type == "cluster" && var.enable_load_balancer ? 1 : 0
  
  name     = "${var.project_name}-admin-tg"
  port     = 3000
  protocol = "HTTP"
  vpc_id   = aws_vpc.chronik.id
  
  health_check {
    enabled             = true
    healthy_threshold   = 2
    unhealthy_threshold = 2
    timeout             = 5
    interval            = 30
    path                = "/health"
    matcher             = "200"
  }
  
  tags = {
    Name = "${var.project_name}-admin-tg"
  }
}

resource "aws_lb_listener" "admin" {
  count = var.deployment_type == "cluster" && var.enable_load_balancer ? 1 : 0
  
  load_balancer_arn = aws_lb.chronik[0].arn
  port              = "3000"
  protocol          = "HTTP"
  
  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.admin[0].arn
  }
}

# S3 bucket for storage (optional)
resource "aws_s3_bucket" "chronik" {
  count = var.create_s3_bucket ? 1 : 0
  
  bucket = var.s3_bucket != "" ? var.s3_bucket : "${var.project_name}-${data.aws_caller_identity.current.account_id}"
  
  tags = {
    Name = "${var.project_name}-storage"
  }
}

data "aws_caller_identity" "current" {}

resource "aws_s3_bucket_versioning" "chronik" {
  count = var.create_s3_bucket ? 1 : 0
  
  bucket = aws_s3_bucket.chronik[0].id
  
  versioning_configuration {
    status = var.enable_s3_versioning ? "Enabled" : "Disabled"
  }
}

resource "aws_s3_bucket_server_side_encryption_configuration" "chronik" {
  count = var.create_s3_bucket && var.enable_encryption ? 1 : 0
  
  bucket = aws_s3_bucket.chronik[0].id
  
  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
  }
}