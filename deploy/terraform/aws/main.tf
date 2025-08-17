terraform {
  required_version = ">= 1.0"
  
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
  }
}

provider "aws" {
  region = var.aws_region
}

variable "aws_region" {
  description = "AWS region"
  type        = string
  default     = "us-east-1"
}

variable "cluster_name" {
  description = "Name of the Chronik Stream cluster"
  type        = string
  default     = "chronik-stream"
}

variable "vpc_cidr" {
  description = "CIDR block for VPC"
  type        = string
  default     = "10.0.0.0/16"
}

variable "key_pair_name" {
  description = "Name of the EC2 key pair"
  type        = string
}

# VPC and Networking
module "vpc" {
  source = "terraform-aws-modules/vpc/aws"
  version = "~> 5.0"
  
  name = "${var.cluster_name}-vpc"
  cidr = var.vpc_cidr
  
  azs             = data.aws_availability_zones.available.names
  private_subnets = ["10.0.1.0/24", "10.0.2.0/24", "10.0.3.0/24"]
  public_subnets  = ["10.0.101.0/24", "10.0.102.0/24", "10.0.103.0/24"]
  
  enable_nat_gateway = true
  enable_vpn_gateway = false
  enable_dns_hostnames = true
  enable_dns_support = true
  
  tags = {
    Terraform = "true"
    Environment = "production"
    Application = "chronik-stream"
  }
}

data "aws_availability_zones" "available" {
  state = "available"
}

# Security Groups
resource "aws_security_group" "pd" {
  name_prefix = "${var.cluster_name}-pd-"
  vpc_id      = module.vpc.vpc_id
  
  ingress {
    from_port   = 2379
    to_port     = 2379
    protocol    = "tcp"
    cidr_blocks = [var.vpc_cidr]
  }
  
  ingress {
    from_port   = 2380
    to_port     = 2380
    protocol    = "tcp"
    cidr_blocks = [var.vpc_cidr]
  }
  
  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
  
  tags = {
    Name = "${var.cluster_name}-pd-sg"
  }
}

resource "aws_security_group" "tikv" {
  name_prefix = "${var.cluster_name}-tikv-"
  vpc_id      = module.vpc.vpc_id
  
  ingress {
    from_port   = 20160
    to_port     = 20160
    protocol    = "tcp"
    cidr_blocks = [var.vpc_cidr]
  }
  
  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
  
  tags = {
    Name = "${var.cluster_name}-tikv-sg"
  }
}

resource "aws_security_group" "ingest" {
  name_prefix = "${var.cluster_name}-ingest-"
  vpc_id      = module.vpc.vpc_id
  
  ingress {
    from_port   = 9092
    to_port     = 9092
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }
  
  ingress {
    from_port   = 9090
    to_port     = 9090
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }
  
  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
  
  tags = {
    Name = "${var.cluster_name}-ingest-sg"
  }
}

# Launch Templates
resource "aws_launch_template" "pd" {
  name_prefix   = "${var.cluster_name}-pd-"
  image_id      = data.aws_ami.ubuntu.id
  instance_type = "t3.large" # 2 vCPU, 8 GB RAM
  key_name      = var.key_pair_name
  
  vpc_security_group_ids = [aws_security_group.pd.id]
  
  block_device_mappings {
    device_name = "/dev/sda1"
    
    ebs {
      volume_size = 50
      volume_type = "gp3"
      iops        = 3000
      throughput  = 125
    }
  }
  
  user_data = base64encode(templatefile("${path.module}/user-data/pd.sh", {
    cluster_name = var.cluster_name
  }))
  
  tag_specifications {
    resource_type = "instance"
    tags = {
      Name = "${var.cluster_name}-pd"
      Type = "pd"
    }
  }
}

resource "aws_launch_template" "tikv" {
  name_prefix   = "${var.cluster_name}-tikv-"
  image_id      = data.aws_ami.ubuntu.id
  instance_type = "m5.2xlarge" # 8 vCPU, 32 GB RAM
  key_name      = var.key_pair_name
  
  vpc_security_group_ids = [aws_security_group.tikv.id]
  
  block_device_mappings {
    device_name = "/dev/sda1"
    
    ebs {
      volume_size = 500
      volume_type = "gp3"
      iops        = 10000
      throughput  = 250
    }
  }
  
  user_data = base64encode(templatefile("${path.module}/user-data/tikv.sh", {
    cluster_name = var.cluster_name
  }))
  
  tag_specifications {
    resource_type = "instance"
    tags = {
      Name = "${var.cluster_name}-tikv"
      Type = "tikv"
    }
  }
}

