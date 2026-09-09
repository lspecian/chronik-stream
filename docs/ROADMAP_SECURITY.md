# Security Roadmap: TLS, SASL, ACLs, Encryption at Rest

**Audit as of 2026-09-06 (v2.12.3).**
**Phase 0 + immediate actions implemented 2026-09-09** — see §2 and §3 Phase 0, both marked
DONE below. Everything else is still open.

---

## Executive summary

| Area | Claimed | Actual | Verdict |
|------|---------|--------|---------|
| TLS (Kafka port) | — | rustls, wired to listener, mTLS-capable | **~70% real**, needs listener model + tests |
| TLS (HTTP :6092, admin) | env vars documented in CLAUDE.md | **warn-only stub; always plain HTTP** | **0% — documented but nonexistent** (docs corrected) |
| TLS (inter-node Raft/WAL) | — | plaintext | **0%** |
| SASL PLAIN | "✅ Working" | validated credentials, **never enforced** | ✅ **ENFORCED (Phase 0)** |
| SASL SCRAM-256/512 | "✅ Working" | **advertised, accepted ANY password** | ✅ **no longer advertised or accepted** |
| ACLs | — | 645-line authorizer, **zero call sites** | **0% — dead code** (principal now available) |
| Encryption at rest (WAL/segments/index/Parquet) | `wal-encryption` feature exists | **no implementation** | **0%** |
| Encryption at rest (object store SSE) | `EncryptionConfig` + builder exist | **never applied to PUT** | ✅ **applied (`CHRONIK_S3_SSE`)** |
| Encryption at rest (backups) | — | AES-256-GCM / ChaCha20-Poly1305, argon2 KDF, wired | **real** |

**Bottom line: anyone who can reach :9092 can do anything, and anyone who can reach :6092
can read every topic over plain HTTP.** TLS on the Kafka port buys encryption in transit and
nothing else.

The recurring pattern is the one that produced the replication rebuild: **code that reports
success while doing nothing.** SASL returns `error_code=0` and no caller consults the answer.

---

## 1. Findings

### 1.1 TLS — the one thing that is genuinely built

Real and working:
- [`tls.rs`](../crates/chronik-server/src/tls.rs) — rustls, PEM cert/key loading, optional
  mTLS via `WebPkiClientVerifier` (`tls.rs:142`).
