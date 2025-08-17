output "deployment_type" {
  description = "Type of deployment (single or cluster)"
  value       = var.deployment_type
}

# Single deployment outputs
output "all_in_one_ip" {
  description = "Public IP of the all-in-one server"
  value       = var.deployment_type == "single" ? hcloud_server.chronik_all_in_one[0].ipv4_address : null
}

output "all_in_one_id" {
  description = "Server ID of the all-in-one server"
  value       = var.deployment_type == "single" ? hcloud_server.chronik_all_in_one[0].id : null
}

# Cluster deployment outputs
output "controller_ips" {
  description = "Public IPs of controller nodes"
  value       = var.deployment_type == "cluster" ? [for s in hcloud_server.chronik_controller : s.ipv4_address] : []
}

output "ingest_ips" {
  description = "Public IPs of ingest nodes"
  value       = var.deployment_type == "cluster" ? [for s in hcloud_server.chronik_ingest : s.ipv4_address] : []
}

output "search_ips" {
  description = "Public IPs of search nodes"
  value       = var.deployment_type == "cluster" && var.enable_search ? [for s in hcloud_server.chronik_search : s.ipv4_address] : []
}

output "load_balancer_ip" {
  description = "Public IP of the load balancer"
  value       = var.deployment_type == "cluster" && var.enable_load_balancer ? hcloud_load_balancer.chronik[0].ipv4 : null
}

# Connection details
output "kafka_endpoint" {
  description = "Kafka API endpoint"
  value = var.deployment_type == "single" ? 
    "${hcloud_server.chronik_all_in_one[0].ipv4_address}:9092" :
    var.enable_load_balancer ? 
      "${hcloud_load_balancer.chronik[0].ipv4}:9092" :
      join(",", [for s in hcloud_server.chronik_ingest : "${s.ipv4_address}:9092"])
}

output "admin_endpoint" {
  description = "Admin API endpoint"
  value = var.deployment_type == "single" ? 
    "http://${hcloud_server.chronik_all_in_one[0].ipv4_address}:3000" :
    var.enable_load_balancer ?
      "http://${hcloud_load_balancer.chronik[0].ipv4}:3000" :
      "http://${hcloud_server.chronik_controller[0].ipv4_address}:3000"
}

output "metrics_endpoints" {
  description = "Prometheus metrics endpoints"
  value = var.deployment_type == "single" ?
    ["http://${hcloud_server.chronik_all_in_one[0].ipv4_address}:9090/metrics"] :
    concat(
      [for s in hcloud_server.chronik_controller : "http://${s.ipv4_address}:9090/metrics"],
      [for s in hcloud_server.chronik_ingest : "http://${s.ipv4_address}:9090/metrics"],
      var.enable_search ? [for s in hcloud_server.chronik_search : "http://${s.ipv4_address}:9090/metrics"] : []
    )
}

# SSH access
output "ssh_private_key" {
  description = "SSH private key for accessing servers"
  value       = tls_private_key.chronik.private_key_pem
  sensitive   = true
}

output "ssh_commands" {
  description = "SSH commands to access servers"
  value = var.deployment_type == "single" ? {
    all_in_one = "ssh -i chronik.pem root@${hcloud_server.chronik_all_in_one[0].ipv4_address}"
  } : {
    controllers = [for i, s in hcloud_server.chronik_controller : "ssh -i chronik.pem root@${s.ipv4_address}"]
    ingest_nodes = [for i, s in hcloud_server.chronik_ingest : "ssh -i chronik.pem root@${s.ipv4_address}"]
    search_nodes = var.enable_search ? [for i, s in hcloud_server.chronik_search : "ssh -i chronik.pem root@${s.ipv4_address}"] : []
  }
}

# Test commands
output "test_commands" {
  description = "Commands to test the deployment"
  value = {
    kafka_test = "kafkactl --brokers ${var.deployment_type == "single" ? hcloud_server.chronik_all_in_one[0].ipv4_address : var.enable_load_balancer ? hcloud_load_balancer.chronik[0].ipv4 : hcloud_server.chronik_ingest[0].ipv4_address}:9092 get brokers"
    health_check = "curl ${var.deployment_type == "single" ? "http://${hcloud_server.chronik_all_in_one[0].ipv4_address}:3000" : var.enable_load_balancer ? "http://${hcloud_load_balancer.chronik[0].ipv4}:3000" : "http://${hcloud_server.chronik_controller[0].ipv4_address}:3000"}/health"
    metrics_check = "curl ${var.deployment_type == "single" ? "http://${hcloud_server.chronik_all_in_one[0].ipv4_address}:9090" : "http://${hcloud_server.chronik_controller[0].ipv4_address}:9090"}/metrics"
  }
}