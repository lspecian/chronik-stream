#!/usr/bin/env bash
# T6: full cluster restart with 5,000 topics.
# PASS: identical catalogs after boot; boot time not worse than BASE_BIN's
# (when given) on the same data shape.
source "$(dirname "$0")/lib.sh"
N="${T6_TOPICS:-5000}"
boot_time() { # binary -> seconds from restart to every node serving the full catalog
  local bin=$1 t0
  t0=$(date +%s.%N)
  CHRONIK_BIN="$bin" env $(node_env) "$DIR/restart.sh" >/dev/null 2>&1
  for n in 1 2 3; do wait_port "$(port_of $n)" || return 1; done
  for _ in $(seq 1 600); do
    ok=1; for n in 1 2 3; do [ "$(topics_on "$n" t6- | wc -l)" = "$N" ] || ok=0; done
    [ "$ok" = 1 ] && break; sleep 1
  done
  echo "$(date +%s.%N) - $t0" | bc
}
populate() { # binary
  BIN=$1 cluster_start || return 1
  seq 1 "$N" | xargs -P 16 -I{} "$K/kafka-topics.sh" --bootstrap-server localhost:9092 --create --topic t6-{} \
    --partitions 1 --replication-factor 3 >/dev/null 2>&1
}
populate "$BIN" || { fail "cluster did not start"; finish T6; }
[ "$(topics_on 1 t6- | wc -l)" = "$N" ] || fail "created $(topics_on 1 t6- | wc -l) of $N topics"
"$DIR/stop.sh" >/dev/null 2>&1
t_new=$(boot_time "$BIN")
say "boot with $N topics: ${t_new}s (binary under test)"
tool check "$BOOTS" t6- || fail "catalogs differ after restart"
if [ -n "${BASE_BIN:-}" ]; then
  populate "$BASE_BIN" || fail "baseline cluster did not start"
  "$DIR/stop.sh" >/dev/null 2>&1
  t_base=$(boot_time "$BASE_BIN")
  say "boot with $N topics: ${t_base}s (baseline $BASE_BIN)"
  awk -v a="$t_new" -v b="$t_base" 'BEGIN{exit !(a <= b * 1.10 + 2)}' || fail "boot ${t_new}s is worse than baseline ${t_base}s"
fi
finish "T6 full restart with $N topics"