resource "aws_launch_template" "ingest" {
  name_prefix   = "${var.cluster_name}-ingest-"
  image_id      = data.aws_ami.ubuntu.id
  instance_type = "t3.xlarge" # 4 vCPU, 16 GB RAM
  key_name      = var.key_pair_name
  
  vpc_security_group_ids = [aws_security_group.ingest.id]
  
  block_device_mappings {
    device_name = "/dev/sda1"
    
    ebs {
      volume_size = 100
      volume_type = "gp3"
      iops        = 3000
      throughput  = 125
    }
  }
  
  user_data = base64encode(templatefile("${path.module}/user-data/ingest.sh", {
    cluster_name = var.cluster_name
  }))
  
  tag_specifications {
    resource_type = "instance"
    tags = {
      Name = "${var.cluster_name}-ingest"
      Type = "ingest"
    }
  }
}

# Auto Scaling Groups
resource "aws_autoscaling_group" "pd" {
  name               = "${var.cluster_name}-pd-asg"
  vpc_zone_identifier = module.vpc.private_subnets
  min_size           = 3
  max_size           = 3
  desired_capacity   = 3
  
  launch_template {
    id      = aws_launch_template.pd.id
    version = "$Latest"
  }
  
  tag {
    key                 = "Name"
    value               = "${var.cluster_name}-pd"
    propagate_at_launch = true
  }
}

resource "aws_autoscaling_group" "tikv" {
  name               = "${var.cluster_name}-tikv-asg"
  vpc_zone_identifier = module.vpc.private_subnets
  min_size           = 3
  max_size           = 9
  desired_capacity   = 3
  
  launch_template {
    id      = aws_launch_template.tikv.id
    version = "$Latest"
  }
  
  tag {
    key                 = "Name"
    value               = "${var.cluster_name}-tikv"
    propagate_at_launch = true
  }
}

resource "aws_autoscaling_group" "ingest" {
  name               = "${var.cluster_name}-ingest-asg"
  vpc_zone_identifier = module.vpc.private_subnets
  min_size           = 3
  max_size           = 10
  desired_capacity   = 3
  
  launch_template {
    id      = aws_launch_template.ingest.id
    version = "$Latest"
  }
  
  target_group_arns = [aws_lb_target_group.ingest.arn]
  
  tag {
    key                 = "Name"
    value               = "${var.cluster_name}-ingest"
    propagate_at_launch = true
  }
}

# Application Load Balancer
resource "aws_lb" "main" {
  name               = "${var.cluster_name}-alb"
  internal           = false
  load_balancer_type = "network"
  subnets            = module.vpc.public_subnets
  
  enable_deletion_protection = false
  
  tags = {
    Name = "${var.cluster_name}-alb"
  }
}

resource "aws_lb_target_group" "ingest" {
  name     = "${var.cluster_name}-ingest-tg"
  port     = 9092
  protocol = "TCP"
  vpc_id   = module.vpc.vpc_id
  
  health_check {
    enabled             = true
    healthy_threshold   = 2
    unhealthy_threshold = 2
    timeout             = 5
    interval            = 30
    protocol            = "TCP"
  }
  
  tags = {
    Name = "${var.cluster_name}-ingest-tg"
  }
}

resource "aws_lb_listener" "ingest" {
  load_balancer_arn = aws_lb.main.arn
  port              = "9092"
  protocol          = "TCP"
  
  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.ingest.arn
  }
}

# Data sources
data "aws_ami" "ubuntu" {
  most_recent = true
  owners      = ["099720109477"] # Canonical
  
  filter {
    name   = "name"
    values = ["ubuntu/images/hvm-ssd/ubuntu-jammy-22.04-amd64-server-*"]
  }
  
  filter {
    name   = "virtualization-type"
    values = ["hvm"]
  }
}

# Outputs
output "load_balancer_dns" {
  value = aws_lb.main.dns_name
  description = "DNS name of the load balancer"
}

output "vpc_id" {
  value = module.vpc.vpc_id
  description = "ID of the VPC"
}

output "kafka_endpoint" {
  value = "${aws_lb.main.dns_name}:9092"
  description = "Kafka connection endpoint"
}