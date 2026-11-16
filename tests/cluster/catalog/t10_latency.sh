#!/usr/bin/env bash
# T10: catalog operation latency on a settled cluster.
# PASS: CreateTopic p99 < 50 ms through the leader and through a follower.
source "$(dirname "$0")/lib.sh"
cluster_start || { fail "cluster did not start"; finish T10; }
sleep 3
L=$(current_leader); F=$(( L % 3 + 1 ))
for n in "$L" "$F"; do
  out=$(tool latency "localhost:$(port_of "$n")" "t10-n$n-" 300 3)
  say "node $n ($( [ "$n" = "$L" ] && echo leader || echo follower )): $out"
  p99=$(echo "$out" | grep -o 'p99=[0-9]*' | grep -o '[0-9]*')
  [ -n "$p99" ] && [ "$p99" -lt 50 ] || fail "node $n CreateTopic p99 ${p99:-?} ms >= 50"
done
finish "T10 CreateTopic p99 < 50 ms"
