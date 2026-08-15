#!/usr/bin/env bash
# The bare-metal measurement: one broker per machine, client on a fourth.
#
# `perf_matrix.sh` runs three brokers and the load generator on one box, which
# understates network cost and overstates disk contention. This runs the same
# benchmark against three Dell R630s over their 1 GbE link, with the client on a
# separate machine — which is the part every previous bare-metal number here
# lacked. The old report co-located the client with a broker, so it could not
# tell a network-bound result from a sender-bound one (OQ1).
#
# It touches nothing else on those machines: the brokers run as plain processes
# on the host, on ports Kubernetes is not using, so the MicroK8s clusters keep
# running untouched.
#
#   ./tests/cluster/baremetal.sh deploy   # copy binary + config to each node
#   ./tests/cluster/baremetal.sh up
#   ./tests/cluster/baremetal.sh bench
#   ./tests/cluster/baremetal.sh down
#   ./tests/cluster/baremetal.sh all      # deploy, up, bench, down
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$ROOT/target/release/chronik-server"
BENCH="$ROOT/target/release/chronik-bench"
REMOTE=/home/ubuntu/chronik-baremetal

NODES=("${BAREMETAL_NODES:-192.168.1.31 192.168.1.32 192.168.1.33}")
read -r -a HOSTS <<< "${NODES[0]}"
BOOT=$(printf '%s:9092,' "${HOSTS[@]}"); BOOT=${BOOT%,}

DURATION="${PERF_DURATION:-30s}"
CONCURRENCY="${PERF_CONCURRENCY:-64}"
RUNS="${PERF_RUNS:-3}"
SETTLE="${PERF_SETTLE:-20}"
SIZES="${PERF_SIZES:-256 1024}"
ACKS_LEVELS="${PERF_ACKS:-0 1 all}"

# Rows are appended here as they are produced, not just printed.
#
# This run takes two hours, and the first attempt lost four of its six rows to a
# `tail` on the calling side. A measurement that expensive should not exist only
# in a pipe.
LOGS="$ROOT/tests/cluster/logs"
RESULTS="${PERF_RESULTS:-$LOGS/baremetal-results.txt}"

say() { printf '%s\n' "$*"; }
sshq() { timeout 120 ssh -o ConnectTimeout=8 -o BatchMode=yes "ubuntu@$1" "$2"; }

deploy() {
  [ -x "$BIN" ] || { say "SKIP: cargo build --release --bin chronik-server"; exit 0; }
  local n=0
  for host in "${HOSTS[@]}"; do
    n=$((n + 1))
    sshq "$host" "mkdir -p $REMOTE/data" || { say "cannot reach $host"; exit 1; }
    # The config differs only in node_id and advertise address, so it is
    # generated rather than checked in three times.
    {
      printf 'enabled = true\nnode_id = %s\ndata_dir = "%s/data"\n' "$n" "$REMOTE"
      printf 'replication_factor = 3\nmin_insync_replicas = 2\n\n'
      printf '[bind]\nkafka = "0.0.0.0:9092"\nwal = "0.0.0.0:9291"\nraft = "0.0.0.0:5001"\n\n'
      printf '[advertise]\nkafka = "%s:9092"\nwal = "%s:9291"\nraft = "%s:5001"\n' "$host" "$host" "$host"
      local p=0
      for peer in "${HOSTS[@]}"; do
        p=$((p + 1))
        # ⛔ `REPL_NET=1` DOES NOT WORK against the broker as it stands, and the
        # reason is worth keeping.
        #
        # The intent was to put inter-broker traffic on the 10 GbE fabric while
        # clients kept reaching the brokers on the 1 GbE address — worth doing,
        # because replication is the larger half of a leader's traffic: at RF=3
        # a leader sends every record twice more than it received it, so its
        # egress is 2x its ingress and both share one port.
        #
        # It assumed `[[peers]].kafka` (what a follower fetches from) and
        # `[advertise].kafka` (what clients are told) were independent. They are
        # not. `cluster/broker_registration.rs` builds the broker list published
        # in Metadata responses from `peers[].kafka`, so setting this makes the
        # cluster advertise 172.16.10.x to *clients* as well:
        #
        #     $ kcat -L -b 192.168.1.31:9092
        #      broker 1 at 172.16.10.31:9092 (controller)
        #      broker 2 at 172.16.10.32:9092
        #      broker 3 at 172.16.10.33:9092
        #
        # A client with no route to that subnet then hangs after bootstrap —
        # observed as `chronik-bench` running for 25 minutes on a `-d 30s` job.
        # `[advertise].kafka` is documented as "what clients connect to" and is
        # not used for this. Fixing that is the prerequisite for a
        # replication-only fabric; see docs/ROADMAP_REPLICATION.md.
        local paddr="$peer"
        if [ "${REPL_NET:-0}" = "1" ]; then
          paddr="172.16.10.${peer##*.}"
        fi
        printf '\n[[peers]]\nid = %s\nkafka = "%s:9092"\nwal = "%s:9291"\nraft = "%s:5001"\n' \
          "$p" "$paddr" "$paddr" "$paddr"
      done
    } | sshq "$host" "cat > $REMOTE/node.toml"
    timeout 600 scp -q "$BIN" "ubuntu@$host:$REMOTE/chronik-server" || { say "copy to $host failed"; exit 1; }
    sshq "$host" "chmod +x $REMOTE/chronik-server"
    say "  $host: deployed"
  done
}

