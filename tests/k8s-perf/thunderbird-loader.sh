#!/usr/bin/env bash
#
# Thunderbird Log Loader for Chronik Stream SV-2a Scale Validation.
# Replaces thunderbird-loader.py — no Python dependency.
#
# Parses Loghub-2.0 Thunderbird supercomputer logs (16.6M lines) and produces
# structured JSON messages to a Chronik topic via kcat.
#
# Usage:
#   # Local (against local Chronik)
#   ./thunderbird-loader.sh
#
#   # Custom settings
#   BOOTSTRAP_SERVERS=chronik:9092 TOPIC=thunderbird MAX_LINES=100000 ./thunderbird-loader.sh
#
# Requirements: kcat (or kafkacat), jq (optional, for counting)
#
set -euo pipefail

BOOTSTRAP="${BOOTSTRAP_SERVERS:-localhost:9092}"
TOPIC="${TOPIC:-thunderbird}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
LOG_FILE="${LOG_FILE:-${SCRIPT_DIR}/../../datasets/thunderbird/Thunderbird/Thunderbird_full.log}"
MAX_LINES="${MAX_LINES:-0}"
BATCH_SIZE="${BATCH_SIZE:-5000}"

# Find kcat or kafkacat
KCAT=""
if command -v kcat &>/dev/null; then
  KCAT="kcat"
elif command -v kafkacat &>/dev/null; then
  KCAT="kafkacat"
else
  echo "ERROR: kcat or kafkacat not found. Install with: apt install kafkacat"
  exit 1
fi

# Resolve log file path
LOG_FILE="$(cd "$(dirname "$LOG_FILE")" 2>/dev/null && pwd)/$(basename "$LOG_FILE")" 2>/dev/null || LOG_FILE="$LOG_FILE"
if [ ! -f "$LOG_FILE" ]; then
  echo "ERROR: Log file not found: $LOG_FILE"
  echo "  Set LOG_FILE env var or ensure the dataset is at the expected location."
  exit 1
fi

FILE_SIZE_MB=$(( $(stat -c%s "$LOG_FILE" 2>/dev/null || stat -f%z "$LOG_FILE" 2>/dev/null) / 1048576 ))
LIMIT_STR="all"
[ "$MAX_LINES" -gt 0 ] && LIMIT_STR="$(printf '%d' "$MAX_LINES")"

echo "Thunderbird Log Loader"
echo "  Bootstrap: $BOOTSTRAP"
echo "  Topic:     $TOPIC"
echo "  Log file:  $LOG_FILE (${FILE_SIZE_MB} MB)"
echo "  Lines:     $LIMIT_STR"
echo "  Batch:     $BATCH_SIZE"
echo ""

# Parse and produce using awk + kcat pipe
# Thunderbird format: Label UnixTs YYYY.MM.DD Hostname Mon DD HH:MM:SS Source Rest
echo "Connecting to $BOOTSTRAP..."

START_TIME=$(date +%s)
SUCCESS=0
PARSE_ERRORS=0

# Use awk to parse lines into JSON, pipe to kcat in batches
process_lines() {
  local batch_file
  batch_file=$(mktemp)
  local count=0 errors=0 batch_count=0

  while IFS= read -r line; do
    # Parse: label timestamp date hostname month day time source rest
    label=$(echo "$line" | awk '{print $1}')
    ts=$(echo "$line" | awk '{print $2}')
    date_str=$(echo "$line" | awk '{print $3}')
    hostname=$(echo "$line" | awk '{print $4}')
    time_str=$(echo "$line" | awk '{print $5" "$6" "$7}')
    source=$(echo "$line" | awk '{print $8}')
    rest=$(echo "$line" | awk '{for(i=9;i<=NF;i++) printf "%s ", $i; print ""}' | sed 's/ $//')

    # Validate timestamp is numeric
    if ! [[ "$ts" =~ ^[0-9]+$ ]]; then
      errors=$((errors + 1))
      continue
    fi

    # Extract component and message from rest
    component=$(echo "$rest" | sed 's/\[.*$//' | sed 's/:.*//')
    message=$(echo "$rest" | sed 's/^[^:]*: *//')

    # Escape JSON special characters
    message=$(echo "$message" | sed 's/\\/\\\\/g; s/"/\\"/g')
    component=$(echo "$component" | sed 's/\\/\\\\/g; s/"/\\"/g')

    # Write JSON line with key (hostname for partition distribution)
    printf '%s\t{"label":"%s","timestamp":%s,"date":"%s","hostname":"%s","time":"%s","source":"%s","component":"%s","message":"%s"}\n' \
      "$hostname" "$label" "$ts" "$date_str" "$hostname" "$time_str" "$source" "$component" "$message" >> "$batch_file"

    count=$((count + 1))
    batch_count=$((batch_count + 1))

    if [ "$batch_count" -ge "$BATCH_SIZE" ]; then
      # Flush batch via kcat
      $KCAT -P -b "$BOOTSTRAP" -t "$TOPIC" -K '\t' < "$batch_file" 2>/dev/null
      > "$batch_file"  # truncate
      batch_count=0
      elapsed=$(( $(date +%s) - START_TIME ))
      [ "$elapsed" -eq 0 ] && elapsed=1
      rate=$((count / elapsed))
      printf "  %'10d produced | %'8d msg/s\n" "$count" "$rate"
    fi

    [ "$MAX_LINES" -gt 0 ] && [ "$count" -ge "$MAX_LINES" ] && break
  done

  # Flush remaining
  if [ "$batch_count" -gt 0 ]; then
    $KCAT -P -b "$BOOTSTRAP" -t "$TOPIC" -K '\t' < "$batch_file" 2>/dev/null
  fi

  rm -f "$batch_file"
  echo ""
  echo "SUCCESS=$count"
  echo "PARSE_ERRORS=$errors"
}

# Run the processor
eval "$(cat "$LOG_FILE" | process_lines | grep -E '^(SUCCESS|PARSE_ERRORS)=')"

ELAPSED=$(( $(date +%s) - START_TIME ))
[ "$ELAPSED" -eq 0 ] && ELAPSED=1
RATE=$((SUCCESS / ELAPSED))

echo ""
echo "Done."
printf "  Produced:     %'d messages\n" "$SUCCESS"
printf "  Parse errors: %'d\n" "$PARSE_ERRORS"
echo "  Elapsed:      ${ELAPSED}s"
printf "  Rate:         %'d msg/s\n" "$RATE"
