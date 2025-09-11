FROM python:3.9-slim

WORKDIR /app

# Install build dependencies for librdkafka
RUN apt-get update && apt-get install -y \
    gcc \
    g++ \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Install confluent-kafka
RUN pip install --no-cache-dir confluent-kafka==2.5.0

# Copy test script
COPY test.py /app/test.py
RUN chmod +x /app/test.py

# Create results directory
RUN mkdir -p /results

# Set environment variables
ENV BOOTSTRAP_SERVERS=chronik:9092
ENV PYTHONUNBUFFERED=1

CMD ["python", "/app/test.py"]