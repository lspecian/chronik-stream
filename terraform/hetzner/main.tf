terraform {
  required_version = ">= 1.0"
  
  required_providers {
    hcloud = {
      source  = "hetznercloud/hcloud"
      version = "~> 1.45"
    }
    tls = {
      source  = "hashicorp/tls"
      version = "~> 4.0"
    }
  }
}

# Configure the Hetzner Cloud Provider
provider "hcloud" {
  token = var.hcloud_token
}

# Generate SSH key pair
resource "tls_private_key" "chronik" {
  algorithm = "RSA"
  rsa_bits  = 4096
}

# Upload SSH key to Hetzner
resource "hcloud_ssh_key" "chronik" {
  name       = "${var.project_name}-deploy"
  public_key = tls_private_key.chronik.public_key_openssh
}

# Create firewall for Chronik
resource "hcloud_firewall" "chronik" {
  name = "${var.project_name}-firewall"
  
  labels = {
    project = var.project_name
    managed_by = "terraform"
  }
  
  # SSH access
  rule {
    direction = "in"
    port      = "22"
    protocol  = "tcp"
    source_ips = var.ssh_allowed_ips
  }
  
  # Kafka API
  rule {
    direction = "in"
    port      = "9092"
    protocol  = "tcp"
    source_ips = var.kafka_allowed_ips
  }
  
  # Admin API
  rule {
    direction = "in"
    port      = "3000"
    protocol  = "tcp"
    source_ips = var.admin_allowed_ips
  }
  
  # Metrics
  rule {
    direction = "in"
    port      = "9090"
    protocol  = "tcp"
    source_ips = var.metrics_allowed_ips
  }
  
  # Search API (if enabled)
  dynamic "rule" {
    for_each = var.enable_search ? [1] : []
    content {
      direction = "in"
      port      = "9200"
      protocol  = "tcp"
      source_ips = var.search_allowed_ips
    }
  }
}

# Create network (optional, for multi-node setup)
resource "hcloud_network" "chronik" {
  count = var.deployment_type == "cluster" ? 1 : 0
  
  name     = "${var.project_name}-network"
  ip_range = var.network_cidr
  
  labels = {
    project = var.project_name
    managed_by = "terraform"
  }
}

resource "hcloud_network_subnet" "chronik" {
  count = var.deployment_type == "cluster" ? 1 : 0
  
  network_id   = hcloud_network.chronik[0].id
  type         = "server"
  network_zone = var.network_zone
  ip_range     = var.subnet_cidr
}

# Cloud-init configuration for servers
locals {
  cloud_init = templatefile("${path.module}/cloud-init.yaml", {
    docker_version     = var.docker_version
    chronik_image      = var.chronik_image
    chronik_version    = var.chronik_version
    storage_backend    = var.storage_backend
    s3_bucket          = var.s3_bucket
    s3_region          = var.s3_region
    aws_access_key_id  = var.aws_access_key_id
    aws_secret_key     = var.aws_secret_access_key
    rust_log          = var.rust_log
    data_volume_size  = var.data_volume_size
  })
}

# Create all-in-one server (for single deployment)
resource "hcloud_server" "chronik_all_in_one" {
  count = var.deployment_type == "single" ? 1 : 0
  
  name        = "${var.project_name}-all-in-one"
  server_type = var.server_type
  image       = var.server_image
  location    = var.location
  ssh_keys    = [hcloud_ssh_key.chronik.id]
  
  firewall_ids = [hcloud_firewall.chronik.id]
  
  user_data = local.cloud_init
  
  labels = {
    project    = var.project_name
    role       = "all-in-one"
    managed_by = "terraform"
  }
  
  public_net {
    ipv4_enabled = true
    ipv6_enabled = var.enable_ipv6
  }
}

# Create data volume for all-in-one server
resource "hcloud_volume" "chronik_data" {
  count = var.deployment_type == "single" && var.data_volume_size > 0 ? 1 : 0
  
  name     = "${var.project_name}-data"
  size     = var.data_volume_size
  location = var.location
  format   = "ext4"
  
  labels = {
    project    = var.project_name
    managed_by = "terraform"
  }
}

# Attach volume to all-in-one server
resource "hcloud_volume_attachment" "chronik_data" {
  count = var.deployment_type == "single" && var.data_volume_size > 0 ? 1 : 0
  
  volume_id = hcloud_volume.chronik_data[0].id
  server_id = hcloud_server.chronik_all_in_one[0].id
  automount = true
}

# Create controller nodes (for cluster deployment)
resource "hcloud_server" "chronik_controller" {
  count = var.deployment_type == "cluster" ? var.controller_count : 0
  
  name        = "${var.project_name}-controller-${count.index + 1}"
  server_type = var.controller_server_type
  image       = var.server_image
  location    = var.location
  ssh_keys    = [hcloud_ssh_key.chronik.id]
  
  firewall_ids = [hcloud_firewall.chronik.id]
  
  user_data = templatefile("${path.module}/cloud-init-controller.yaml", {
    docker_version  = var.docker_version
    chronik_image   = var.chronik_image
    chronik_version = var.chronik_version
    node_index      = count.index
    rust_log        = var.rust_log
  })
  
  labels = {
    project    = var.project_name
    role       = "controller"
    index      = tostring(count.index)
    managed_by = "terraform"
  }
  
  public_net {
    ipv4_enabled = true
    ipv6_enabled = var.enable_ipv6
  }
  
  dynamic "network" {
    for_each = var.deployment_type == "cluster" ? [1] : []
    content {
      network_id = hcloud_network.chronik[0].id
      ip         = cidrhost(var.subnet_cidr, 10 + count.index)
    }
  }
}