up() {
  for host in "${HOSTS[@]}"; do
    sshq "$host" "rm -rf $REMOTE/data && mkdir -p $REMOTE/data && \
      cd $REMOTE && RUST_LOG=warn CHRONIK_UNIFIED_API_PORT=6092 \
      nohup ./chronik-server start --config $REMOTE/node.toml > $REMOTE/node.log 2>&1 & echo started"
  done
  for host in "${HOSTS[@]}"; do
    for _ in $(seq 1 60); do
      (exec 3<>/dev/tcp/"$host"/9092) 2>/dev/null && { exec 3<&-; break; }
      sleep 1
    done
  done
  sleep 15
  say "  brokers up on ${HOSTS[*]}"
}

down() {
  for host in "${HOSTS[@]}"; do
    # Matched on the CONFIG PATH, which is the only thing that is both unique to
    # these processes and actually present in their command line.
    #
    # Two ways to get this wrong, and this harness has had both. `chronik-server
    # start` also matches the Kubernetes containers on these machines, whose
    # PIDs are visible from the host — that risks killing a live cluster while
    # cleaning up a benchmark. Matching the full binary path is safe but matches
    # *nothing*: `up` starts them as `./chronik-server` after a `cd`, so the
    # command line never contains the directory. Teardown silently did nothing,
    # and three brokers were found still running a day later.
    sshq "$host" "pkill -9 -f 'config $REMOTE/node.toml' >/dev/null 2>&1; sync; echo stopped"
  done
  # Do not return while a listener is still held: the next run would measure a
  # broker that is going away. Same hazard as perf_matrix.sh.
  for host in "${HOSTS[@]}"; do
    for _ in $(seq 1 60); do
      (exec 3<>/dev/tcp/"$host"/9092) 2>/dev/null && { exec 3<&-; sleep 1; continue; }
      break
    done
  done
}

extract() { # $1=output $2=field
  case "$2" in
    msgs) printf '%s' "$1" | sed -n 's/.*Message rate: *\([0-9,]*\) msg\/s.*/\1/p' | tail -1 | tr -d ',' ;;
    mb)   printf '%s' "$1" | sed -n 's/.*Bandwidth: *\([0-9.,]*\) MB\/s.*/\1/p' | tail -1 | tr -d ',' ;;
    p99)  printf '%s' "$1" | sed -n 's/.*p99: .*(\ *\([0-9.]*\) ms).*/\1/p' | tail -1 ;;
  esac
}
median() { printf '%s\n' "$@" | sort -n | awk '{v[NR]=$0} END {print v[int((NR+1)/2)]}'; }

