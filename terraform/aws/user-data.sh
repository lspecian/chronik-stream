#!/bin/bash
set -e

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

# Mount data volume if attached
if [ -e /dev/sdf ]; then
    mkfs -t ext4 /dev/sdf || true
    mount /dev/sdf /data
    echo "/dev/sdf /data ext4 defaults,nofail 0 2" >> /etc/fstab
fi

# Mount instance store if configured
if [ "${use_instance_store}" = "true" ] && [ -e /dev/nvme1n1 ]; then
    mkfs -t ext4 /dev/nvme1n1 || true
    mkdir -p /data/cache
    mount /dev/nvme1n1 /data/cache
    echo "/dev/nvme1n1 /data/cache ext4 defaults,nofail 0 2" >> /etc/fstab
fi

# Pull Chronik Stream image
docker pull ${chronik_image}:${chronik_version} || true

# Create systemd service
cat > /etc/systemd/system/chronik.service <<'EOF'
[Unit]
Description=Chronik Stream All-in-One
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
  -e RUST_LOG=${rust_log} \
  -e STORAGE_BACKEND=${storage_backend} \
  -e S3_BUCKET=${s3_bucket} \
  -e S3_REGION=${s3_region} \
  -e AWS_DEFAULT_REGION=${s3_region} \
  ${chronik_image}:${chronik_version}

[Install]
WantedBy=multi-user.target
EOF

# Enable and start service
systemctl daemon-reload
systemctl enable chronik
systemctl start chronik

# Install CloudWatch agent
dnf install -y amazon-cloudwatch-agent
/opt/aws/amazon-cloudwatch-agent/bin/amazon-cloudwatch-agent-ctl \
    -a fetch-config \
    -m ec2 \
    -s -c ssm:AmazonCloudWatch-DefaultConfig

echo "Chronik Stream deployment complete!"