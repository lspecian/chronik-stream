#!/usr/bin/env bash
# Pre-tag matrix for Schema A ingest data-loss fix.
# Reproduces issue #2's queries + the symmetric `_value` / `_key` alias tests
# across three cells: hot-on, hot-off, REST-only.
#
# A green run means: `_value` / `_key` are populated on the cold path AND
# the alias `value` ↔ `_value` resolves in both directions.
#
# Run from repo root. Kills any lingering chronik-server before starting.

set -u
set -o pipefail

REPO="$(git rev-parse --show-toplevel)"
BIN="${REPO}/target/release/chronik-server"
BASE_DATA="/tmp/chronik-issue2-matrix"
KAFKA_HOST="localhost:9092"
API="http://localhost:6092"
TOPIC="issue2-repro"
KEY="A"
VALUE='{"note":"has foo content"}'

if [[ ! -x "${BIN}" ]]; then
  echo "FATAL: ${BIN} not found. Build it first: cargo build --release --bin chronik-server" >&2
  exit 2
fi

cleanup() {
  pkill -f "target/release/chronik-server" 2>/dev/null || true
  sleep 1
}
trap cleanup EXIT
cleanup

# Each query lives in its own function so the cell loop is just 6 one-liners.
# Expected hit counts are parameterised per cell because REST-only uses a
# different index type; the fact expectations differ per cell is itself a
# signal (document not tokenised the same way), but the matrix we care about
# is hot-on and hot-off × Kafka-produced.

# ---------- server control ----------
start_server() {
  local cell="$1"
  local env_flag="$2"  # "" or "CHRONIK_HOT_TEXT_ENABLED=false"
  local data_dir="${BASE_DATA}/${cell}"
  rm -rf "${data_dir}"
  mkdir -p "${data_dir}"
  echo ">>> [${cell}] starting server (env: ${env_flag:-default})"
  # CHRONIK_DEFAULT_SEARCHABLE=true — auto-created Kafka topics need this, else
  # the realtime indexer AND hot text index both skip them (traits.rs:72-81).
  # The bug under test is about indexing/query correctness once a topic IS
  # searchable; the searchable default itself is orthogonal.
  ( cd "${REPO}" && env ${env_flag} \
      CHRONIK_DEFAULT_SEARCHABLE=true \
      CHRONIK_DATA_DIR="${data_dir}" \
      RUST_LOG=warn \
      "${BIN}" start --data-dir "${data_dir}" \
      >"${data_dir}/server.log" 2>&1 ) &
  # Wait for unified API to come up
  for _ in $(seq 1 30); do
    if curl -fsS "${API}/health" >/dev/null 2>&1; then
      echo ">>> [${cell}] server up"
      return 0
    fi
    sleep 1
  done
  echo "!!! [${cell}] server failed to come up, tail:"
  tail -20 "${data_dir}/server.log"
  return 1
}

stop_server() {
  pkill -f "target/release/chronik-server" 2>/dev/null || true
  # Give it a moment to release the port
  for _ in $(seq 1 10); do
    if ! curl -fsS "${API}/health" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
}

# ---------- fixtures ----------
produce_one_record() {
  # kcat needs -u to not drop the message; -P = produce; -Z = null value → empty, we want value present
  printf '%s' "${VALUE}" | kcat -b "${KAFKA_HOST}" -t "${TOPIC}" -P -k "${KEY}" -z snappy
}

rest_create_doc() {
  # Mirror of produce_one_record for the REST-only cell. The REST API uses a
  # dynamic schema: the doc body fields become index fields. We build the body
  # with jq so the Kafka value (itself JSON) embeds safely as a string.
  curl -fsS -X PUT "${API}/${TOPIC}" -H 'content-type: application/json' \
    -d '{"mappings":{"properties":{"value":{"type":"text"},"key":{"type":"keyword"}}}}' >/dev/null \
    || echo "!!! [rest-only] create_index returned HTTP error"
  local body
  body=$(jq -cn --arg v "${VALUE}" --arg k "${KEY}" '{value: $v, key: $k}')
  curl -fsS -X PUT "${API}/${TOPIC}/_doc/1" -H 'content-type: application/json' \
    -d "${body}" >/dev/null \
    || echo "!!! [rest-only] put_doc returned HTTP error"
  # Refresh so the doc is visible immediately (best-effort; endpoint may 404).
  curl -fsS -X POST "${API}/${TOPIC}/_refresh" >/dev/null 2>&1 || true
}

