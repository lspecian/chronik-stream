#!/bin/bash

# Test produce
echo "test-topic
0
0
1
key1
value1" | nc -w 1 localhost 9092 | xxd | head -20

sleep 1

# Test consume
echo "test-topic
0
0
-1
1000
1
1" | nc -w 1 localhost 9092 | xxd | head -20
