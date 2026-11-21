#!/bin/bash
# Unified API routes with path parameters answer, over HTTP and HTTPS.
#
# For HTTP framework changes: a route whose pattern the router no longer
# understands does not fail to compile, it panics at startup (axum 0.8 on
# `/:name`) or falls through to an empty 404. One node, plain HTTP then TLS
# (self-signed, HTTP/1.1 and HTTP/2), each parameterized route family must
# answer from its handler: SQL describe, Schema Registry subjects/versions/
# ids, vector stats, Elasticsearch-style _search and _doc.
#
# Usage: unified_api_routes.sh <binary> <label>
# Needs: KAFKA_HOME, kcat, openssl.
set -u
BIN=${1:?binary under test}; LABEL=${2:?label}
K="${KAFKA_HOME:?set KAFKA_HOME to a Kafka distribution}/bin"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"
command -v openssl >/dev/null || { echo "SKIP: openssl not installed"; exit 77; }
D=tests/cluster/data/routes-$LABEL; LOG=tests/cluster/logs/routes-$LABEL
B=localhost:9092; T=routes
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}
# "<status> <body bytes>" for a request; an unmatched route is "404 0".
hit() { # <curl args...>
  local out; out=$(curl -s -k -m 20 -o "$D/.resp" -w '%{http_code}' "$@"); echo "$out $(wc -c < "$D/.resp")"
}
status() { hit "$@" | cut -d' ' -f1; }
answered() { # handler answered: any status, non-empty body
  local r; r=$(hit "$@"); [ "${r#* }" != 0 ] && echo yes || echo "no ($r)"
}

tests/cluster/stop.sh >/dev/null 2>&1
. tests/cluster/ports_free.sh # exits when the ports are taken
rm -rf "$D"; mkdir -p "$D" "$(dirname "$LOG")"
pid=
trap 'kill $pid 2>/dev/null; wait $pid 2>/dev/null; rm -f "$D/.resp"' EXIT

run_checks() { # <scheme> <curl extra args...>
  local u="$1://localhost:6092"; shift
  expect "$u /health" "$(status "$@" $u/health)" "200"
  expect "$u /_sql/describe/{table}" "$(status "$@" $u/_sql/describe/$T)" "200"
  local subj="routes-$RANDOM-value"
  local id; id=$(curl -s -k -m 20 "$@" -X POST $u/subjects/$subj/versions -H 'Content-Type: application/vnd.schemaregistry.v1+json' \
    -d '{"schema":"{\"type\":\"record\",\"name\":\"R\",\"fields\":[{\"name\":\"n\",\"type\":\"long\"}]}"}' | grep -o '"id":[0-9]*' | cut -d: -f2)
  expect "$u POST /subjects/{subject}/versions returns an id" "$([ -n "$id" ] && echo yes || echo no)" "yes"
  expect "$u GET /subjects/{subject}/versions" "$(status "$@" $u/subjects/$subj/versions)" "200"
  expect "$u GET /subjects/{subject}/versions/{version}" "$(status "$@" $u/subjects/$subj/versions/1)" "200"
  expect "$u GET /schemas/ids/{id}" "$(status "$@" $u/schemas/ids/${id:-0})" "200"
  expect "$u /_vector/{topic}/stats answered by its handler" "$(answered "$@" $u/_vector/$T/stats)" "yes"
  expect "$u POST /{index}/_search" "$(status "$@" -X POST $u/$T/_search -H 'Content-Type: application/json' -d '{"query":{"match_all":{}}}')" "200"
  expect "$u GET /{index}/_doc/{id} answered by its handler" "$(answered "$@" $u/$T/_doc/0)" "yes"
  expect "$u an unknown route is a plain 404" "$(status "$@" $u/no/such/route)" "404"
}

start_node() { # <log suffix> [env...]
  env CHRONIK_ADVERTISED_ADDR=localhost "${@:2}" "$BIN" start --data-dir "$D" >> "$LOG-$1.log" 2>&1 &
  pid=$!
  for _ in $(seq 90); do curl -sk -m 2 -o /dev/null http://localhost:6092/health && return 0; curl -sk -m 2 -o /dev/null https://localhost:6092/health && return 0; sleep 1; done
  echo "FAIL: node did not come up ($LOG-$1.log)"; exit 1
}

echo "=== $LABEL ($BIN)"
start_node http
$K/kafka-topics.sh --bootstrap-server $B --create --topic $T --partitions 1 --replication-factor 1 >/dev/null 2>&1
sleep 2
seq 0 99 | awk '{ printf "{\"n\":%d,\"msg\":\"hello %d\"}\n", $1, $1 }' | kcat -P -b $B -t $T -p 0 2>/dev/null
sleep 3
echo "-- HTTP"
run_checks http

kill $pid; wait $pid 2>/dev/null
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=localhost \
  -keyout "$D/tls.key" -out "$D/tls.crt" >/dev/null 2>&1
start_node https CHRONIK_API_TLS_CERT="$D/tls.crt" CHRONIK_API_TLS_KEY="$D/tls.key"
echo "-- HTTPS, HTTP/1.1"
run_checks https --http1.1
echo "-- HTTPS, HTTP/2"
run_checks https --http2
expect "plain HTTP refused while TLS is on" "$(status http://localhost:6092/health)" "000"
expect "panics" "$(cat "$LOG"-*.log | grep -c panicked)" "0"

if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
