variable "hcloud_token" {
  description = "Hetzner Cloud API token"
  type        = string
  sensitive   = true
}

variable "project_name" {
  description = "Project name for resource naming"
  type        = string
  default     = "chronik-stream"
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

variable "location" {
  description = "Hetzner datacenter location"
  type        = string
  default     = "fsn1"
}

variable "network_zone" {
  description = "Network zone for the private network"
  type        = string
  default     = "eu-central"
}

# Server configuration
variable "server_type" {
  description = "Server type for all-in-one deployment"
  type        = string
  default     = "cpx21"  # 3 vCPU, 4GB RAM
}

variable "controller_server_type" {
  description = "Server type for controller nodes"
  type        = string
  default     = "cpx11"  # 2 vCPU, 2GB RAM
}

variable "ingest_server_type" {
  description = "Server type for ingest nodes"
  type        = string
  default     = "cpx21"  # 3 vCPU, 4GB RAM
}

variable "search_server_type" {
  description = "Server type for search nodes"
  type        = string
  default     = "cpx31"  # 4 vCPU, 8GB RAM
}

variable "server_image" {
  description = "Server image to use"
  type        = string
  default     = "debian-12"
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

variable "ingest_count" {
  description = "Number of ingest nodes"
  type        = number
  default     = 2
}

variable "search_count" {
  description = "Number of search nodes"
  type        = number
  default     = 1
}

# Storage configuration
variable "data_volume_size" {
  description = "Size of data volume in GB (0 to disable)"
  type        = number
  default     = 100
}

variable "storage_backend" {
  description = "Storage backend: embedded, s3, gcs, azure"
  type        = string
  default     = "embedded"
}

variable "s3_bucket" {
  description = "S3 bucket for storage (if using S3 backend)"
  type        = string
  default     = ""
}

variable "s3_region" {
  description = "S3 region"
  type        = string
  default     = "us-east-1"
}

variable "aws_access_key_id" {
  description = "AWS access key ID (if using S3 backend)"
  type        = string
  default     = ""
  sensitive   = true
}

variable "aws_secret_access_key" {
  description = "AWS secret access key (if using S3 backend)"
  type        = string
  default     = ""
  sensitive   = true
}

# Network configuration
variable "network_cidr" {
  description = "CIDR for the private network"
  type        = string
  default     = "10.0.0.0/16"
}

variable "subnet_cidr" {
  description = "CIDR for the subnet"
  type        = string
  default     = "10.0.1.0/24"
}

variable "enable_ipv6" {
  description = "Enable IPv6 on servers"
  type        = bool
  default     = false
}

# Firewall configuration
variable "ssh_allowed_ips" {
  description = "IP addresses allowed to connect via SSH"
  type        = list(string)
  default     = ["0.0.0.0/0", "::/0"]
}

variable "kafka_allowed_ips" {
  description = "IP addresses allowed to connect to Kafka API"
  type        = list(string)
  default     = ["0.0.0.0/0", "::/0"]
}

variable "admin_allowed_ips" {
  description = "IP addresses allowed to connect to Admin API"
  type        = list(string)
  default     = ["0.0.0.0/0", "::/0"]
}

variable "metrics_allowed_ips" {
  description = "IP addresses allowed to connect to metrics endpoint"
  type        = list(string)
  default     = ["0.0.0.0/0", "::/0"]
}

variable "search_allowed_ips" {
  description = "IP addresses allowed to connect to search API"
  type        = list(string)
  default     = ["0.0.0.0/0", "::/0"]
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

variable "load_balancer_type" {
  description = "Load balancer type"
  type        = string
  default     = "lb11"  # Smallest load balancer
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