#!/usr/bin/env bash
# T2: 1,000 concurrent creates of 200 distinct names from all nodes.
# PASS: each name created exactly once, losers get TOPIC_ALREADY_EXISTS,
# no other errors, identical catalogs (topic ids included) on every node.
source "$(dirname "$0")/lib.sh"
cluster_start || { fail "cluster did not start"; finish T2; }
out=$(tool race "$BOOTS" 200 1000 3 3 t2-)
say "$out"
echo "$out" | grep -q 'created=200 ' || fail "not exactly 200 creations"
echo "$out" | grep -q 'exists=800 ' || fail "losers did not all get TOPIC_ALREADY_EXISTS"
echo "$out" | grep -q 'errors=0 ' || fail "unexpected errors"
echo "$out" | grep -q 'names_created_more_than_once=0 ' || fail "a name was created twice"
sleep 2
tool check "$BOOTS" t2- || fail "catalogs differ between nodes"
finish "T2 each name created once, identical everywhere"
