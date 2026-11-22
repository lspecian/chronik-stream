#!/bin/bash
# A cluster whose Kafka port uses TLS replicates.
#
# Before: followers dialled their leaders in plaintext. Against a TLS port
# every replica fetch failed ("announced a 352518912-byte response frame": the
# TLS alert read as a length), every ISR shrank to its leader and acks=all
# writes timed out, while clients saw a running cluster. Also, setting a CA
# made client certificates mandatory whatever CHRONIK_TLS_REQUIRE_CLIENT_CERT
# said.
#
# For each of: TLS with client certificates required (mTLS), and the same with
# SASL required (SASL_SSL, brokers authenticating as __chronik_broker):
#   - an RF3 topic takes 6,000 keyed acks=all records, all acknowledged;
#   - every partition returns to a full ISR, with no failed replica fetch;
#   - every broker serves all 6,000 records;
#   - a plaintext client is refused;
#   - (mTLS) /_sql through each node counts every record: queries fan out to
#     peers over HTTPS (they used plain HTTP against the HTTPS port).
# And without REQUIRE_CLIENT_CERT a client with no certificate is served.
#
# Usage: tls_cluster.sh <binary> <label>
# Needs: KAFKA_HOME, kcat, openssl.
set -u
export CHRONIK_BIN=${1:?binary under test}; LABEL=${2:?label}
source "$(dirname "$0")/../catalog/lib.sh"
for c in kcat openssl; do command -v $c >/dev/null || { echo "SKIP: $c not installed"; exit 77; }; done
N=6000
PKI="$DIR/data/tls-$LABEL"
fail=0
expect() {
  if [ "$2" = "$3" ]; then echo "  ok   $1: $2"
  else echo "  FAIL $1: got '$2', want '$3'"; fail=1; fi
}

rm -rf "$PKI"; mkdir -p "$PKI"
(
  cd "$PKI"
  openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.crt -days 2 -subj "/CN=chronik-e2e-ca" 2>/dev/null
  openssl req -newkey rsa:2048 -nodes -keyout tls.key -out tls.csr -subj "/CN=localhost" 2>/dev/null
  printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth,clientAuth\n' > ext.cnf
  openssl x509 -req -in tls.csr -CA ca.crt -CAkey ca.key -CAcreateserial -out tls.crt -days 2 -extfile ext.cnf 2>/dev/null
) || { echo "FAIL: could not create certificates"; exit 1; }
TLS_ENV="CHRONIK_TLS_CERT=$PKI/tls.crt CHRONIK_TLS_KEY=$PKI/tls.key CHRONIK_TLS_CA_CERT=$PKI/ca.crt"
trap cluster_stop EXIT

KC=() # kcat security settings for the current phase
# lib.sh probes the port in plaintext, which a TLS broker refuses.
wait_port() { for _ in $(seq 1 120); do kcat -L -b "localhost:$1" "${KC[@]}" -t __none >/dev/null 2>&1 && return 0; sleep 0.5; done; return 1; }
client_props() { # file security.protocol [sasl]
  {
    echo "security.protocol=$2"
    echo "ssl.truststore.type=PEM"
    echo "ssl.truststore.location=$PKI/ca.crt"
    echo "ssl.keystore.type=PEM"
    echo "ssl.keystore.certificate.chain=$(awk '{ printf "%s\\n", $0 }' "$PKI/tls.crt")"
    echo "ssl.keystore.key=$(awk '{ printf "%s\\n", $0 }' "$PKI/tls.key")"
    if [ -n "${3:-}" ]; then
      echo "sasl.mechanism=SCRAM-SHA-512"
      echo "sasl.jaas.config=org.apache.kafka.common.security.scram.ScramLoginModule required username=\"e2e\" password=\"e2e-password\";"
    fi
  } > "$1"
}
short_isr() { # topic props -> partitions short of 3 in-sync replicas, after waiting
  local short
  for _ in $(seq 60); do
    short=$("$K"/kafka-topics.sh --bootstrap-server localhost:9092 --command-config "$2" --describe --topic "$1" 2>/dev/null \
      | grep -o 'Isr: [0-9,]*' | awk -F'[ ,]' '{ if (NF - 1 < 3) n++ } END { print n+0 }')
    [ "$short" = 0 ] && break
    sleep 2
  done
  echo "$short"
}
fetch_failures() { grep -h 'Fetch from leader' "$DIR"/logs/node*.log 2>/dev/null | grep -c 'failed'; }
sql_count_via() { # node topic -> COUNT(*) as that node's Unified API answers it (HTTPS, client certificate)
  curl -s -m 30 --cacert "$PKI/ca.crt" --cert "$PKI/tls.crt" --key "$PKI/tls.key" -X POST "https://localhost:$((6091 + $1))/_sql" \
    -H 'Content-Type: application/json' -d "{\"query\":\"SELECT COUNT(*) AS c FROM $2\"}" | grep -o '"c":[0-9]*' | head -1 | cut -d: -f2
}

