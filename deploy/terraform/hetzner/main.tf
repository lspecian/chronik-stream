terraform {
  required_version = ">= 1.0"
  
  required_providers {
    hcloud = {
      source  = "hetznercloud/hcloud"
      version = "~> 1.45"
    }
  }
}

provider "hcloud" {
  token = var.hcloud_token
}

variable "hcloud_token" {
  description = "Hetzner Cloud API Token"
  type        = string
  sensitive   = true
}

variable "cluster_name" {
  description = "Name of the Chronik Stream cluster"
  type        = string
  default     = "chronik-stream"
}

variable "region" {
  description = "Hetzner Cloud region"
  type        = string
  default     = "nbg1" # Nuremberg
}

# Network
resource "hcloud_network" "chronik" {
  name     = "${var.cluster_name}-network"
  ip_range = "10.0.0.0/16"
}

resource "hcloud_network_subnet" "chronik" {
  network_id   = hcloud_network.chronik.id
  type         = "cloud"
  network_zone = "eu-central"
  ip_range     = "10.0.1.0/24"
}

# SSH Key
resource "hcloud_ssh_key" "default" {
  name       = "${var.cluster_name}-key"
  public_key = file("~/.ssh/id_rsa.pub")
}

# Firewall
resource "hcloud_firewall" "chronik" {
  name = "${var.cluster_name}-firewall"
  
  rule {
    direction = "in"
    protocol  = "tcp"
    port      = "22"
    source_ips = ["0.0.0.0/0", "::/0"]
  }
  
  rule {
    direction = "in"
    protocol  = "tcp"
    port      = "9092"
    source_ips = ["0.0.0.0/0", "::/0"]
  }
  
  rule {
    direction = "in"
    protocol  = "tcp"
    port      = "3000"
    source_ips = ["0.0.0.0/0", "::/0"]
  }
  
  rule {
    direction = "in"
    protocol  = "tcp"
    port      = "9090"
    source_ips = ["0.0.0.0/0", "::/0"]
  }
}

# Load Balancer
resource "hcloud_load_balancer" "chronik" {
  name               = "${var.cluster_name}-lb"
  load_balancer_type = "lb11"
  location           = var.region
}

resource "hcloud_load_balancer_network" "chronik" {
  load_balancer_id = hcloud_load_balancer.chronik.id
  network_id       = hcloud_network.chronik.id
}

resource "hcloud_load_balancer_service" "kafka" {
  load_balancer_id = hcloud_load_balancer.chronik.id
  protocol         = "tcp"
  listen_port      = 9092
  destination_port = 9092
}

resource "hcloud_load_balancer_target" "ingest" {
  count            = 3
  type             = "server"
  load_balancer_id = hcloud_load_balancer.chronik.id
  server_id        = hcloud_server.ingest[count.index].id
}

# TiKV PD Servers
resource "hcloud_server" "pd" {
  count       = 3
  name        = "${var.cluster_name}-pd-${count.index}"
  server_type = "cpx31" # 4 vCPU, 8 GB RAM
  image       = "ubuntu-22.04"
  location    = var.region
  ssh_keys    = [hcloud_ssh_key.default.id]
  firewall_ids = [hcloud_firewall.chronik.id]
  
  network {
    network_id = hcloud_network.chronik.id
    ip         = "10.0.1.${10 + count.index}"
  }
  
  user_data = templatefile("${path.module}/user-data/pd.yaml", {
    node_index = count.index
    cluster_name = var.cluster_name
  })
  
  labels = {
    type = "pd"
    cluster = var.cluster_name
  }
}

# TiKV Storage Servers
resource "hcloud_server" "tikv" {
  count       = 3
  name        = "${var.cluster_name}-tikv-${count.index}"
  server_type = "cpx41" # 8 vCPU, 16 GB RAM
  image       = "ubuntu-22.04"
  location    = var.region
  ssh_keys    = [hcloud_ssh_key.default.id]
  firewall_ids = [hcloud_firewall.chronik.id]
  
  network {
    network_id = hcloud_network.chronik.id
    ip         = "10.0.1.${20 + count.index}"
  }
  
  user_data = templatefile("${path.module}/user-data/tikv.yaml", {
    node_index = count.index
    cluster_name = var.cluster_name
    pd_servers = join(",", [for i in range(3) : "10.0.1.${10 + i}:2379"])
  })
  
  labels = {
    type = "tikv"
    cluster = var.cluster_name
  }
}

# Chronik Ingest Servers
resource "hcloud_server" "ingest" {
  count       = 3
  name        = "${var.cluster_name}-ingest-${count.index}"
  server_type = "cpx31" # 4 vCPU, 8 GB RAM
  image       = "ubuntu-22.04"
  location    = var.region
  ssh_keys    = [hcloud_ssh_key.default.id]
  firewall_ids = [hcloud_firewall.chronik.id]
  
  network {
    network_id = hcloud_network.chronik.id
    ip         = "10.0.1.${30 + count.index}"
  }
  
  user_data = templatefile("${path.module}/user-data/ingest.yaml", {
    node_index = count.index
    cluster_name = var.cluster_name
    pd_servers = join(",", [for i in range(3) : "10.0.1.${10 + i}:2379"])
  })
  
  labels = {
    type = "ingest"
    cluster = var.cluster_name
  }
}

# Outputs
output "load_balancer_ip" {
  value = hcloud_load_balancer.chronik.ipv4
  description = "Public IP of the load balancer"
}

output "pd_servers" {
  value = [for s in hcloud_server.pd : s.ipv4_address]
  description = "Public IPs of PD servers"
}

output "tikv_servers" {
  value = [for s in hcloud_server.tikv : s.ipv4_address]
  description = "Public IPs of TiKV servers"
}

output "ingest_servers" {
  value = [for s in hcloud_server.ingest : s.ipv4_address]
  description = "Public IPs of Ingest servers"
}