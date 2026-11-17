# Sourced by start.sh and restart.sh before launching nodes.
#
# A node that cannot bind its port exits at once, while whatever holds the port
# keeps answering — so a check run after a failed (re)start silently tests the
# wrong process. Refuse to start instead. Waits a few seconds first, for a
# cluster stop.sh has just killed to release its sockets.
for _ in $(seq 10); do
    busy=$(ss -ltnpH 2>/dev/null | awk '$4 ~ /:(9092|9093|9094|5001|5002|5003|6092)$/' || true)
    [ -z "$busy" ] && break
    sleep 1
done
if [ -n "$busy" ]; then
    echo "ERROR: the local cluster's ports are in use:" >&2
    echo "$busy" >&2
    echo "Stop whatever holds them (tests/cluster/stop.sh stops this cluster's nodes, whatever the binary is called)." >&2
    exit 1
fi
