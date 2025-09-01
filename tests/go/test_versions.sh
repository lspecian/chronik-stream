#!/bin/bash
# Test different versions of confluent-kafka-go to find where the crash starts

versions=("v2.0.2" "v2.1.0" "v2.2.0" "v2.3.0" "v2.4.0" "v2.5.0")

for version in "${versions[@]}"; do
    echo "================================================"
    echo "Testing confluent-kafka-go $version"
    echo "================================================"
    
    # Update go.mod
    go mod edit -require=github.com/confluentinc/confluent-kafka-go/v2@$version
    go mod tidy >/dev/null 2>&1
    
    # Run test
    output=$(go run test_downgraded.go 2>&1)
    
    # Check for crash
    if echo "$output" | grep -q "Assertion failed"; then
        echo "❌ CRASHES with $version"
    else
        echo "✅ WORKS with $version"
    fi
    
    # Check if it connects to Chronik
    if echo "$output" | grep -q "Chronik.*✓.*Metadata retrieved"; then
        echo "   ✓ Connects to Chronik successfully"
    else
        echo "   ⚠ Issues connecting to Chronik"
    fi
done

echo ""
echo "================================================"
echo "Summary complete!"