# Create ingest nodes (for cluster deployment)
resource "hcloud_server" "chronik_ingest" {
  count = var.deployment_type == "cluster" ? var.ingest_count : 0
  
  name        = "${var.project_name}-ingest-${count.index + 1}"
  server_type = var.ingest_server_type
  image       = var.server_image
  location    = var.location
  ssh_keys    = [hcloud_ssh_key.chronik.id]
  
  firewall_ids = [hcloud_firewall.chronik.id]
  
  user_data = templatefile("${path.module}/cloud-init-ingest.yaml", {
    docker_version    = var.docker_version
    chronik_image     = var.chronik_image
    chronik_version   = var.chronik_version
    controller_ips    = join(",", [for s in hcloud_server.chronik_controller : s.ipv4_address])
    node_index        = count.index
    storage_backend   = var.storage_backend
    s3_bucket         = var.s3_bucket
    s3_region         = var.s3_region
    aws_access_key_id = var.aws_access_key_id
    aws_secret_key    = var.aws_secret_access_key
    rust_log          = var.rust_log
  })
  
  labels = {
    project    = var.project_name
    role       = "ingest"
    index      = tostring(count.index)
    managed_by = "terraform"
  }
  
  public_net {
    ipv4_enabled = true
    ipv6_enabled = var.enable_ipv6
  }
  
  dynamic "network" {
    for_each = var.deployment_type == "cluster" ? [1] : []
    content {
      network_id = hcloud_network.chronik[0].id
      ip         = cidrhost(var.subnet_cidr, 20 + count.index)
    }
  }
}

# Create search nodes (optional, for cluster deployment)
resource "hcloud_server" "chronik_search" {
  count = var.deployment_type == "cluster" && var.enable_search ? var.search_count : 0
  
  name        = "${var.project_name}-search-${count.index + 1}"
  server_type = var.search_server_type
  image       = var.server_image
  location    = var.location
  ssh_keys    = [hcloud_ssh_key.chronik.id]
  
  firewall_ids = [hcloud_firewall.chronik.id]
  
  user_data = templatefile("${path.module}/cloud-init-search.yaml", {
    docker_version  = var.docker_version
    chronik_image   = var.chronik_image
    chronik_version = var.chronik_version
    controller_ips  = join(",", [for s in hcloud_server.chronik_controller : s.ipv4_address])
    node_index      = count.index
    rust_log        = var.rust_log
  })
  
  labels = {
    project    = var.project_name
    role       = "search"
    index      = tostring(count.index)
    managed_by = "terraform"
  }
  
  public_net {
    ipv4_enabled = true
    ipv6_enabled = var.enable_ipv6
  }
  
  dynamic "network" {
    for_each = var.deployment_type == "cluster" ? [1] : []
    content {
      network_id = hcloud_network.chronik[0].id
      ip         = cidrhost(var.subnet_cidr, 30 + count.index)
    }
  }
}

# Create load balancer (optional, for cluster deployment)
resource "hcloud_load_balancer" "chronik" {
  count = var.deployment_type == "cluster" && var.enable_load_balancer ? 1 : 0
  
  name               = "${var.project_name}-lb"
  load_balancer_type = var.load_balancer_type
  location           = var.location
  
  labels = {
    project    = var.project_name
    managed_by = "terraform"
  }
}

# Attach load balancer to network
resource "hcloud_load_balancer_network" "chronik" {
  count = var.deployment_type == "cluster" && var.enable_load_balancer ? 1 : 0
  
  load_balancer_id = hcloud_load_balancer.chronik[0].id
  network_id       = hcloud_network.chronik[0].id
  ip               = cidrhost(var.subnet_cidr, 5)
}

# Load balancer target for Kafka API
resource "hcloud_load_balancer_target" "kafka" {
  count = var.deployment_type == "cluster" && var.enable_load_balancer ? 1 : 0
  
  type             = "label_selector"
  load_balancer_id = hcloud_load_balancer.chronik[0].id
  label_selector   = "project=${var.project_name},role=ingest"
  use_private_ip   = true
}

# Load balancer service for Kafka API
resource "hcloud_load_balancer_service" "kafka" {
  count = var.deployment_type == "cluster" && var.enable_load_balancer ? 1 : 0
  
  load_balancer_id = hcloud_load_balancer.chronik[0].id
  protocol         = "tcp"
  listen_port      = 9092
  destination_port = 9092
  
  health_check {
    protocol = "tcp"
    port     = 9092
    interval = 15
    timeout  = 10
    retries  = 3
  }
}

# Load balancer service for Admin API
resource "hcloud_load_balancer_service" "admin" {
  count = var.deployment_type == "cluster" && var.enable_load_balancer ? 1 : 0
  
  load_balancer_id = hcloud_load_balancer.chronik[0].id
  protocol         = "http"
  listen_port      = 3000
  destination_port = 3000
  
  health_check {
    protocol = "http"
    port     = 3000
    interval = 15
    timeout  = 10
    retries  = 3
    http {
      path = "/health"
    }
  }
}