- Wired to the real accept path: [`server.rs:682`](../crates/chronik-server/src/integrated_server/server.rs#L682)
  — `run()` dispatches to `run_with_tls()` when `CHRONIK_TLS_CERT`/`CHRONIK_TLS_KEY` are set.
- `MaybeTlsReadHalf`/`MaybeTlsWriteHalf` keep the plaintext and TLS paths unified downstream.

Gaps:
1. **All-or-nothing per broker.** No Kafka listener model — you cannot run
   `PLAINTEXT://:9092` and `SASL_SSL://:9093` side by side, so there is no migration path
   for an existing deployment and no separate inter-broker listener.
2. **Inter-node traffic is plaintext.** No rustls anywhere in `chronik-raft` or
   `chronik-wal`. Raft consensus, metadata replication (:9291) and follower-pull all cross
   the network unencrypted. A TLS'd client port with a plaintext replication port is not a
   secure cluster.
3. **No HTTP TLS at all.** [`admin_api.rs:1213`](../crates/chronik-server/src/admin_api.rs#L1213)
   detects `CHRONIK_ADMIN_TLS_CERT` and **only logs a warning**: *"TLS configuration
   detected but axum-server crate not available"*. The Unified API binds plain
   `axum::Server::bind` at [`unified_api/mod.rs:854`](../crates/chronik-server/src/unified_api/mod.rs#L854)
   and `:906`. **CLAUDE.md documents these TLS env vars as functional. They are not.**
4. No TLS version floor, no cipher-suite policy, no cert hot-reload (rotation = restart).
5. **Zero tests.** No test connects a real client over TLS, and none asserts that a bad
   cert is *rejected*.

### 1.2 SASL — authenticates, then discards the answer

**The enforcement gap.** [`kafka_handler.rs:1819`](../crates/chronik-server/src/kafka_handler.rs#L1819)
says it outright:

```rust
// Create a temporary authenticator for this request
// NOTE: In a full implementation, this would be per-connection state
// tracked in the connection handler. For now, we create a new one per request.
let mut authenticator = SaslAuthenticator::new();
```

There is no per-connection auth state anywhere in the server. The dispatch signature is
`handle_request(&self, request_bytes: &[u8])` ([`kafka_handler.rs:99`](../crates/chronik-server/src/kafka_handler.rs#L99))
— **it has no idea who the caller is**. A client can skip SASL entirely and issue Produce.

**SCRAM is a stub that authenticates anyone.** `continue_scram_auth` in
[`sasl.rs`](../crates/chronik-protocol/src/sasl.rs):

```rust
// For this stub, we'll accept any client-final message
// In production, this would verify the client proof
info!("SCRAM authentication successful for user: {}", scram_state.username);
self.state = SaslState::Authenticated(scram_state.username.clone());
```

The client proof is never verified. The salt is the literal string `base64::encode("salt")`,
iterations are hardcoded `i=4096`, and the server signature is
`base64::encode("server-signature")`. **And `handle_handshake` advertises
`SCRAM-SHA-256` and `SCRAM-SHA-512` as enabled mechanisms.** A strict client will reject the
bogus server signature — but the *server* has already marked the connection authenticated,
and a lenient or custom client is straight through. This is more dangerous than not
implementing SCRAM, because it is advertised.

**Other issues:**
- Hardcoded default users `admin/admin123`, `user/user123`, `kafka/kafka-secret` unless
  `CHRONIK_SASL_NO_DEFAULTS` is set (`sasl.rs:114`).
- Credentials come from `CHRONIK_SASL_USERS` as plaintext `user:pass` pairs. No hashing, no
  persistence, no cluster replication, no `kafka-configs.sh` support.
- Only PLAIN actually verifies anything. GSSAPI and OAUTHBEARER are enum variants only.
- **The test lies.** [`sasl_test_standalone.rs`](../crates/chronik-server/tests/sasl_test_standalone.rs)
  asserts `validate_connection_for_request(conn, 0).is_err()` — Produce blocked when
  unauthenticated. But it defines its *own* `ConnectionRegistry` inside the test file
  ("Simplified version of our SASL components for testing"). It tests a mock of a component
  that **does not exist in the server**. It is a green test for a security control that was
  never built. Delete it.
- `docs/PENDING_IMPLEMENTATIONS.md:559` records "SASL Auth (PLAIN, SCRAM) ✅ Working".

### 1.3 ACLs — 645 lines of dead code

- [`acl.rs`](../crates/chronik-server/src/acl.rs) is a complete-looking authorizer:
  deny-over-allow precedence, literal/prefixed patterns, super-users, host matching.
- **Zero call sites.** Grepping `AclManager|AclStore|AuthorizationResult` across the
  workspace outside `acl.rs` returns nothing. It compiles into the binary and never runs.
- The Kafka ACL APIs parse and encode correctly but return `SECURITY_DISABLED`:
  [`handler.rs:2739`](../crates/chronik-protocol/src/handler.rs#L2739) (DescribeAcls),
  `:2779` (CreateAcls), `:2819` (DeleteAcls) — *"no authorizer configured"*.
- No Produce/Fetch/consumer-group/admin path performs an authorization check.
- It could not work even if wired: **there is no principal to authorize**, because §1.2.
- No persistence and no cluster replication — `MetadataStore`
  ([`traits.rs:415+`](../crates/chronik-common/src/metadata/traits.rs#L415)) has no ACL or
  credential methods.

### 1.4 Encryption at rest — essentially none

| Data | State |
|------|-------|
| WAL segments (Tier 1, local disk) | **plaintext** |
| Raw segments (Tier 2, S3/GCS/Azure) | **plaintext** |
| Tantivy indexes (Tier 3) | **plaintext** |
| Parquet columnar files | **plaintext** |
| Metadata WAL (ChronikMetaLog) + DR uploads | **plaintext** |
| HNSW vector indexes | **plaintext** |
| Backups (`chronik-backup`) | **encrypted — genuinely implemented** |

Two dead knobs that look like features:

1. **`wal-encryption` feature.** `chronik-wal/Cargo.toml:14` declares
   `wal-encryption = ["wal", "dep:ring"]`. There is **no `use ring` anywhere in
   `chronik-wal/src`**. Enabling the feature pulls in a dependency and changes nothing.
2. **Object-store SSE.** `object_store/config.rs:228-248` defines `EncryptionConfig` and
   `EncryptionType::{Aes256, AwsKms}` with a `with_encryption()` builder. The S3 backend's
   `put_object` chain (`backends/s3.rs:191`) never calls `.server_side_encryption(...)`.
   `s3.rs:423` only *reads back* `response.server_side_encryption` on GET. Configuring
   encryption has no effect on any write.

The only real cryptography in the tree is [`chronik-backup/src/encryption.rs`](../crates/chronik-backup/src/encryption.rs)
— AES-256-GCM and ChaCha20-Poly1305 with argon2/pbkdf2 key derivation, wired into
`manager.rs:225`, `backup` is a default feature. Backups can be encrypted; live data cannot.
Note `MemoryKeyManager` holds keys in process memory — there is no KMS integration.

---

## 2. Immediate actions — ✅ DONE (2026-09-09)

These were live hazards independent of the roadmap. All five are implemented:

1. ✅ **Stopped advertising SCRAM.** `ENABLED_MECHANISMS` in `sasl.rs` is now `[PLAIN]`, and
   the fake `handle_scram_auth`/`continue_scram_auth` (hardcoded `base64("salt")`, fabricated
   server signature, client proof never verified) are **deleted** rather than left dormant.
   `handle_authenticate` refuses any non-PLAIN mechanism, so re-adding one to the list cannot
   silently reintroduce an unverified path. `kafka_handler` also had its **own** hardcoded
   mechanism list advertising SCRAM; it now reads the connection's authenticator, so there is
   one source of truth.
2. ✅ **Removed the hardcoded default users.** `admin/admin123`, `user/user123` and
   `kafka/kafka-secret` are gone. Users come only from `CHRONIK_SASL_USERS`; unset means every
   client is rejected, and the broker says so at startup.
3. ✅ **Deleted `sasl_test_standalone.rs`** and replaced it with real tests — unit tests in
   `connection.rs` and the end-to-end suite in `tests/integration/sasl_enforcement_test.rs`.
4. ✅ **Corrected the docs.** `CHRONIK_ADMIN_TLS_*` is now marked NOT IMPLEMENTED in CLAUDE.md;
   `PENDING_IMPLEMENTATIONS.md` no longer claims SCRAM works.
5. ✅ **SSE applied on object-store writes.** `S3Backend::resolve_encryption()` applies the
   configured algorithm to `put_object` **and** `create_multipart_upload` (omitting the latter
   would have left exactly the large objects — segments, snapshots — unencrypted). Configured
   with `CHRONIK_S3_SSE=AES256|aws:kms` plus optional `CHRONIK_S3_SSE_KMS_KEY_ID`. SSE-C is
   rejected with a clear error rather than half-applied, because it also requires the customer
   key on every read.

---

## 3. Implementation plan

### Phase 0 — Connection context — ✅ DONE (2026-09-09)

The single change that makes SASL real and gives ACLs something to authorize.
Implemented in [`crates/chronik-server/src/connection.rs`](../crates/chronik-server/src/connection.rs).

- `ConnectionContext { id, peer_addr, tls, sasl, auth }`, one per accepted connection,
  created in both accept loops. Requests on a connection are handled concurrently (each is
  spawned), so the mutable half sits behind a mutex.
- `AuthState`: `Disabled | Unauthenticated | Authenticated { principal, mechanism, at }`,
  with `principal()` returning Kafka's `User:name` form for Phase 3.
- `SaslConfig::from_env()` — `CHRONIK_SASL_ENABLED` (**default off**: enabling auth on a
  running cluster locks out every client at once, and there is no listener model yet to stage
  the rollout) and `CHRONIK_SASL_USERS`.
- The dispatch is now `handle_request_with_context(&self, ctx, bytes)`; the old
  `handle_request` remains as an auth-disabled shim for in-process callers and tests.
- **Pre-auth gate**, enforced in *two* places: in the dispatch (defense in depth) and — the
  one that actually protects clients — in each accept loop, **before** the handler is spawned.

**Refusal closes the connection; it does not answer.** This was not the original plan and is
worth recording: `ErrorHandler::build_error_response()` *ignores the error code* for Produce,
Fetch, Metadata and CreateTopics and emits an empty but well-formed **success** body. Had the
gate returned an error for those APIs, a refused Produce would have reached the client as
"accepted, zero results" — silent data loss dressed as success, the very failure mode this
work exists to remove. Kafka closes the connection here; so do we.

**Two bugs surfaced by running the tests, neither visible by reading:**

1. **`SaslHandshake` was encoded as a flexible response at v1**, but the Kafka spec marks it
   `"flexibleVersions": "none"`. The stray tagged-fields byte shifted the body one byte and
   librdkafka rejected every handshake with `Invalid MechanismCount 553648128` (0x21000000).
   A comment in `parser.rs` had flagged the discrepancy and left it uncorrected "because there
   is no SASL client in the test bed to prove the change" — there is now, and it fails against
   the old value. Fixed in `is_flexible_version()` and both handshake paths.
2. **`SaslAuthenticate` failures returned error code 31** (CLUSTER_AUTHORIZATION_FAILED) under
   a comment claiming it was SASL_AUTHENTICATION_FAILED, which is **58**.

*Actual: ~1 day, matching the estimate.*

### Phase 1 — SASL, production grade

- Move `SaslAuthenticator` into the connection context; delete the per-request `new()`.
- **Implement SCRAM correctly**: per-user random salt, configurable iteration count, stored
  `StoredKey`/`ServerKey`, real client-proof verification and server signature. `hmac` +
  `sha2` are already workspace deps.
- **Credential store in `ChronikMetaLog`** so credentials replicate across the cluster:
  new `MetadataStore` methods + `MetadataEvent` variants. Then implement
  `DescribeUserScramCredentials` (50) / `AlterUserScramCredentials` (51) so
  `kafka-configs.sh` works.
- Legacy `SaslHandshake` v0 path (pre-KIP-152 raw token exchange, not wrapped in
  `SaslAuthenticate`) for older clients.
- **mTLS principal extraction**: derive `User:CN=...` from the peer certificate so SSL is an
  authentication mechanism, not just encryption.
- Re-authentication / `session_lifetime_ms` (KIP-368) — currently returns a hardcoded 1h
  that nothing enforces.
- Inter-broker authentication for Raft + replication.

*Estimate: 4–6 days.*

### Phase 2 — Listener model

- `listeners=PLAINTEXT://:9092,SASL_SSL://:9093`, `advertised.listeners`,
  `listener.security.protocol.map`, `inter.broker.listener.name`.
- Metadata/DescribeCluster responses must advertise the endpoint matching the listener the
  client arrived on. (Note the v2.12.2 DescribeCluster fix — this code is version-sensitive
  for Java clients.)
- Unblocks: TLS migration without downtime, and a separately-secured inter-broker path.

*Estimate: 3–4 days. Touches cluster config + Metadata encoding.*

### Phase 3 — ACLs

- Wire `acl.rs` into dispatch behind the Phase 0 principal.
- **Map every API to (ResourceType, name, Operation)** per Kafka's authorizer table — 19
  APIs, including the non-obvious ones (`Fetch` → `Read` on topic; `JoinGroup` → `Read` on
  group; `InitProducerId` → `Write` on TransactionalId; `Metadata` → `Describe`, with
  auto-create requiring `Create` on cluster).
- Persist ACLs in the metadata store + replicate via `MetadataEvent`; back
  DescribeAcls/CreateAcls/DeleteAcls with it (replace `SECURITY_DISABLED`).
- `super.users`, `allow.everyone.if.no.acl.found`, audit logging of denials.
- **Hot-path cost is the design risk.** Produce/Fetch run at 230K msg/s single-node. An
  authorization decision per request must be a cached lookup keyed by
  `(principal, resource_type, name, op)`, not a `RwLock` scan. Benchmark it — and per
  standing practice, **measure with bytes-landed**: a denied request returns faster than a
  served one, so msg/s alone will read a broken authorizer as a speedup.

*Estimate: 5–7 days.*

### Phase 4 — Unified API (:6092)

Currently the largest hole and the easiest to overlook. `/_sql` will read **any** topic;
`/_search` and `/_vector` likewise. That is a complete bypass of whatever Kafka-side ACLs
Phase 3 adds. Only `/memory/v1/*` has tenant + API-key checks
([`unified_api/memory.rs:81`](../crates/chronik-server/src/unified_api/memory.rs#L81)).

- Add HTTPS (`axum-server` + rustls, sharing `tls.rs` config).
- Map HTTP identity → the same principal type, and run topic reads through the Phase 3
  authorizer.
- Consistent authn across `/_sql`, `/_search`, `/_vector`, `/admin`, `/subjects`.

*Estimate: 2–3 days.*

### Phase 5 — Encryption at rest

- **Quick win first:** apply SSE on object-store PUTs (see §2.5).
- **Model:** envelope encryption — per-segment DEK, KEK from a pluggable KMS (AWS KMS /
  Vault / file-backed for dev). Extend the existing `KeyManager` trait from
  `chronik-backup` rather than inventing a second one.
- **WAL encryption** is the delicate part:
  - Must not disturb the three-CRC architecture — encrypt *after* checksumming, decrypt
    *before* verification, and never touch `compressed_records_wire_bytes` (the preserved
    Kafka CRC-32C bytes that Java clients validate).
  - `WalManager::recover()` must decrypt, including for segments written under a previous
    key → key ID in the segment header, key rotation without rewriting history.
  - io_uring zero-copy write path assumes plaintext buffers; encryption adds a copy.
  - Benchmark: AES-NI runs ~1–3 GB/s/core, so this is measurable at current throughput.
- Then Tantivy indexes, Parquet files, and the metadata WAL.

*Estimate: 5–8 days, and it is separable — defer it behind Phases 0–4 unless a compliance
requirement forces it earlier.*

---

## 4. Test plan — the part that decides whether this is real

Every one of the defects above would have been caught by running something. None was
catchable by reading, and one (`sasl_test_standalone.rs`) was actively concealed by a green
test. So the deliverable for each phase is a **negative test — proof that access is denied.**

**TLS:** real client connects over TLS; wrong-CA cert **rejected**; expired cert
**rejected**; mTLS without a client cert **rejected**; plaintext client against a TLS
listener fails cleanly rather than hanging.

**SASL:** wrong password **rejected**; **Produce without authenticating rejected** (the exact
assertion the mock test faked); SCRAM with a wrong password **rejected** (this fails today);
un-enabled mechanism **rejected**; credentials survive broker restart and are visible on all
three nodes.

**ACL:** principal with no ACL **denied** on produce/fetch/create-topic; explicit deny beats
allow; prefixed pattern matches correctly; super-user bypasses; denial is audit-logged;
ACLs replicate across the cluster.

**At rest:** `grep` the raw WAL/segment/Parquet file for a known plaintext payload and find
**nothing**; recovery after restart decrypts correctly; recovery works across a key rotation.

**Clients:** Java (`kafka-console-*`, `AdminClient` from `ksql/confluent-7.5.0/`) and Rust
rdkafka. Java is the strict one — it validates server signatures and cert chains where
looser clients do not. Per project policy, no Python.

Run these on the 3-node local cluster (`tests/cluster/start.sh`), then on Thunderbird before
any release.

---

## 5. Sequencing and effort

Critical path: **0 → 1 → 3** (context → authentication → authorization). Phase 2 can land
before or after Phase 1. Phase 4 must not lag Phase 3 or the ACLs are bypassable over HTTP.
Phase 5 is independent.

| Phase | Work | Estimate |
|-------|------|----------|
| ~~Immediate fixes (§2)~~ | ~~Stop advertising fake SCRAM, drop default users, fix docs, SSE on PUT~~ | ✅ **DONE** |
| ~~0 — Connection context~~ | ~~Per-connection auth state + pre-auth gate~~ | ✅ **DONE** |
| 1 — SASL | Real SCRAM, credential store, APIs 50/51, mTLS principal | 4–6 days |
| 2 — Listener model | Multi-listener + advertised map + inter-broker listener | 3–4 days |
| 3 — ACLs | Wire authorizer, API→resource map, persistence, hot-path cache | 5–7 days |
| 4 — Unified API | HTTPS + principal mapping + authz on :6092 | 2–3 days |
| 5 — At rest | Envelope encryption, KMS, WAL integration, benchmarks | 5–8 days |
| Testing | Negative tests + Java/rdkafka matrix, throughout | ~5 days |

**≈4–6 weeks** of focused work for genuinely production-grade. A credible **minimum viable
secure broker** — TLS + enforced SASL PLAIN/SCRAM + ACLs on the Kafka port and :6092, no
at-rest encryption — is **Phases 0–4, roughly 3 weeks.**

## 6. Risks

- **Shipping green again.** The failure mode here is a passing test suite over unenforced
  controls. Negative tests are the gate, not a nice-to-have.
- **Hot-path regression.** Authorization on Produce/Fetch. Benchmark with bytes-landed.
- **Version-sensitive protocol surface.** Listener changes touch Metadata/DescribeCluster
  encoding — the area of the v2.12.2 Java-client bug. Run the 228-test protocol conformance
  suite.
- **Silent lockout.** Enabling enforcement on an existing cluster locks out every client at
  once. Phase 2's listener model is what makes a staged rollout possible; do it before
  turning enforcement on anywhere real.
