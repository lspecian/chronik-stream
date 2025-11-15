#!/bin/bash
# Chronik Deadlock Detection Script
# Monitors running chronik-server processes for deadlock symptoms

echo "🔍 Checking for deadlocks in chronik-server processes..."
echo ""

DEADLOCKS_FOUND=0

for pid in $(pgrep -f chronik-server); do
    # Get process info
    cmd=$(ps -p $pid -o cmd= | head -c 50)
    cpu=$(ps -p $pid -o %cpu= | awk '{print $1}')

    # Check if CPU is exactly 0.0 (strict check to avoid false positives)
    if [ "$cpu" = "0.0" ]; then
        DEADLOCKS_FOUND=$((DEADLOCKS_FOUND + 1))
        echo "⚠️  WARNING: Possible deadlock detected"
        echo "   PID: $pid"
        echo "   Command: $cmd"
        echo "   CPU: ${cpu}%"
        echo ""

        # Get stack trace (requires gdb)
        if command -v gdb &> /dev/null; then
            echo "   Stack trace:"
            timeout 5s gdb -batch -ex "thread apply all bt" -p $pid 2>/dev/null | grep -A 5 "^Thread"
        else
            echo "   (Install gdb for stack traces: sudo apt-get install gdb)"
        fi
        echo ""
    else
        echo "✓ PID $pid: CPU ${cpu}% (healthy)"
    fi
done

echo ""
if [ $DEADLOCKS_FOUND -eq 0 ]; then
    echo "✅ No deadlocks detected"
    exit 0
else
    echo "❌ Found $DEADLOCKS_FOUND potential deadlocks"
    exit 1
fi
