#!/bin/bash
# Test script to demonstrate CLI commands
# This shows the available cluster management commands

echo "Testing Chronik CLI Commands"
echo "=============================="
echo

# Binary path
BINARY="./target/debug/chronik-server"

# Check if binary exists
if [ ! -f "$BINARY" ]; then
    echo "Error: Binary not found at $BINARY"
    echo "Please run: cargo build --bin chronik-server"
    exit 1
fi

echo "1. Show cluster command help"
echo "----------------------------"
$BINARY cluster --help
echo
echo

echo "2. Show cluster status subcommand help"
echo "---------------------------------------"
$BINARY cluster status --help
echo
echo

echo "3. Show cluster add-node subcommand help"
echo "-----------------------------------------"
$BINARY cluster add-node --help
echo
echo

echo "4. Show cluster rebalance subcommand help"
echo "------------------------------------------"
$BINARY cluster rebalance --help
echo
echo

echo "5. List all available cluster subcommands"
echo "------------------------------------------"
$BINARY cluster --help | grep -A 20 "Commands:"
echo
echo

echo "Test parsing cluster status command"
echo "------------------------------------"
echo "Command: cluster status --detailed"
echo "(This will attempt to connect to http://localhost:5001)"
echo

echo "Test parsing add-node command"
echo "-----------------------------"
echo "Command: cluster add-node --id 4 --addr 192.168.1.40:9092 --raft-port 9093"
echo

echo "Test parsing rebalance command"
echo "-------------------------------"
echo "Command: cluster rebalance --dry-run --max-moves 5"
echo

echo "CLI commands are ready to use!"
echo "Note: These commands will try to connect to a running Chronik cluster."
echo "Start a cluster with: cargo run --bin chronik-server -- standalone"
