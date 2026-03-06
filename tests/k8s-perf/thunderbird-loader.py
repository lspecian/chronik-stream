#!/usr/bin/env python3
"""
Thunderbird Log Loader for Chronik Stream SV-2a Scale Validation.

Parses Loghub-2.0 Thunderbird supercomputer logs (16.6M lines) and produces
structured JSON messages to a Chronik topic via the Kafka protocol.

Usage:
  # Local (against local Chronik)
  python3 thunderbird-loader.py

  # Custom settings
  BOOTSTRAP_SERVERS=chronik:9092 TOPIC=thunderbird MAX_LINES=100000 python3 thunderbird-loader.py

  # In k8s Job (see 92-thunderbird-loader-job.yaml)

Environment Variables:
  BOOTSTRAP_SERVERS  Kafka bootstrap (default: localhost:9092)
  TOPIC              Target topic (default: thunderbird)
  LOG_FILE           Path to Thunderbird_full.log (default: datasets/thunderbird/...)
  MAX_LINES          0 = all 16.6M lines (default: 0)
  BATCH_SIZE         Flush every N messages (default: 5000)
  NUM_PARTITIONS     Partitions for auto-created topic (default: 6)
"""

import json
import os
import re
import sys
import time

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

BOOTSTRAP = os.environ.get('BOOTSTRAP_SERVERS', 'localhost:9092')
TOPIC = os.environ.get('TOPIC', 'thunderbird')
LOG_FILE = os.environ.get(
    'LOG_FILE',
    os.path.join(os.path.dirname(__file__), '..', '..', 'datasets',
                 'thunderbird', 'Thunderbird', 'Thunderbird_full.log')
)
MAX_LINES = int(os.environ.get('MAX_LINES', '0'))
BATCH_SIZE = int(os.environ.get('BATCH_SIZE', '5000'))
NUM_PARTITIONS = int(os.environ.get('NUM_PARTITIONS', '6'))

# ---------------------------------------------------------------------------
# Thunderbird log line parser
# ---------------------------------------------------------------------------
# Format: Label UnixTimestamp YYYY.MM.DD Hostname Mon DD HH:MM:SS Source Rest
# Examples:
#   - 1131523501 2005.11.09 aadmin1 Nov 10 00:05:01 src@aadmin1 in.tftpd[14620]: tftp: client does not accept options
#   ECC 1131674844 2005.11.10 cn994 Nov 10 18:07:24 cn994/cn994 Server Administrator: ...
#   CPU 1131685767 2005.11.10 bn257 Nov 10 21:09:27 bn257/bn257 kernel: Losing some ticks...

LINE_PATTERN = re.compile(
    r'^(\S+)\s+'           # 1: label (-, ECC, CPU, etc.)
    r'(\d+)\s+'            # 2: unix timestamp
    r'(\S+)\s+'            # 3: date YYYY.MM.DD
    r'(\S+)\s+'            # 4: hostname
    r'(\w+)\s+(\d+)\s+'   # 5,6: month, day
    r'([\d:]+)\s+'         # 7: time HH:MM:SS
    r'(\S+)\s+'            # 8: source (e.g., src@host or host/host)
    r'(.+)'                # 9: rest — component[pid]: message
)


def parse_line(line):
    """Parse a single Thunderbird log line into a structured dict."""
    m = LINE_PATTERN.match(line)
    if not m:
        return None

    rest = m.group(9)

    # Split component from message at first ': '
    colon_idx = rest.find(': ')
    if colon_idx > 0:
        component_raw = rest[:colon_idx].strip()
        message = rest[colon_idx + 2:].strip()
    else:
        component_raw = rest.strip()
        message = ''

    # Extract clean component name (strip [pid] suffix)
    bracket_idx = component_raw.find('[')
    if bracket_idx > 0:
        component = component_raw[:bracket_idx]
    else:
        component = component_raw

    return {
        'label': m.group(1),
        'timestamp': int(m.group(2)),
        'date': m.group(3),
        'hostname': m.group(4),
        'time': f'{m.group(5)} {m.group(6)} {m.group(7)}',
        'source': m.group(8),
        'component': component,
        'message': message,
    }


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    # Install kafka-python if needed (for k8s Job where it's not pre-installed)
    try:
        from kafka import KafkaProducer
    except ImportError:
        import subprocess
        print('Installing kafka-python...')
        subprocess.check_call([sys.executable, '-m', 'pip', 'install', 'kafka-python', '-q'])
        from kafka import KafkaProducer

    # Resolve log file path
    log_path = os.path.abspath(LOG_FILE)
    if not os.path.exists(log_path):
        print(f'ERROR: Log file not found: {log_path}')
        print(f'  Set LOG_FILE env var or ensure the dataset is at the expected location.')
        sys.exit(1)

    file_size_mb = os.path.getsize(log_path) / (1024 * 1024)
    limit_str = f'{MAX_LINES:,}' if MAX_LINES > 0 else 'all'

    print(f'Thunderbird Log Loader')
    print(f'  Bootstrap: {BOOTSTRAP}')
    print(f'  Topic:     {TOPIC}')
    print(f'  Log file:  {log_path} ({file_size_mb:.0f} MB)')
    print(f'  Lines:     {limit_str}')
    print(f'  Batch:     {BATCH_SIZE}')
    print()

    # Connect producer
    print(f'Connecting to {BOOTSTRAP}...')
    producer = KafkaProducer(
        bootstrap_servers=BOOTSTRAP,
        value_serializer=lambda v: json.dumps(v).encode('utf-8'),
        key_serializer=lambda k: k.encode('utf-8') if k else None,
        api_version=(0, 10, 0),
        linger_ms=50,
        batch_size=65536,
        buffer_memory=134217728,  # 128 MB buffer for sustained throughput
        max_request_size=10485760,  # 10 MB max request
    )
    print(f'Connected.\n')

    # Load data
    start = time.time()
    success = 0
    parse_errors = 0
    total_bytes = 0

    with open(log_path, 'r', encoding='utf-8', errors='replace') as f:
        for line_no, raw_line in enumerate(f, 1):
            if MAX_LINES > 0 and line_no > MAX_LINES:
                break

            line = raw_line.rstrip('\n')
            doc = parse_line(line)

            if doc is None:
                parse_errors += 1
                continue

            # Use hostname as key for partition distribution
            key = doc['hostname']
            producer.send(TOPIC, key=key, value=doc)
            success += 1
            total_bytes += len(raw_line)

            if success % BATCH_SIZE == 0:
                producer.flush()
                elapsed = time.time() - start
                rate = success / elapsed
                mb_read = total_bytes / (1024 * 1024)
                pct = ''
                if MAX_LINES > 0:
                    pct = f' ({100.0 * line_no / MAX_LINES:.1f}%)'
                else:
                    pct = f' ({100.0 * line_no / 16601745:.1f}%)'
                print(f'  {success:>10,} produced | {rate:>8,.0f} msg/s | '
                      f'{mb_read:>7,.0f} MB read{pct}')

    producer.flush()
    producer.close()

    elapsed = time.time() - start
    rate = success / elapsed if elapsed > 0 else 0
    mb_total = total_bytes / (1024 * 1024)

    print()
    print(f'Done.')
    print(f'  Produced:     {success:,} messages')
    print(f'  Parse errors: {parse_errors:,}')
    print(f'  Elapsed:      {elapsed:.1f}s')
    print(f'  Rate:         {rate:,.0f} msg/s')
    print(f'  Data read:    {mb_total:,.0f} MB')


if __name__ == '__main__':
    main()