# Peak receive rate on a broker's NIC during the run, in Mbit/s.
#
# This is what answers OQ1. The link is 1 GbE, so anything approaching ~940
# Mbit/s means the transport is the constraint and no amount of sender work will
# help; well below it means the bottleneck is the broker or the client.
nic_peak() { # $1=host, $2=seconds
  sshq "$1" "
    dev=\$(ip route get 1.1.1.1 2>/dev/null | sed -n 's/.* dev \\([^ ]*\\).*/\\1/p' | head -1)
    prev=\$(cat /sys/class/net/\$dev/statistics/rx_bytes)
    peak=0
    for _ in \$(seq 1 $2); do
      sleep 1
      cur=\$(cat /sys/class/net/\$dev/statistics/rx_bytes)
      rate=\$(( (cur - prev) * 8 / 1000000 ))
      prev=\$cur
      [ \$rate -gt \$peak ] && peak=\$rate
    done
    echo \$peak"
}

bench() {
  [ -x "$BENCH" ] || { say "SKIP: cargo build --release --bin chronik-bench"; exit 0; }
  say "== bare metal: 3x Dell R630, 1 GbE, client on $(hostname) =="
  say "   $CONCURRENCY producers, ${DURATION}, median of $RUNS, client is NOT on a broker"
  say ""
  mkdir -p "$(dirname "$RESULTS")"
  for size in $SIZES; do
    for acks in $ACKS_LEVELS; do
      local msgs=() mbs=() p99s=() peak=""
      for run in $(seq 1 "$RUNS"); do
        up
        local nicfile; nicfile=$(mktemp)
        ( nic_peak "${HOSTS[0]}" 30 > "$nicfile" ) &
        local nicpid=$!
        # `--linger-ms 0` is deliberate, not an oversight: every producer waits
        # for its own acknowledgement, so the run measures the ROUND TRIP rather
        # than the ingest ceiling. `PERF_LINGER=10` measures the other regime —
        # what the same cluster does when the client batches — and the gap
        # between the two is the cost of asking per message instead of per
        # batch. The report the numbers here replaced measured the batched
        # regime and described it as the cluster's throughput.
        local out; out=$("$BENCH" -b "$BOOT" -t "bm${PERF_LINGER:-0}-$size-$acks-$run-$$" \
          -c "$CONCURRENCY" -s "$size" -d "$DURATION" -p 3 --acks "$acks" \
          --linger-ms "${PERF_LINGER:-0}" -m produce 2>&1)
        wait $nicpid 2>/dev/null
        peak=$(cat "$nicfile" 2>/dev/null); rm -f "$nicfile"
        msgs+=("$(extract "$out" msgs)")
        mbs+=("$(extract "$out" mb)")
        p99s+=("$(extract "$out" p99)")
        down
        sleep "$SETTLE"
      done
      printf '  %-6s acks=%-4s %10s msg/s  %8s MB/s  p99 %sms  node1 NIC peak %s Mbit/s   (%s)\n' \
        "${size}B" "$acks" "$(median "${msgs[@]}")" "$(median "${mbs[@]}")" \
        "$(median "${p99s[@]}")" "${peak:-?}" "$(printf '%s ' "${msgs[@]}")" \
        | tee -a "$RESULTS"
    done
  done
}

