# Building Your First Chronik Stream Application

This tutorial will guide you through building a real-time log analysis application that demonstrates Chronik Stream's unique combination of streaming and search capabilities.

## What We'll Build

A log monitoring system that:
- Ingests application logs in real-time
- Automatically indexes logs for instant search
- Provides real-time alerts for errors
- Offers a search interface for historical analysis

## Prerequisites

- Chronik Stream running (see [Quick Start](quick-start.md))
- Python 3.8+ installed
- Basic knowledge of Kafka concepts

## Step 1: Set Up the Project

Create a new directory and install dependencies:

```bash
mkdir chronik-log-monitor
cd chronik-log-monitor

# Create virtual environment
python -m venv venv
source venv/bin/activate  # On Windows: venv\Scripts\activate

# Install dependencies
pip install kafka-python requests flask
```

## Step 2: Create the Log Producer

Create `log_producer.py` to simulate application logs:

```python
#!/usr/bin/env python3
"""
Log Producer - Simulates application logs
"""
import json
import random
import time
import uuid
from datetime import datetime
from kafka import KafkaProducer

# Log templates
LOG_LEVELS = ['DEBUG', 'INFO', 'WARN', 'ERROR', 'FATAL']
SERVICES = ['auth-service', 'api-gateway', 'user-service', 'payment-service']
MESSAGES = {
    'DEBUG': ['Starting request processing', 'Cache hit for key: {}', 'Database query executed'],
    'INFO': ['Request completed successfully', 'User {} logged in', 'Payment processed'],
    'WARN': ['High memory usage detected', 'Slow query warning', 'Rate limit approaching'],
    'ERROR': ['Database connection failed', 'Authentication error', 'Payment gateway timeout'],
    'FATAL': ['Service crashed', 'Out of memory', 'Disk full']
}

class LogProducer:
    def __init__(self, bootstrap_servers='localhost:9092'):
        self.producer = KafkaProducer(
            bootstrap_servers=bootstrap_servers,
            value_serializer=lambda v: json.dumps(v).encode('utf-8'),
            compression_type='snappy'
        )
        
    def generate_log(self):
        """Generate a random log entry"""
        level = random.choice(LOG_LEVELS)
        service = random.choice(SERVICES)
        message_template = random.choice(MESSAGES[level])
        
        # Format message with random data
        if '{}' in message_template:
            message = message_template.format(f'user_{random.randint(1000, 9999)}')
        else:
            message = message_template
            
        log_entry = {
            'id': str(uuid.uuid4()),
            'timestamp': datetime.utcnow().isoformat() + 'Z',
            'level': level,
            'service': service,
            'message': message,
            'metadata': {
                'host': f'server-{random.randint(1, 10)}',
                'region': random.choice(['us-east', 'us-west', 'eu-central']),
                'version': f'v1.{random.randint(0, 5)}.{random.randint(0, 20)}'
            }
        }
        
        # Add stack trace for errors
        if level in ['ERROR', 'FATAL']:
            log_entry['stack_trace'] = self.generate_stack_trace()
            
        return log_entry
    
    def generate_stack_trace(self):
        """Generate a fake stack trace"""
        return """Exception in thread "main" java.lang.NullPointerException
    at com.example.service.UserService.getUser(UserService.java:45)
    at com.example.api.UserController.handleRequest(UserController.java:28)
    at com.example.framework.RequestHandler.process(RequestHandler.java:102)"""
    
    def start_producing(self, topic='application-logs', rate=10):
        """Start producing logs at specified rate (logs per second)"""
        print(f"Starting log producer - {rate} logs/second")
        
        try:
            while True:
                log = self.generate_log()
                self.producer.send(topic, value=log)
                
                # Print sample logs
                if random.random() < 0.1:  # Print 10% of logs
                    print(f"[{log['level']}] {log['service']}: {log['message']}")
                
                time.sleep(1.0 / rate)
                
        except KeyboardInterrupt:
            print("\nShutting down producer...")
        finally:
            self.producer.flush()
            self.producer.close()

if __name__ == '__main__':
    # Create topic first
    from kafka.admin import KafkaAdminClient, NewTopic
    
    admin = KafkaAdminClient(bootstrap_servers='localhost:9092')
    try:
        topic = NewTopic(name='application-logs', num_partitions=6, replication_factor=1)
        admin.create_topics([topic])
        print("Created topic: application-logs")
    except Exception as e:
        print(f"Topic might already exist: {e}")
    
    # Start producing
    producer = LogProducer()
    producer.start_producing(rate=20)  # 20 logs per second
```

