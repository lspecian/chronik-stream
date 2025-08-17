# Chronik Stream - Terraform Configurations

> ⚠️ **EXPERIMENTAL INFRASTRUCTURE** ⚠️
> 
> These Terraform configurations deploy experimental software.
> Do NOT use for production workloads. See main README for disclaimers.

## Overview

This directory contains Terraform configurations for deploying Chronik Stream to various cloud providers:

- **AWS** - Amazon Web Services (EC2, VPC, S3, ALB/NLB)
- **Hetzner** - Hetzner Cloud (European cloud provider)

## Quick Start

### Hetzner Cloud

```bash
cd terraform/hetzner

# Copy and edit the example configuration
cp terraform.tfvars.example terraform.tfvars
# Edit terraform.tfvars with your Hetzner Cloud token

# Initialize Terraform
terraform init

# Plan the deployment
terraform plan

# Deploy (single all-in-one instance)
terraform apply

# Destroy when done
terraform destroy
```

### AWS

```bash
cd terraform/aws

# Configure AWS credentials
aws configure

# Copy and edit the example configuration
cp terraform.tfvars.example terraform.tfvars
# Edit terraform.tfvars with your preferences

# Initialize Terraform
terraform init

# Plan the deployment
terraform plan

# Deploy
terraform apply

# Destroy when done
terraform destroy
```

## Deployment Types

### Single Node (All-in-One)

The simplest deployment with all services running in a single instance:

```hcl
deployment_type = "single"
```

**Hetzner**: Creates one server with attached volume
**AWS**: Creates one EC2 instance with optional EBS volume

Cost: ~€8-15/month (Hetzner) or ~$30-50/month (AWS)

### Cluster Mode

Multi-node deployment for better scalability and reliability:

```hcl
deployment_type = "cluster"
```

Components:
- **Controller nodes** (3 by default) - Metadata and coordination
- **Ingest nodes** (2+ nodes) - Kafka protocol handling
- **Search nodes** (optional) - Full-text search
- **Load balancer** - Traffic distribution

Cost: ~€50-100/month (Hetzner) or ~$150-300/month (AWS)

## Configuration Options

### Essential Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `deployment_type` | "single" or "cluster" | "single" |
| `project_name` | Name prefix for resources | "chronik-stream" |
| `data_volume_size` | Storage volume size in GB | 100 |
| `storage_backend` | "embedded" or "s3" | "embedded" |

### Server Sizing

#### Hetzner Server Types
- `cpx11` - 2 vCPU, 2GB RAM (~€4/month)
- `cpx21` - 3 vCPU, 4GB RAM (~€8/month)
- `cpx31` - 4 vCPU, 8GB RAM (~€15/month)
- `cpx41` - 8 vCPU, 16GB RAM (~€30/month)

#### AWS Instance Types
- `t3.micro` - 2 vCPU, 1GB RAM (~$7/month)
- `t3.small` - 2 vCPU, 2GB RAM (~$15/month)
- `t3.medium` - 2 vCPU, 4GB RAM (~$30/month)
- `t3.large` - 2 vCPU, 8GB RAM (~$60/month)

### Storage Backends

#### Embedded Storage
Local disk storage, suitable for development:
```hcl
storage_backend = "embedded"
data_volume_size = 100  # GB
```

#### S3 Storage
External object storage for production:
```hcl
storage_backend = "s3"
create_s3_bucket = true  # AWS only
s3_bucket = "my-chronik-bucket"
```

### Security Configuration

⚠️ **IMPORTANT**: Default configurations allow access from anywhere (0.0.0.0/0).
Restrict access in production:

```hcl
# Hetzner
ssh_allowed_ips = ["YOUR_IP/32"]
kafka_allowed_ips = ["YOUR_NETWORK/24"]

# AWS
ssh_allowed_cidrs = ["YOUR_IP/32"]
kafka_allowed_cidrs = ["YOUR_VPC_CIDR"]
```

## Outputs

After deployment, Terraform will output:

- **Connection endpoints** - Kafka, Admin API, Metrics
- **SSH commands** - Access to instances
- **Test commands** - Verify deployment
- **Load balancer URLs** - For cluster deployments

Example:
```bash
kafka_endpoint = "168.119.116.228:9092"
admin_endpoint = "http://168.119.116.228:3000"
ssh_command = "ssh -i chronik.pem root@168.119.116.228"
```

## Testing the Deployment

After deployment, test with:

```bash
# Test Kafka connectivity
kafkactl --brokers <kafka_endpoint> get brokers

# Check health
curl <admin_endpoint>/health

# View metrics
curl <admin_endpoint>:9090/metrics

# SSH to instance
terraform output -raw ssh_private_key > chronik.pem
chmod 600 chronik.pem
ssh -i chronik.pem root@<instance_ip>  # Hetzner
ssh -i chronik.pem ec2-user@<instance_ip>  # AWS
```

## Cost Optimization

### Development/Testing
- Use smallest instance types
- Deploy in cheapest regions (Hetzner: fsn1, AWS: us-east-1)
- Use spot instances (AWS)
- Destroy resources when not in use

### Production (when ready)
- Use reserved instances (AWS) for predictable workloads
- Enable auto-scaling for dynamic load
- Use S3 for storage instead of EBS
- Consider ARM instances for better price/performance

## Monitoring

### CloudWatch (AWS)
Automatically enabled with `enable_monitoring = true`

### Prometheus + Grafana
Deploy separately and scrape metrics endpoints:
```
http://<instance>:9090/metrics
```

## Backup and Recovery

⚠️ **No automated backups configured** - This is experimental software

For data protection:
1. Use S3 storage backend with versioning
2. Take EBS/Volume snapshots manually
3. Export data regularly using Admin API

## Troubleshooting

### Common Issues

1. **Deployment fails with "insufficient capacity"**
   - Try different availability zone or instance type

2. **Cannot connect to Kafka**
   - Check security group/firewall rules
   - Verify instance is running: `terraform show`

3. **High costs**
   - Review instance types and counts
   - Check for orphaned resources
   - Use `terraform destroy` when not needed

### Logs

View application logs:
```bash
# SSH to instance, then:
docker logs chronik

# Or use cloud provider tools:
# AWS: CloudWatch Logs
# Hetzner: SSH only
```

## Clean Up

**IMPORTANT**: Always destroy resources when done testing:

```bash
terraform destroy -auto-approve
```

Verify all resources are deleted:
- Check cloud provider console
- Review billing/usage reports

## Support

This is experimental software. For issues:
- GitHub Issues: https://github.com/lspecian/chronik-stream/issues
- Documentation: See main README.md

## License

MIT - See LICENSE file in repository root