wait_for_visibility() {
  # Realtime indexer flushes on a batch interval (default ~5s). Hot index is
  # sub-second. Poll for match_all to return ≥1 doc for up to 30s.
  # Uses size:1 because search_in_index short-circuits size:0 to hits.len()==0.
  for _ in $(seq 1 30); do
    local n
    n=$(curl -fsS -X POST "${API}/${TOPIC}/_search" -H 'content-type: application/json' \
      -d '{"query":{"match_all":{}},"size":1}' 2>/dev/null | jq -r '.hits.total.value // 0')
    if [[ "${n:-0}" -ge 1 ]]; then
      return 0
    fi
    sleep 1
  done
  return 1
}

# ---------- queries ----------
hits_for() {
  local body="$1"
  curl -fsS -X POST "${API}/${TOPIC}/_search" -H 'content-type: application/json' \
    -d "${body}" | jq -r '.hits.total.value // 0'
}

run_queries() {
  local cell="$1"
  local -a results=()
  # A: match_all + match.key="B" — expect 0
  results+=("A:$(hits_for '{"query":{"bool":{"must":[{"match_all":{}},{"match":{"key":"B"}}]}},"size":10}')")
  # B: match.value="foo" + match.key="B" — expect 0
  results+=("B:$(hits_for '{"query":{"bool":{"must":[{"match":{"value":"foo"}},{"match":{"key":"B"}}]}},"size":10}')")
  # C: match.value="foo" — expect 1
  results+=("C:$(hits_for '{"query":{"match":{"value":"foo"}},"size":10}')")
  # D: match.key="A" — expect 1
  results+=("D:$(hits_for '{"query":{"match":{"key":"A"}},"size":10}')")
  # E: match._value="foo" — expect 1 (alias is symmetric)
  results+=("E:$(hits_for '{"query":{"match":{"_value":"foo"}},"size":10}')")
  # F: match._key="A" — expect 1
  results+=("F:$(hits_for '{"query":{"match":{"_key":"A"}},"size":10}')")

  echo ">>> [${cell}] results: ${results[*]}"

  # Evaluate
  local fail=0
  check() { local label="$1" want="$2" got="$3"
    if [[ "${got}" != "${want}" ]]; then
      echo "!!! [${cell}] ${label} expected ${want}, got ${got}"
      fail=1
    fi
  }
  check "A match_all+key=B"        0 "${results[0]#A:}"
  check "B value=foo+key=B"        0 "${results[1]#B:}"
  check "C value=foo"              1 "${results[2]#C:}"
  check "D key=A"                  1 "${results[3]#D:}"
  check "E _value=foo alias"       1 "${results[4]#E:}"
  check "F _key=A alias"           1 "${results[5]#F:}"
  return ${fail}
}

# ---------- cells ----------

overall=0

# Cell 1: hot-on (default)
start_server "hot-on" ""
produce_one_record
if ! wait_for_visibility; then
  echo "!!! [hot-on] doc never became visible"; overall=1
else
  run_queries "hot-on" || overall=1
fi
stop_server

# Cell 2: hot-off
start_server "hot-off" "CHRONIK_HOT_TEXT_ENABLED=false"
produce_one_record
if ! wait_for_visibility; then
  echo "!!! [hot-off] doc never became visible"; overall=1
else
  run_queries "hot-off" || overall=1
fi
stop_server

# Cell 3: REST-only — use default server; document created via REST, no Kafka produce
start_server "rest-only" ""
rest_create_doc
# Small pause for in-memory commit
sleep 1
if ! wait_for_visibility; then
  echo "!!! [rest-only] doc never became visible"; overall=1
else
  run_queries "rest-only" || overall=1
fi
stop_server

if [[ ${overall} -eq 0 ]]; then
  echo "==== MATRIX PASS ===="
else
  echo "==== MATRIX FAIL ===="
fi
exit ${overall}
