FROM python:3.9-slim

WORKDIR /app

# Install kafka-python
RUN pip install --no-cache-dir kafka-python==2.0.2

# Copy test script
COPY test.py /app/test.py
RUN chmod +x /app/test.py

# Create results directory
RUN mkdir -p /results

# Set environment variables
ENV BOOTSTRAP_SERVERS=chronik:9092

CMD ["python", "/app/test.py"]