## Step 3: Create the Error Monitor

Create `error_monitor.py` to consume logs and detect errors:

```python
#!/usr/bin/env python3
"""
Error Monitor - Monitors logs for errors and sends alerts
"""
import json
from collections import defaultdict
from datetime import datetime, timedelta
from kafka import KafkaConsumer

class ErrorMonitor:
    def __init__(self, bootstrap_servers='localhost:9092'):
        self.consumer = KafkaConsumer(
            'application-logs',
            bootstrap_servers=bootstrap_servers,
            auto_offset_reset='latest',
            value_deserializer=lambda m: json.loads(m.decode('utf-8')),
            group_id='error-monitor-group'
        )
        self.error_counts = defaultdict(lambda: defaultdict(int))
        self.alert_threshold = 5  # Alert after 5 errors per service per minute
        
    def process_log(self, log):
        """Process a log entry and check for alerts"""
        level = log['level']
        service = log['service']
        
        if level in ['ERROR', 'FATAL']:
            # Track error count
            current_minute = datetime.utcnow().replace(second=0, microsecond=0)
            self.error_counts[service][current_minute] += 1
            
            # Check if we should alert
            if self.error_counts[service][current_minute] >= self.alert_threshold:
                self.send_alert(service, log)
                
            # Clean up old entries
            self.cleanup_old_counts()
            
    def send_alert(self, service, log):
        """Send an alert for high error rate"""
        print(f"\n🚨 ALERT: High error rate detected!")
        print(f"Service: {service}")
        print(f"Latest error: {log['message']}")
        print(f"Time: {log['timestamp']}")
        if 'stack_trace' in log:
            print(f"Stack trace:\n{log['stack_trace'][:200]}...")
        print("-" * 50)
        
    def cleanup_old_counts(self):
        """Remove error counts older than 5 minutes"""
        cutoff = datetime.utcnow() - timedelta(minutes=5)
        for service in list(self.error_counts.keys()):
            old_times = [t for t in self.error_counts[service].keys() if t < cutoff]
            for t in old_times:
                del self.error_counts[service][t]
                
    def start_monitoring(self):
        """Start monitoring logs"""
        print("Error monitor started - watching for errors...")
        
        try:
            for message in self.consumer:
                log = message.value
                self.process_log(log)
                
                # Print error summary every 100 messages
                if message.offset % 100 == 0:
                    self.print_summary()
                    
        except KeyboardInterrupt:
            print("\nShutting down monitor...")
        finally:
            self.consumer.close()
            
    def print_summary(self):
        """Print current error summary"""
        total_errors = sum(
            sum(counts.values()) 
            for counts in self.error_counts.values()
        )
        if total_errors > 0:
            print(f"\nError Summary - Total: {total_errors}")
            for service, counts in self.error_counts.items():
                service_total = sum(counts.values())
                if service_total > 0:
                    print(f"  {service}: {service_total} errors")

if __name__ == '__main__':
    monitor = ErrorMonitor()
    monitor.start_monitoring()
```

## Step 4: Create the Search Interface

Create `search_app.py` to search and analyze logs:

```python
#!/usr/bin/env python3
"""
Search Application - Web interface for searching logs
"""
import json
import requests
from flask import Flask, render_template_string, request, jsonify
from datetime import datetime, timedelta

app = Flask(__name__)

# HTML template for the search interface
HTML_TEMPLATE = '''
<!DOCTYPE html>
<html>
<head>
    <title>Chronik Log Search</title>
    <style>
        body { font-family: Arial, sans-serif; margin: 20px; }
        .search-box { margin-bottom: 20px; }
        input, select { padding: 8px; margin: 5px; }
        button { padding: 10px 20px; background: #007bff; color: white; border: none; cursor: pointer; }
        button:hover { background: #0056b3; }
        .results { margin-top: 20px; }
        .log-entry { border: 1px solid #ddd; padding: 10px; margin: 5px 0; border-radius: 5px; }
        .log-entry.ERROR { border-color: #dc3545; background: #f8d7da; }
        .log-entry.WARN { border-color: #ffc107; background: #fff3cd; }
        .log-entry.FATAL { border-color: #721c24; background: #f5c6cb; }
        .metadata { color: #6c757d; font-size: 0.9em; }
        .message { font-weight: bold; margin: 5px 0; }
        .stats { background: #f8f9fa; padding: 15px; margin: 20px 0; border-radius: 5px; }
    </style>
</head>
<body>
    <h1>Chronik Log Search</h1>
    
    <div class="search-box">
        <input type="text" id="query" placeholder="Search logs..." size="50">
        <select id="level">
            <option value="">All Levels</option>
            <option value="DEBUG">DEBUG</option>
            <option value="INFO">INFO</option>
            <option value="WARN">WARN</option>
            <option value="ERROR">ERROR</option>
            <option value="FATAL">FATAL</option>
        </select>
        <select id="service">
            <option value="">All Services</option>
            <option value="auth-service">Auth Service</option>
            <option value="api-gateway">API Gateway</option>
            <option value="user-service">User Service</option>
            <option value="payment-service">Payment Service</option>
        </select>
        <button onclick="searchLogs()">Search</button>
        <button onclick="getStats()">Statistics</button>
    </div>
    
    <div id="stats" class="stats" style="display:none;"></div>
    <div id="results" class="results"></div>
    
    <script>
        async function searchLogs() {
            const query = document.getElementById('query').value;
            const level = document.getElementById('level').value;
            const service = document.getElementById('service').value;
            
            const params = new URLSearchParams();
            if (query) params.append('q', query);
            if (level) params.append('level', level);
            if (service) params.append('service', service);
            
            const response = await fetch('/api/search?' + params);
            const data = await response.json();
            
            displayResults(data.results);
        }
        
        async function getStats() {
            const response = await fetch('/api/stats');
            const data = await response.json();
            
            const statsDiv = document.getElementById('stats');
            statsDiv.style.display = 'block';
            statsDiv.innerHTML = `
                <h3>Log Statistics (Last Hour)</h3>
                <p>Total Logs: ${data.total}</p>
                <p>Error Rate: ${data.error_rate.toFixed(2)}%</p>
                <h4>By Level:</h4>
                ${Object.entries(data.by_level).map(([level, count]) => 
                    `<p>${level}: ${count}</p>`
                ).join('')}
                <h4>By Service:</h4>
                ${Object.entries(data.by_service).map(([service, count]) => 
                    `<p>${service}: ${count}</p>`
                ).join('')}
            `;
        }
        
        function displayResults(results) {
            const resultsDiv = document.getElementById('results');
            
            if (results.length === 0) {
                resultsDiv.innerHTML = '<p>No results found.</p>';
                return;
            }
            
            resultsDiv.innerHTML = '<h3>Search Results (' + results.length + ')</h3>' +
                results.map(log => `
                    <div class="log-entry ${log.level}">
                        <div class="metadata">
                            ${new Date(log.timestamp).toLocaleString()} | 
                            ${log.level} | 
                            ${log.service} | 
                            ${log.metadata.host}
                        </div>
                        <div class="message">${log.message}</div>
                        ${log.stack_trace ? '<pre>' + log.stack_trace + '</pre>' : ''}
                    </div>
                `).join('');
        }
        
        // Auto-refresh for real-time updates
        setInterval(() => {
            if (document.getElementById('query').value === '') {
                searchLogs();
            }
        }, 5000);
    </script>
</body>
</html>
'''

class LogSearcher:
    def __init__(self, chronik_api='http://localhost:8080'):
        self.api_base = chronik_api
        
    def search(self, query=None, level=None, service=None, limit=100):
        """Search logs using Chronik Stream's search API"""
        # Build search query
        search_query = {"bool": {"must": []}}
        
        if query:
            search_query["bool"]["must"].append({
                "multi_match": {
                    "query": query,
                    "fields": ["message", "stack_trace"]
                }
            })
            
        if level:
            search_query["bool"]["must"].append({
                "term": {"level": level}
            })
            
        if service:
            search_query["bool"]["must"].append({
                "term": {"service": service}
            })
            
        # Add time range (last hour)
        search_query["bool"]["must"].append({
            "range": {
                "timestamp": {
                    "gte": (datetime.utcnow() - timedelta(hours=1)).isoformat() + 'Z'
                }
            }
        })
        
        # Send search request
        response = requests.post(
            f"{self.api_base}/api/v1/search",
            json={
                "topic": "application-logs",
                "query": search_query,
                "limit": limit,
                "sort": [{"timestamp": "desc"}]
            }
        )
        
        if response.status_code == 200:
            return response.json().get('hits', [])
        else:
            print(f"Search error: {response.status_code} - {response.text}")
            return []
            
    def get_statistics(self):
        """Get log statistics for the last hour"""
        # Search for all logs in the last hour
        all_logs = self.search(limit=10000)
        
        stats = {
            'total': len(all_logs),
            'by_level': defaultdict(int),
            'by_service': defaultdict(int),
            'error_rate': 0
        }
        
        error_count = 0
        for log in all_logs:
            stats['by_level'][log['level']] += 1
            stats['by_service'][log['service']] += 1
            if log['level'] in ['ERROR', 'FATAL']:
                error_count += 1
                
        if stats['total'] > 0:
            stats['error_rate'] = (error_count / stats['total']) * 100
            
        return stats

# Initialize searcher
searcher = LogSearcher()

@app.route('/')
def index():
    return render_template_string(HTML_TEMPLATE)

@app.route('/api/search')
def search():
    query = request.args.get('q')
    level = request.args.get('level')
    service = request.args.get('service')
    
    results = searcher.search(query, level, service)
    return jsonify({'results': results})

@app.route('/api/stats')
def stats():
    statistics = searcher.get_statistics()
    return jsonify(statistics)

if __name__ == '__main__':
    print("Starting search application on http://localhost:5000")
    app.run(debug=True)
```