phase() { # name security.protocol extra-env sasl
  local name=$1 proto=$2 extra=$3 sasl=$4 t=tls_$1 props="$PKI/$1.properties"
  echo "-- $name"
  client_props "$props" "$proto" "$sasl"
  KC=(-X security.protocol="$proto" -X ssl.ca.location="$PKI/ca.crt" -X ssl.certificate.location="$PKI/tls.crt" -X ssl.key.location="$PKI/tls.key")
  [ -n "$sasl" ] && KC+=(-X sasl.mechanism=SCRAM-SHA-512 -X sasl.username=e2e -X sasl.password=e2e-password)
  EXTRA_ENV="$TLS_ENV CHRONIK_TLS_REQUIRE_CLIENT_CERT=true $extra" cluster_start || { echo "  FAIL cluster did not start"; fail=1; return; }
  "$K"/kafka-topics.sh --bootstrap-server "$BOOTS" --command-config "$props" --create --topic $t --partitions 6 --replication-factor 3 >/dev/null 2>&1
  sleep 3
  local failed
  failed=$(seq 1 $N | awk '{ printf "%d|{\"n\":%d}\n", $1, $1 }' \
    | kcat -P -K '|' -b "$BOOTS" "${KC[@]}" -t $t -X acks=all -X message.timeout.ms=60000 2>&1 | grep -c 'Delivery failed')
  expect "$name: acks=all records not acknowledged" "$failed" "0"
  expect "$name: partitions short of a full ISR" "$(short_isr $t "$props")" "0"
  expect "$name: failed replica fetches" "$(fetch_failures)" "0"
  for port in 9092 9093 9094; do
    expect "$name: records served by $port" \
      "$(kcat -C -b "localhost:$port" "${KC[@]}" -t $t -o beginning -e -q -f '%s\n' 2>/dev/null | sort -u | wc -l)" "$N"
  done
  if [ "$name" = mtls ]; then
    # Each node counts its peers' partitions over HTTPS (fan-out was plain HTTP).
    for n in 1 2 3; do
      for _ in $(seq 30); do [ "$(sql_count_via $n $t)" = "$N" ] && break; sleep 2; done
      expect "$name: /_sql count via node $n" "$(sql_count_via $n $t)" "$N"
    done
  fi
  expect "$name: a plaintext client is refused" \
    "$(kcat -L -b localhost:9092 -m 5 2>/dev/null | grep -c "topic \"$t\"")" "0"
  expect "$name: panics" "$(panics)" "0"
}

phase mtls SSL "" ""
phase sasl_ssl SASL_SSL "CHRONIK_SASL_ENABLED=true CHRONIK_SASL_USERS=e2e:e2e-password CHRONIK_INTER_BROKER_SECRET=e2e-inter-broker-secret-0123" sasl

echo "-- a CA without REQUIRE_CLIENT_CERT"
KC=(-X security.protocol=ssl -X ssl.ca.location="$PKI/ca.crt")
EXTRA_ENV="$TLS_ENV" cluster_start || { echo "  FAIL cluster did not start"; fail=1; }
expect "a client without a certificate is served" \
  "$(kcat -L -b localhost:9092 -X security.protocol=ssl -X ssl.ca.location="$PKI/ca.crt" -m 5 2>/dev/null | grep -c 'brokers:')" "1"

if [ $fail = 0 ]; then echo "RESULT $LABEL: PASS"; else echo "RESULT $LABEL: FAIL"; exit 1; fi
