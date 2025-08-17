variable "aws_region" {
  description = "AWS region"
  type        = string
  default     = "us-east-1"
}

variable "project_name" {
  description = "Project name for resource naming"
  type        = string
  default     = "chronik-stream"
}

variable "environment" {
  description = "Environment name"
  type        = string
  default     = "dev"
}

variable "deployment_type" {
  description = "Deployment type: 'single' for all-in-one or 'cluster' for multi-node"
  type        = string
  default     = "single"
  
  validation {
    condition     = contains(["single", "cluster"], var.deployment_type)
    error_message = "Deployment type must be 'single' or 'cluster'."
  }
}

# Network configuration
variable "vpc_cidr" {
  description = "CIDR block for VPC"
  type        = string
  default     = "10.0.0.0/16"
}

variable "availability_zones_count" {
  description = "Number of availability zones to use"
  type        = number
  default     = 2
}

variable "enable_nat_gateway" {
  description = "Enable NAT gateway for private subnets (cluster mode)"
  type        = bool
  default     = true
}

# Instance configuration
variable "instance_type" {
  description = "Instance type for all-in-one deployment"
  type        = string
  default     = "t3.medium"  # 2 vCPU, 4GB RAM
}

variable "controller_instance_type" {
  description = "Instance type for controller nodes"
  type        = string
  default     = "t3.small"  # 2 vCPU, 2GB RAM
}

variable "ingest_instance_type" {
  description = "Instance type for ingest nodes"
  type        = string
  default     = "t3.medium"  # 2 vCPU, 4GB RAM
}

variable "search_instance_type" {
  description = "Instance type for search nodes"
  type        = string
  default     = "t3.large"  # 2 vCPU, 8GB RAM
}

# Cluster sizing
variable "controller_count" {
  description = "Number of controller nodes (should be odd for consensus)"
  type        = number
  default     = 3
  
  validation {
    condition     = var.controller_count % 2 == 1
    error_message = "Controller count should be odd for consensus."
  }
}

variable "ingest_min_count" {
  description = "Minimum number of ingest nodes"
  type        = number
  default     = 2
}

variable "ingest_max_count" {
  description = "Maximum number of ingest nodes"
  type        = number
  default     = 10
}

variable "ingest_desired_count" {
  description = "Desired number of ingest nodes"
  type        = number
  default     = 2
}

variable "search_count" {
  description = "Number of search nodes"
  type        = number
  default     = 1
}

# Storage configuration
variable "root_volume_size" {
  description = "Size of root volume in GB"
  type        = number
  default     = 20
}

variable "root_volume_type" {
  description = "Type of root volume"
  type        = string
  default     = "gp3"
}

variable "data_volume_size" {
  description = "Size of data volume in GB (0 to disable)"
  type        = number
  default     = 100
}

variable "data_volume_type" {
  description = "Type of data volume"
  type        = string
  default     = "gp3"
}

variable "use_instance_store" {
  description = "Use instance store for ephemeral storage"
  type        = bool
  default     = false
}

variable "enable_encryption" {
  description = "Enable encryption for volumes"
  type        = bool
  default     = true
}

# S3 configuration
variable "storage_backend" {
  description = "Storage backend: embedded, s3"
  type        = string
  default     = "embedded"
}

variable "create_s3_bucket" {
  description = "Create S3 bucket for storage"
  type        = bool
  default     = false
}

variable "s3_bucket" {
  description = "S3 bucket name (will be auto-generated if empty)"
  type        = string
  default     = ""
}

variable "enable_s3_versioning" {
  description = "Enable versioning on S3 bucket"
  type        = bool
  default     = false
}

# Security configuration
variable "ssh_allowed_cidrs" {
  description = "CIDR blocks allowed to connect via SSH"
  type        = list(string)
  default     = ["0.0.0.0/0"]
}

variable "kafka_allowed_cidrs" {
  description = "CIDR blocks allowed to connect to Kafka API"
  type        = list(string)
  default     = ["0.0.0.0/0"]
}

variable "admin_allowed_cidrs" {
  description = "CIDR blocks allowed to connect to Admin API"
  type        = list(string)
  default     = ["0.0.0.0/0"]
}

variable "metrics_allowed_cidrs" {
  description = "CIDR blocks allowed to connect to metrics endpoint"
  type        = list(string)
  default     = ["0.0.0.0/0"]
}

variable "search_allowed_cidrs" {
  description = "CIDR blocks allowed to connect to search API"
  type        = list(string)
  default     = ["0.0.0.0/0"]
}

# Feature flags
variable "enable_search" {
  description = "Enable search nodes in cluster deployment"
  type        = bool
  default     = true
}

variable "enable_load_balancer" {
  description = "Enable load balancer for cluster deployment"
  type        = bool
  default     = true
}

variable "enable_auto_scaling" {
  description = "Enable auto-scaling for ingest nodes"
  type        = bool
  default     = false
}

variable "enable_monitoring" {
  description = "Enable CloudWatch monitoring"
  type        = bool
  default     = true
}

# Auto-scaling configuration
variable "scale_up_threshold" {
  description = "CPU utilization threshold for scaling up"
  type        = number
  default     = 70
}

variable "scale_down_threshold" {
  description = "CPU utilization threshold for scaling down"
  type        = number
  default     = 30
}

# Application configuration
variable "chronik_image" {
  description = "Chronik Docker image"
  type        = string
  default     = "ghcr.io/lspecian/chronik-stream"
}

variable "chronik_version" {
  description = "Chronik version tag"
  type        = string
  default     = "latest"
}

variable "docker_version" {
  description = "Docker version to install"
  type        = string
  default     = "24.0"
}

variable "rust_log" {
  description = "Rust log level"
  type        = string
  default     = "info"
}