## Step 5: Run the Application

1. **Start the log producer** (Terminal 1):
```bash
python log_producer.py
```

2. **Start the error monitor** (Terminal 2):
```bash
python error_monitor.py
```

3. **Start the search interface** (Terminal 3):
```bash
python search_app.py
```

4. **Open the search interface**:
   - Navigate to http://localhost:5000
   - Try searching for "error", "payment", or specific services
   - Click "Statistics" to see real-time metrics

## Step 6: Experiment and Extend

### Try These Experiments

1. **Search for specific errors**:
   - Search: "database connection"
   - Filter by level: ERROR
   - Watch real-time updates

2. **Monitor error patterns**:
   - Let the system run for a few minutes
   - Watch the error monitor for alerts
   - Check statistics in the web interface

3. **Performance testing**:
   - Increase the log rate in `log_producer.py`
   - Monitor system performance
   - Test search response times

### Extension Ideas

1. **Add anomaly detection**:
```python
# In error_monitor.py
def detect_anomaly(self, service, error_rate):
    # Simple anomaly detection based on historical average
    historical_avg = self.get_historical_average(service)
    if error_rate > historical_avg * 2:
        self.send_anomaly_alert(service, error_rate)
```

2. **Create dashboards**:
   - Use Grafana with Chronik Stream's metrics API
   - Build real-time charts with WebSockets
   - Add geographic visualization

3. **Implement log correlation**:
```python
# Find related logs across services
def correlate_logs(self, trace_id):
    return searcher.search(query=f"trace_id:{trace_id}")
```

## Key Takeaways

You've built an application that demonstrates Chronik Stream's core capabilities:

1. **Kafka Compatibility**: Used standard Kafka clients for producing and consuming
2. **Real-time Search**: Instantly searchable logs without external tools
3. **Stream Processing**: Real-time error detection and alerting
4. **Unified Platform**: No need for separate streaming and search systems

## Next Steps

- Explore [Advanced Search Features](../api-reference/search-api.md#advanced-queries)
- Learn about [Performance Optimization](../deployment/performance-tuning.md)
- Check out more [Example Applications](../examples/index.md)
- Deploy to [Production](../deployment/production-best-practices.md)

## Complete Code

All code from this tutorial is available at:
https://github.com/chronik-stream/examples/tree/main/log-monitor