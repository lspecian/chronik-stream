#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# Ontology O-0 + O-1 exit-gate E2E test (docs/ROADMAP_ONTOLOGY.md).
#
# Verifies the object model AND link traversal end-to-end against a LIVE Chronik
# broker started with the ontology enabled:
#   - registry: an ObjectType produced to ont.types.{tenant} is served by
#     GET /ontology/v1/types
#   - get_object: resolves an instance's attributes from its backing mem.fact
#     projection (single + multi-valued), mapping predicate -> attribute
#   - provenance-cite gate: EVERY resolved attribute cites its source offsets
#   - as_of: point-in-time resolution honours valid_from
#   - 404 on an unknown instance
#
# Prereqs (the caller starts the server — this script does not):
#   chronik-server built with `--features memory`, started with:
#     CHRONIK_ONTOLOGY_ENABLED=true CHRONIK_MEMORY_KAFKA=localhost:9092 \
#     CHRONIK_MEMORY_API=http://127.0.0.1:6092  (+ an embedding provider, since
#     v2.12 rejects vector.enabled topic creation without one)
#   kcat + jq on PATH.
#
# Env: CHRONIK_API (default http://localhost:6092), KAFKA (default localhost:9092).
# Exit 0 = all assertions pass.
# ─────────────────────────────────────────────────────────────────────────────
set -uo pipefail

API="${CHRONIK_API:-http://localhost:6092}"
KAFKA="${KAFKA:-localhost:9092}"
NS="onto0gate"          # colon-free -> tenant == namespace
PASS=0; FAIL=0
ok()   { echo "  ✓ $1"; PASS=$((PASS+1)); }
bad()  { echo "  ✗ $1"; FAIL=$((FAIL+1)); }

for tool in kcat jq curl; do command -v "$tool" >/dev/null || { echo "missing $tool"; exit 2; }; done
[ "$(curl -s -m3 -o /dev/null -w '%{http_code}' "$API/health")" = "200" ] || { echo "broker not healthy at $API"; exit 2; }

echo "== fixture: init-namespace, ObjectType, labeled facts =="
curl -s -m15 -X POST "$API/memory/v1/admin/init-namespace" -H 'content-type: application/json' \
  -d "{\"tenant\":\"$NS\",\"agent\":\"a1\"}" >/dev/null

# NOTE: single line — kcat -P treats each newline as a separate message.
OT='{"schema_version":1,"object_type":{"type_name":"Entity","attributes":[{"name":"degree","type":"string","from_predicate":"has_degree"},{"name":"hobbies","type":"string","from_predicate":"enjoys","multi":true}],"identity":{"id_field":"subject","normalize":true},"backing":{"topic_prefix":"mem.fact","append_only":false}}}'
printf 'Entity:%s' "$OT" | kcat -P -b "$KAFKA" -t "ont.types.$NS" -K:

fact() { # key json
  printf '%s:%s' "$1" "$2" | kcat -P -b "$KAFKA" -t "mem.fact.$NS" -K:
}
fact "Alice|has_degree"    "{\"namespace\":\"$NS\",\"key\":\"Alice|has_degree\",\"valid_from\":\"2023-09-01T00:00:00Z\",\"confidence\":1.0,\"source\":{\"topic\":\"mem.raw.$NS\",\"offsets\":[10],\"extractor\":\"t@1\"},\"type\":\"fact\",\"body\":{\"subject\":\"Alice\",\"predicate\":\"has_degree\",\"object\":\"Business Administration\",\"text\":\"Alice degree Business Administration\"}}"
fact "Alice|enjoys|hiking"  "{\"namespace\":\"$NS\",\"key\":\"Alice|enjoys|hiking\",\"valid_from\":\"2024-01-01T00:00:00Z\",\"confidence\":1.0,\"source\":{\"topic\":\"mem.raw.$NS\",\"offsets\":[100],\"extractor\":\"t@1\"},\"type\":\"fact\",\"body\":{\"subject\":\"Alice\",\"predicate\":\"enjoys\",\"object\":\"hiking\",\"text\":\"Alice enjoys hiking\"}}"
fact "Alice|enjoys|chess"   "{\"namespace\":\"$NS\",\"key\":\"Alice|enjoys|chess\",\"valid_from\":\"2024-06-01T00:00:00Z\",\"confidence\":1.0,\"source\":{\"topic\":\"mem.raw.$NS\",\"offsets\":[205],\"extractor\":\"t@1\"},\"type\":\"fact\",\"body\":{\"subject\":\"Alice\",\"predicate\":\"enjoys\",\"object\":\"chess\",\"text\":\"Alice enjoys chess\"}}"
# O-1 traversal graph: Alice --works_at--> Acme --located_in--> Portugal
fact "Alice|works_at" "{\"namespace\":\"$NS\",\"key\":\"Alice|works_at\",\"valid_from\":\"2023-09-01T00:00:00Z\",\"confidence\":1.0,\"source\":{\"topic\":\"mem.raw.$NS\",\"offsets\":[300],\"extractor\":\"t@1\"},\"type\":\"fact\",\"body\":{\"subject\":\"Alice\",\"predicate\":\"works_at\",\"object\":\"Acme\",\"text\":\"Alice works at Acme\"}}"
fact "Acme|located_in" "{\"namespace\":\"$NS\",\"key\":\"Acme|located_in\",\"valid_from\":\"2023-09-01T00:00:00Z\",\"confidence\":1.0,\"source\":{\"topic\":\"mem.raw.$NS\",\"offsets\":[301],\"extractor\":\"t@1\"},\"type\":\"fact\",\"body\":{\"subject\":\"Acme\",\"predicate\":\"located_in\",\"object\":\"Portugal\",\"text\":\"Acme located in Portugal\"}}"

