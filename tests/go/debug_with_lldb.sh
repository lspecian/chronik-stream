#!/bin/bash
# Debug Go client crash with lldb

echo "Building Go test with debug symbols..."
cd /Users/luis/Development/chronik-stream/tests/go
go build -gcflags="all=-N -l" -o test_go_client_debug test_go_client.go

echo "Running with lldb..."
lldb -o "run" -o "bt" -o "quit" ./test_go_client_debug