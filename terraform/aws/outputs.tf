output "deployment_type" {
  description = "Type of deployment (single or cluster)"
  value       = var.deployment_type
}

# VPC outputs
output "vpc_id" {
  description = "ID of the VPC"
  value       = aws_vpc.chronik.id
}

output "public_subnet_ids" {
  description = "IDs of public subnets"
  value       = aws_subnet.public[*].id
}

output "private_subnet_ids" {
  description = "IDs of private subnets"
  value       = aws_subnet.private[*].id
}

# Single deployment outputs
output "all_in_one_instance_id" {
  description = "Instance ID of the all-in-one server"
  value       = var.deployment_type == "single" ? aws_instance.chronik_all_in_one[0].id : null
}

output "all_in_one_public_ip" {
  description = "Public IP of the all-in-one server"
  value       = var.deployment_type == "single" ? aws_instance.chronik_all_in_one[0].public_ip : null
}

output "all_in_one_private_ip" {
  description = "Private IP of the all-in-one server"
  value       = var.deployment_type == "single" ? aws_instance.chronik_all_in_one[0].private_ip : null
}

# Cluster deployment outputs
output "controller_asg_name" {
  description = "Name of the controller Auto Scaling Group"
  value       = var.deployment_type == "cluster" ? aws_autoscaling_group.controller[0].name : null
}

output "alb_dns_name" {
  description = "DNS name of the Application Load Balancer"
  value       = var.deployment_type == "cluster" && var.enable_load_balancer ? aws_lb.chronik[0].dns_name : null
}

output "nlb_dns_name" {
  description = "DNS name of the Network Load Balancer (Kafka)"
  value       = var.deployment_type == "cluster" && var.enable_load_balancer ? aws_lb.kafka[0].dns_name : null
}

# Connection endpoints
output "kafka_endpoint" {
  description = "Kafka API endpoint"
  value = var.deployment_type == "single" ? 
    "${aws_instance.chronik_all_in_one[0].public_ip}:9092" :
    var.enable_load_balancer ? 
      "${aws_lb.kafka[0].dns_name}:9092" :
      "Use Auto Scaling Group instances"
}

output "admin_endpoint" {
  description = "Admin API endpoint"
  value = var.deployment_type == "single" ? 
    "http://${aws_instance.chronik_all_in_one[0].public_ip}:3000" :
    var.enable_load_balancer ?
      "http://${aws_lb.chronik[0].dns_name}:3000" :
      "Use Auto Scaling Group instances"
}

output "metrics_endpoint" {
  description = "Prometheus metrics endpoint"
  value = var.deployment_type == "single" ?
    "http://${aws_instance.chronik_all_in_one[0].public_ip}:9090/metrics" :
    "Use individual instance metrics"
}

# S3 bucket output
output "s3_bucket_name" {
  description = "Name of the S3 bucket for storage"
  value       = var.create_s3_bucket ? aws_s3_bucket.chronik[0].id : null
}

output "s3_bucket_arn" {
  description = "ARN of the S3 bucket"
  value       = var.create_s3_bucket ? aws_s3_bucket.chronik[0].arn : null
}

# SSH access
output "ssh_private_key" {
  description = "SSH private key for accessing instances"
  value       = tls_private_key.chronik.private_key_pem
  sensitive   = true
}

output "ssh_command" {
  description = "SSH command to access the instance"
  value = var.deployment_type == "single" ?
    "ssh -i chronik.pem ec2-user@${aws_instance.chronik_all_in_one[0].public_ip}" :
    "Use Session Manager or bastion host for cluster instances"
}

# Test commands
output "test_commands" {
  description = "Commands to test the deployment"
  value = {
    kafka_test = var.deployment_type == "single" ?
      "kafkactl --brokers ${aws_instance.chronik_all_in_one[0].public_ip}:9092 get brokers" :
      var.enable_load_balancer ?
        "kafkactl --brokers ${aws_lb.kafka[0].dns_name}:9092 get brokers" :
        "Configure kafkactl with Auto Scaling Group instances"
    
    health_check = var.deployment_type == "single" ?
      "curl http://${aws_instance.chronik_all_in_one[0].public_ip}:3000/health" :
      var.enable_load_balancer ?
        "curl http://${aws_lb.chronik[0].dns_name}:3000/health" :
        "Use individual instance health checks"
    
    metrics_check = var.deployment_type == "single" ?
      "curl http://${aws_instance.chronik_all_in_one[0].public_ip}:9090/metrics" :
      "Use individual instance metrics"
  }
}

# CloudWatch dashboard URL (if monitoring enabled)
output "cloudwatch_dashboard_url" {
  description = "URL to CloudWatch dashboard"
  value = var.enable_monitoring ?
    "https://console.aws.amazon.com/cloudwatch/home?region=${var.aws_region}#dashboards:name=${var.project_name}" :
    null
}