echo "== wait for registry + search indexing =="
for _ in $(seq 1 20); do
  curl -s "$API/ontology/v1/types?namespace=$NS" | jq -e '.types[]?|select(.type_name=="Entity")' >/dev/null 2>&1 && break
  sleep 2
done
for _ in $(seq 1 20); do
  n=$(curl -s -X POST "$API/_search" -H 'content-type: application/json' -d "{\"index\":\"mem.fact.$NS\",\"size\":20,\"query\":{\"match\":{\"_all\":\"Alice\"}}}" | jq '.hits.total.value // 0')
  [ "${n:-0}" -ge 4 ] && break
  sleep 2
done

echo "== assertions =="
# 1. registry lists Entity
curl -s "$API/ontology/v1/types?namespace=$NS" | jq -e '.types[]?|select(.type_name=="Entity")' >/dev/null \
  && ok "registry serves the Entity ObjectType" || bad "registry missing Entity"

GO=$(curl -s -m15 -X POST "$API/ontology/v1/get_object" -H 'content-type: application/json' \
  -d "{\"namespace\":\"$NS\",\"type\":\"Entity\",\"id\":\"Alice\"}")

# 2. single-valued degree
[ "$(echo "$GO" | jq -r '.attributes[]|select(.name=="degree")|.values[0]')" = "Business Administration" ] \
  && ok "get_object degree = Business Administration" || bad "degree wrong: $(echo "$GO"|jq -c '.attributes')"

# 3. multi-valued hobbies (set-equal to hiking,chess)
[ "$(echo "$GO" | jq -c '.attributes[]|select(.name=="hobbies")|.values|sort')" = '["chess","hiking"]' ] \
  && ok "get_object hobbies = [chess,hiking] (multi-valued)" || bad "hobbies wrong: $(echo "$GO"|jq -c '.attributes')"

# 4. provenance-cite gate: every attribute value carries >=1 source offset
NOCITE=$(echo "$GO" | jq '[.attributes[]|select((.provenance|map(.offsets|length)|add // 0) == 0)]|length')
[ "${NOCITE:-1}" = "0" ] && ok "provenance-cite gate: every attribute cites source offsets" \
  || bad "provenance-cite gate FAILED: $NOCITE attribute(s) lack offsets"

# 5. as_of correctness
EARLY=$(curl -s -X POST "$API/ontology/v1/get_object" -H 'content-type: application/json' \
  -d "{\"namespace\":\"$NS\",\"type\":\"Entity\",\"id\":\"Alice\",\"as_of\":\"2024-03-01T00:00:00Z\"}")
[ "$(echo "$EARLY" | jq -c '.attributes[]|select(.name=="hobbies")|.values|sort')" = '["hiking"]' ] \
  && ok "as_of 2024-03 -> hobbies = [hiking] only" || bad "as_of wrong: $(echo "$EARLY"|jq -c '.attributes')"

# 6. unknown instance -> not_found
[ "$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/ontology/v1/get_object" -H 'content-type: application/json' \
  -d "{\"namespace\":\"$NS\",\"type\":\"Entity\",\"id\":\"Nobody\"}")" = "404" ] \
  && ok "unknown instance -> 404" || bad "unknown instance not 404"

# 7. O-1 traverse: 1-hop works_at -> Acme, provenance-carrying
T1=$(curl -s -X POST "$API/ontology/v1/traverse" -H 'content-type: application/json' \
  -d "{\"namespace\":\"$NS\",\"from\":\"Alice\",\"edge_type\":\"works_at\",\"depth\":1}")
[ "$(echo "$T1" | jq -r '.edges[]|select(.edge_type=="works_at")|.to')" = "Acme" ] \
  && [ "$(echo "$T1" | jq '[.edges[]|select((.provenance|map(.offsets|length)|add // 0)>0)]|length')" -ge 1 ] \
  && ok "traverse 1-hop Alice-[works_at]->Acme with provenance" || bad "traverse 1-hop wrong: $(echo "$T1"|jq -c '.edges')"

# 8. O-1 traverse: 2-hop reaches Portugal via Acme (depth 2)
T2=$(curl -s -X POST "$API/ontology/v1/traverse" -H 'content-type: application/json' \
  -d "{\"namespace\":\"$NS\",\"from\":\"Alice\",\"edge_type\":\"*\",\"depth\":2}")
[ "$(echo "$T2" | jq -r '[.edges[]|select(.to=="Portugal" and .depth==2)]|length')" = "1" ] \
  && ok "traverse 2-hop Alice->Acme->Portugal (depth 2)" || bad "traverse 2-hop wrong: $(echo "$T2"|jq -c '.edges')"

echo
echo "== O-0 exit gate: $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