# The OTHER regime: how fast the cluster ingests when the client batches.
#
# Not `chronik-bench --linger-ms`. That benchmark awaits each message's delivery
# before sending the next, so a producer never has more than one message in
# flight and there is nothing for linger to accumulate — it only adds dead time.
# Measured, at 256 B `acks=0`: 5,359 msg/s with `--linger-ms 10` against 110,427
# with 0, and the NIC fell from 627 to 18 Mbit/s. 64 producers / 10 ms = 6,400,
# which is the whole of it. Linger cannot batch a workload that is synchronous
# by construction.
#
# `kcat` pipes records in without waiting on any of them, so librdkafka batches
# them for real. Same method as `perf_replication.sh`, pointed at the Dells.
batched() {
  command -v kcat >/dev/null || { say "SKIP: kcat not installed"; exit 0; }
  # Five million, not the 200,000 `perf_replication.sh` uses.
  #
  # 200,000 x 256B finishes in about half a second at these rates, and half a
  # second measures the client's send buffer draining, not the cluster's
  # throughput. It reported `acks=0` at 355,871 and `acks=1` at 386,100 — i.e.
  # acknowledging every batch on the leader's disk looked FREE, and slightly
  # faster than not acknowledging at all, which cannot be true. Over a proper
  # window the same runs give 429,737 and 122,828.
  #
  # The run length is printed with every row so a too-short window is visible
  # instead of silently becoming a number.
  local records="${PERF_RECORDS:-5000000}"
  local payload; payload=$(head -c "${PERF_SIZE:-256}" /dev/zero | tr '\0' 'x')
  mkdir -p "$(dirname "$RESULTS")"
  say "== bare metal, BATCHED: kcat, $records records x ${PERF_SIZE:-256}B, median of $RUNS =="
  say "   the client does not wait per message; this is the ingest ceiling"
  say ""
  for acks in $ACKS_LEVELS; do
    local rates=() secs=() errs_total=0 stored_last=""
    for run in $(seq 1 "$RUNS"); do
      up
      local topic="bmb-$acks-$run-$$"
      echo warmup | timeout 60 kcat -P -b "$BOOT" -t "$topic" -X request.required.acks="$acks" 2>/dev/null
      sleep 8
      # Client errors are part of the result, not noise to be discarded.
      #
      # A run that produces N records and stores fewer is either a broker losing
      # acknowledged writes or a client giving up, and throwing stderr away makes
      # those indistinguishable — which is how a *failed* run becomes a
      # throughput number. The local equivalent of this measurement returned
      # 5,555 msg/s at `acks=all` with 76,952 `REQTMOUT` errors and 543,430
      # records missing; without the error count that reads as a throughput
      # result rather than a collapse.
      local errfile="$LOGS/bmb-$acks-$run.err"
      local t0 t1 ms
      t0=$(date +%s%3N)
      seq 1 "$records" | sed "s/\$/ $payload/" \
        | timeout 900 kcat -P -b "$BOOT" -t "$topic" -X request.required.acks="$acks" 2>"$errfile"
      t1=$(date +%s%3N)
      local errs; errs=$(grep -c . "$errfile" 2>/dev/null); errs=${errs:-0}
      errs_total=$((errs_total + errs))
      # Summed across every partition, not partition 0.
      #
      # Querying one partition of a three-partition topic reported "stored
      # 1,656,764/5,000,000" for a run that had in fact stored all five million —
      # a two-thirds shortfall that was purely the query. A correctness check
      # that cries wolf gets ignored, which is worse than not having it.
      local stored=0 p end
      for p in 0 1 2; do
        end=$(timeout 30 kcat -Q -b "$BOOT" -t "$topic:$p:-1" 2>/dev/null \
          | sed -n 's/.*:\([0-9]*\)$/\1/p' | tail -1)
        stored=$(( stored + ${end:-0} ))
      done
      stored_last="$stored"
      ms=$((t1 - t0)); [ "$ms" -le 0 ] && ms=1
      rates+=("$(( records * 1000 / ms ))")
      secs+=("$(( ms / 1000 ))")
      down
      sleep "$SETTLE"
    done
    local window; window=$(median "${secs[@]}")
    local warn=""
    [ "$window" -lt 10 ] && warn="  ⚠ WINDOW TOO SHORT — raise PERF_RECORDS"
    [ "$errs_total" -gt 0 ] && warn="$warn  ⚠ NOT A THROUGHPUT RESULT: $errs_total client error(s)"
    printf '  batched %-4s acks=%-4s %10s msg/s over %ss   stored %s/%s  errors %s   (%s)%s\n' \
      "${PERF_SIZE:-256}B" "$acks" "$(median "${rates[@]}")" "$window" \
      "${stored_last:-?}" "$records" "$errs_total" \
      "$(printf '%s ' "${rates[@]}")" "$warn" \
      | tee -a "$RESULTS"
  done
}

case "${1:-all}" in
  deploy) deploy ;;
  batched) batched ;;
  up)     up ;;
  down)   down ;;
  bench)  bench ;;
  all)    deploy; bench; down ;;
  *)      say "usage: $0 {deploy|up|bench|down|all}"; exit 1 ;;
esac
