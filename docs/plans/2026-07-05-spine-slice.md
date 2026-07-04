# Spine Slice Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One agent enrolls over a token, receives a CA-signed client cert, opens a single persistent **mTLS** `Session`, heartbeats, and shows up as `online` on a fleet page — with reconnect/self-heal on control-plane restart.

**Architecture:** The control plane runs an internal CA (persisted AES-GCM-encrypted in Postgres). It serves the agent surface over TLS with *optional* client auth: `Enroll` is token-gated and needs no client cert; `Session` requires a CA-signed client cert whose CN carries the `agent_id`. The agent generates its keypair + CSR locally (private key never leaves the guest), enrolls, then holds one bidi `Session` stream carrying `Hello` + `Heartbeat`, reconnecting with backoff+jitter. The browser surface exposes `GET /api/fleet`, rendered by a React fleet page with status badges.

**Tech Stack:** Rust workspace; `tonic` 0.14 (gRPC, `tls-ring`); `rustls`/`rcgen`/`tokio-rustls` (ring provider); `sqlx` 0.9 (Postgres, compile-time-checked); `aes-gcm` + `sha2` + `x509-parser`; React + Vite + `@e412/rnui-react`.

## Global Constraints

- **Language/build:** single Cargo workspace, edition 2021, Rust ≥ 1.82. Agent release builds target `x86_64-unknown-linux-musl` (static).
- **Crypto provider:** `ring` everywhere. Never enable `aws_lc_rs` (needs cmake). tonic gains TLS via the `tls-ring` feature only.
- **Proto is source of truth:** `crates/proto/proto/argus.proto`; codegen is protoc-free (`protox` → `tonic-prost-build`). Do not hand-edit generated code.
- **DB:** `sqlx` **compile-time-checked** queries (`query!`/`query_as!`). A reachable Postgres (via `DATABASE_URL`) OR a committed `.sqlx` offline cache is required for every `cargo build`. Migrations are embedded and run on startup.
- **Audit from day one:** every verb/identity-bearing action writes `audit_log`. In the Spine that means `agent.enroll` is audit-logged. A handler that mutates identity state without an audit row is incomplete.
- **Do NOT reopen:** (1) the two-entrypoint mTLS split — the control plane terminates mTLS itself on the agent surface, never via Traefik; (2) the boring metrics table (not touched in this slice).
- **Scope fence:** the Spine does **not** include OIDC, metrics, docker, systemd, logs, or terminal. `/api/fleet` is unauthenticated for local dev; an auth task precedes any real deployment (tracked separately, out of this plan).
- **Reference:** `docs/PRD.md` §4 (proto), §5 (handshake), §6 (schema). Match its message and column names exactly.

---

## File structure (created/modified in this slice)

```
crates/server/src/
  crypto.rs      NEW  FieldCipher: AES-256-GCM encrypt/decrypt of the CA key (env field key)
  ca.rs          MOD  CertAuthority: load_or_init (persist/decrypt), sign_csr, issue_server_cert
  repo.rs        NEW  DB access: tokens, machines upsert, agent_certs, audit, online/offline
  identity.rs    NEW  parse agent_id (CN) from a peer client-cert DER (x509-parser)
  grpc.rs        MOD  AgentService::{enroll, session}; build rustls-backed tonic server; serve()
  http.rs        MOD  add GET /api/fleet (+ FleetRow serialization)
  jobs.rs        MOD  offline sweeper: mark machines offline after missed heartbeats
  config.rs      MOD  parse field key into FieldCipher; agent-surface bind addr
  main.rs        MOD  wire CA + repo pool + grpc::serve alongside http::serve + sweeper
crates/agent/src/
  info.rs        NEW  AgentInfo gathering (machine-id, hostname, os, kernel, ip, arch)
  identity.rs    NEW  on-disk keypair+cert paths; generate keypair+CSR (rcgen); load/persist
  enroll.rs      MOD  ensure_enrolled: load identity or call Enroll over server-auth TLS
  session.rs     MOD  run: mTLS Session, Hello+Heartbeat, backoff+jitter reconnect
frontend/src/
  api.ts         NEW  typed fetch for /api/fleet
  FleetPage.tsx  NEW  fleet table with online/offline/reconnecting badges
  App.tsx        MOD  render FleetPage
docs/
  DEV.md         NEW  local dev loop: dev Postgres, env, running control-plane + agent
```

---

## Task 0: Dev environment — git, dev Postgres, sqlx tooling, deps

**Files:**
- Modify: `Cargo.toml` (workspace deps), `crates/server/Cargo.toml`, `crates/agent/Cargo.toml`
- Create: `docs/DEV.md`

**Interfaces:**
- Produces: a running Postgres reachable at `$DATABASE_URL`; `sqlx-cli` installed; new crates available to later tasks (`aes-gcm`, `sha2`, `base64`, `hex`, `x509-parser`, `rand`, `rustix`), tonic `tls-ring` enabled.

- [ ] **Step 1: Ensure a git repo exists** (commit steps below need it)

```bash
cd /home/aloks98/projects/argus
git rev-parse --is-inside-work-tree 2>/dev/null || git init
```

- [ ] **Step 2: Start a dev Postgres and export DATABASE_URL**

```bash
# Podman or Docker both work. Pin a version CNPG also ships.
docker run -d --name argus-pg -e POSTGRES_PASSWORD=argus -e POSTGRES_DB=argus \
  -p 5432:5432 postgres:17
export DATABASE_URL='postgres://postgres:argus@localhost:5432/argus'
```
Record both in `docs/DEV.md`. Expected: `docker ps` shows `argus-pg` healthy.

- [ ] **Step 3: Install sqlx-cli and apply migrations**

```bash
cargo install sqlx-cli --no-default-features --features rustls,postgres
sqlx migrate run --source crates/server/migrations
```
Expected: `0001_init` and `0002_metrics` applied; `\dt` in `psql` lists `machines`, `enrollment_tokens`, `agent_certs`, `ca_material`, `audit_log`, `metrics`.

- [ ] **Step 4: Add workspace dependencies**

In root `Cargo.toml` `[workspace.dependencies]`, add:
```toml
aes-gcm = "0.10"
sha2 = "0.10"
base64 = "0.22"
hex = "0.4"
x509-parser = "0.16"
rand = "0.9"
rustix = { version = "1", features = ["system"] }
```
Change the tonic line to enable TLS (keeps existing default features):
```toml
tonic = { version = "0.14", features = ["tls-ring"] }
```

- [ ] **Step 5: Reference the new deps in the crates**

In `crates/server/Cargo.toml` `[dependencies]` add: `aes-gcm.workspace = true`, `sha2.workspace = true`, `base64.workspace = true`, `hex.workspace = true`, `x509-parser.workspace = true`, `rand.workspace = true`.
In `crates/agent/Cargo.toml` `[dependencies]` add: `rand.workspace = true`, `rustix.workspace = true`.

- [ ] **Step 6: Verify the workspace still resolves and builds**

Run: `cargo check --workspace`
Expected: `Finished`, no errors (unused-dep warnings are fine until wired).

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "chore: dev postgres + sqlx tooling + spine deps"
```

---

## Task 1: Field encryption (AES-256-GCM)

Unit-testable in isolation. Encrypts the CA private key before it touches the DB.

**Files:**
- Create: `crates/server/src/crypto.rs`
- Modify: `crates/server/src/main.rs` (add `mod crypto;`)

**Interfaces:**
- Produces:
  - `FieldCipher::from_b64_key(b64: &str) -> anyhow::Result<FieldCipher>` (key must decode to 32 bytes)
  - `FieldCipher::encrypt(&self, plaintext: &[u8]) -> anyhow::Result<(Vec<u8> /*ciphertext*/, Vec<u8> /*12-byte nonce*/)>`
  - `FieldCipher::decrypt(&self, ciphertext: &[u8], nonce: &[u8]) -> anyhow::Result<Vec<u8>>`

- [ ] **Step 1: Write the failing test**

Add to `crates/server/src/crypto.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine};

    #[test]
    fn round_trips_plaintext() {
        let key_b64 = STANDARD.encode([7u8; 32]);
        let c = FieldCipher::from_b64_key(&key_b64).unwrap();
        let (ct, nonce) = c.encrypt(b"ca-private-key").unwrap();
        assert_ne!(ct, b"ca-private-key");
        assert_eq!(c.decrypt(&ct, &nonce).unwrap(), b"ca-private-key");
    }

    #[test]
    fn rejects_wrong_length_key() {
        let key_b64 = STANDARD.encode([7u8; 16]); // 128-bit, not allowed
        assert!(FieldCipher::from_b64_key(&key_b64).is_err());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p argus-server crypto:: 2>&1 | tail -20`
Expected: FAIL — `FieldCipher` not found.

- [ ] **Step 3: Write minimal implementation**

Prepend to `crates/server/src/crypto.rs`:
```rust
//! AES-256-GCM field encryption for the CA private key (PRD §2.3, §5).

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::{Aead, OsRng};
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};

pub struct FieldCipher {
    cipher: Aes256Gcm,
}

impl FieldCipher {
    pub fn from_b64_key(b64: &str) -> Result<Self> {
        let raw = STANDARD.decode(b64).context("field key is not valid base64")?;
        if raw.len() != 32 {
            return Err(anyhow!("field key must be 32 bytes (got {})", raw.len()));
        }
        let key = Key::<Aes256Gcm>::from_slice(&raw);
        Ok(Self { cipher: Aes256Gcm::new(key) })
    }

    pub fn encrypt(&self, plaintext: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
        let mut nonce_bytes = [0u8; 12];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ct = self
            .cipher
            .encrypt(nonce, plaintext)
            .map_err(|e| anyhow!("aes-gcm encrypt failed: {e}"))?;
        Ok((ct, nonce_bytes.to_vec()))
    }

    pub fn decrypt(&self, ciphertext: &[u8], nonce: &[u8]) -> Result<Vec<u8>> {
        let nonce = Nonce::from_slice(nonce);
        self.cipher
            .decrypt(nonce, ciphertext)
            .map_err(|e| anyhow!("aes-gcm decrypt failed: {e}"))
    }
}
```
Add `mod crypto;` to `crates/server/src/main.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p argus-server crypto:: 2>&1 | tail -20`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/crypto.rs crates/server/src/main.rs
git commit -m "feat(server): AES-256-GCM field cipher for CA key"
```

---

## Task 2: Internal CA — generate/persist/load, sign CSR, issue server cert

**SPIKE FIRST:** the exact rcgen 0.14 CSR-signing + issuer API is the linchpin of this slice. Confirm it before writing the impl.

**Files:**
- Modify: `crates/server/src/ca.rs` (replace the stub)

**Interfaces:**
- Consumes: `FieldCipher` (Task 1); `PgPool`; `Config.field_key_b64`.
- Produces:
  - `CertAuthority::load_or_init(pool: &PgPool, cipher: &FieldCipher) -> Result<CertAuthority>`
  - `CertAuthority::ca_cert_pem(&self) -> &str`
  - `CertAuthority::sign_csr(&self, csr_pem: &str, agent_id: Uuid) -> Result<SignedCert>` where
    `struct SignedCert { pub cert_pem: String, pub serial: String /*decimal*/, pub fingerprint_hex: String, pub not_before: OffsetDateTime, pub not_after: OffsetDateTime }`
  - `CertAuthority::issue_server_cert(&self, sans: &[String]) -> Result<(String /*cert_pem*/, String /*key_pem*/)>`

- [ ] **Step 1: Spike the rcgen 0.14 signing API**

Run: `cargo doc -p rcgen --no-deps` then open the `Issuer`, `CertificateSigningRequestParams`, and `CertifiedKey` items; or read <https://docs.rs/rcgen/0.14>. Confirm the exact spelling of: building an `Issuer` from a CA cert PEM + `KeyPair`, and signing a parsed CSR (`signed_by`). Write the confirmed signatures as a comment at the top of `ca.rs`. Expected: you can name the two calls precisely before coding.

- [ ] **Step 2: Write the failing test**

Replace `ca.rs` test section with:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{CertificateParams, KeyPair};
    use uuid::Uuid;
    use x509_parser::prelude::*;

    // A helper that mimics the agent making a CSR.
    fn make_csr() -> String {
        let kp = KeyPair::generate().unwrap();
        let params = CertificateParams::new(vec!["ignored".into()]).unwrap();
        params.serialize_request(&kp).unwrap().pem().unwrap()
    }

    #[test]
    fn signs_csr_with_agent_id_in_cn_and_chains_to_ca() {
        let ca = CertAuthority::self_signed_for_test();
        let agent_id = Uuid::from_u128(0x1234);
        let signed = ca.sign_csr(&make_csr(), agent_id).unwrap();

        let pem = pem::parse(signed.cert_pem.as_bytes()).unwrap();
        let (_, cert) = X509Certificate::from_der(pem.contents()).unwrap();
        let cn = cert
            .subject()
            .iter_common_name()
            .next()
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(cn, agent_id.to_string());
    }
}
```
> Note: add `pem = "3"` to server dev-deps if not transitively available; `x509-parser` is a Task 0 dep. `CertAuthority::self_signed_for_test()` is a `#[cfg(test)]` constructor you add in Step 3 that builds a CA purely in memory (no DB).

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p argus-server ca:: 2>&1 | tail -20`
Expected: FAIL — `sign_csr`/`self_signed_for_test` not found.

- [ ] **Step 4: Write the implementation**

Replace the body of `ca.rs` (keep the module doc comment). Implement, using the API confirmed in Step 1:
- an in-memory `CertAuthority { cert_pem: String, key_pem: String }`;
- `self_signed_for_test()` and a private `generate() -> (cert_pem, key_pem)` that builds a CA cert (`is_ca = IsCa::Ca(BasicConstraints::Unconstrained)`, `KeyUsagePurpose::{KeyCertSign, CrlSign}`) via `params.self_signed(&key_pair)`;
- `sign_csr`: parse CSR (`CertificateSigningRequestParams::from_pem`), set `csr.params.distinguished_name` CN = `agent_id.to_string()`, set a 365-day validity and a random serial, `signed_by(&issuer)`; compute `fingerprint_hex = hex(sha256(DER))` and read back `not_before/not_after`;
- `issue_server_cert(sans)`: build a leaf with `subject_alt_names` = `sans`, `ExtendedKeyUsagePurpose::ServerAuth`, sign with the issuer; return `(cert_pem, key_pem)`;
- `load_or_init(pool, cipher)`: `SELECT cert_pem, key_ciphertext, key_nonce FROM ca_material WHERE id = 1`. If a row exists → `cipher.decrypt` the key → construct. If not → `generate()` → `cipher.encrypt(key_pem)` → `INSERT INTO ca_material (id, cert_pem, key_ciphertext, key_nonce) VALUES (1, $1, $2, $3)` → construct. Log which path ran.

- [ ] **Step 5: Run the crypto test to verify it passes**

Run: `cargo test -p argus-server ca::tests::signs_csr 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 6: Integration-check persistence against dev Postgres**

Add a `#[sqlx::test]`-style or `#[tokio::test]` (behind `--ignored` if it needs a live DB) that calls `load_or_init` twice and asserts the second load returns the same `ca_cert_pem` (proves persistence + decrypt). Run with `DATABASE_URL` set:
```bash
cargo test -p argus-server ca:: -- --ignored 2>&1 | tail -20
```
Expected: PASS — CA is stable across loads.

- [ ] **Step 7: Commit**

```bash
git add crates/server/src/ca.rs crates/server/Cargo.toml
git commit -m "feat(server): internal CA — persist/load + sign CSR + server cert"
```

---

## Task 3: Server DB repo — tokens, machines, certs, audit, status

All queries are `sqlx::query!`/`query_as!` (compile-time-checked; `DATABASE_URL` must be set).

**Files:**
- Create: `crates/server/src/repo.rs`
- Modify: `crates/server/src/main.rs` (`mod repo;`)

**Interfaces:**
- Produces (all `async`, take `&PgPool`):
  - `consume_enrollment_token(pool, token_plain: &str) -> Result<TokenCheck>` where `enum TokenCheck { Valid { token_name: String }, Invalid }` — hashes `sha256(token_plain)`, looks up by `token_hash`, rejects revoked/expired/uses-exhausted, otherwise `UPDATE ... SET uses = uses + 1` and returns `Valid`.
  - `upsert_machine(pool, info: &AgentInfoRow) -> Result<Uuid>` — insert by `machine_id` or update inventory; returns `machines.id` (the `agent_id`).
  - `insert_agent_cert(pool, machine_id: Uuid, serial: &str, fingerprint: &str, not_before, not_after) -> Result<()>`
  - `cert_is_active(pool, fingerprint: &str) -> Result<Option<Uuid>>` — returns `machine_id` if a non-revoked, unexpired `agent_certs` row matches.
  - `mark_online(pool, machine_id: Uuid) -> Result<()>` / `touch_last_seen(pool, machine_id) -> Result<()>` / `mark_stale_offline(pool, older_than: Duration) -> Result<u64>`
  - `audit(pool, actor: &str, action: &str, machine_id: Option<Uuid>, result: &str) -> Result<()>`
  - `AgentInfoRow { machine_id, hostname, os, kernel, arch, primary_ip, agent_version }` (all `String`/`Option<String>` mirroring `proto AgentInfo`).

- [ ] **Step 1: Write failing tests (against dev Postgres)**

Create `crates/server/src/repo.rs` with `#[cfg(test)]` `#[tokio::test]` cases (marked `#[ignore]`, run with `DATABASE_URL`):
```rust
// token: insert a hashed token row, then consume_enrollment_token returns Valid once,
//        and Invalid after max_uses is exhausted / when revoked / when expired.
// machine: upsert_machine twice with same machine_id returns the SAME uuid and updates hostname.
// cert: insert_agent_cert then cert_is_active(fingerprint) returns the machine_id; revoked -> None.
// status: mark_online then mark_stale_offline(0s) flips status to 'offline' and returns count 1.
// audit: audit(...) inserts a row retrievable by action.
```
Write each as a concrete assertion (use `sqlx::query!` to seed and read back).

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p argus-server repo:: -- --ignored 2>&1 | tail -30`
Expected: FAIL — functions not defined.

- [ ] **Step 3: Implement the repo functions**

Write each function using `sqlx::query!`/`query_as!` and the exact column names from `0001_init.sql`. Hash tokens with `sha2::Sha256`. For `consume_enrollment_token`, do the check-and-increment in a single `UPDATE ... WHERE token_hash = $1 AND NOT revoked AND (expires_at IS NULL OR expires_at > now()) AND (max_uses IS NULL OR uses < max_uses) RETURNING name` so it is atomic. For `upsert_machine`, use `INSERT ... ON CONFLICT (machine_id) DO UPDATE SET hostname = EXCLUDED.hostname, ... , updated_at = now() RETURNING id`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p argus-server repo:: -- --ignored 2>&1 | tail -30`
Expected: PASS (all repo tests).

- [ ] **Step 5: Regenerate the sqlx offline cache and commit**

```bash
cargo sqlx prepare --workspace     # writes .sqlx/ so CI can build without a DB
git add crates/server/src/repo.rs crates/server/src/main.rs .sqlx
git commit -m "feat(server): DB repo for tokens, machines, certs, audit, status"
```

---

## Task 4: Enroll handler + server-auth TLS (no client cert yet)

Deliverable: a real agent (Task 5) can hit `Enroll` over server-authenticated TLS and get a signed cert.

**Files:**
- Modify: `crates/server/src/grpc.rs`, `crates/server/src/config.rs`, `crates/server/src/main.rs`

**Interfaces:**
- Consumes: `CertAuthority` (Task 2), repo fns (Task 3).
- Produces: `AgentSvc { ca: Arc<CertAuthority>, pool: PgPool }`; a working `AgentService::enroll`; `grpc::serve(cfg, svc, server_identity) -> Result<()>` that serves over TLS. `Config` gains `agent_addr` (already present) and exposes `FieldCipher`.

- [ ] **Step 1: Implement `enroll`**

In `grpc.rs`, replace the `enroll` stub: read `EnrollRequest`; `repo::consume_enrollment_token`; on `Invalid` → `audit(system, agent.enroll, None, "denied")` + `Err(Status::unauthenticated(...))`; on `Valid` → `repo::upsert_machine(info)` → `ca.sign_csr(csr_pem, agent_id)` → `repo::insert_agent_cert(...)` → `audit(token_name, agent.enroll, Some(agent_id), "ok")` → return `EnrollResponse { client_cert_pem, ca_cert_pem, agent_id: agent_id.to_string() }`.

- [ ] **Step 2: Build the TLS server**

Implement `grpc::serve`. Use tonic's `ServerTlsConfig` with the server identity from `ca.issue_server_cert(["localhost", agent-host])`:
```rust
let tls = ServerTlsConfig::new()
    .identity(Identity::from_pem(server_cert_pem, server_key_pem));
Server::builder()
    .tls_config(tls)?
    .add_service(AgentServiceServer::new(svc))
    .serve(cfg.agent_addr.parse()?)
    .await?;
```
(Client-cert verification is added in Task 6 — this step is server-auth only.)

- [ ] **Step 3: Wire into `main.rs`**

Build `FieldCipher::from_b64_key(&cfg.field_key_b64)`, `CertAuthority::load_or_init`, issue the server cert, construct `AgentSvc`, and run `grpc::serve` concurrently with `http::serve` (`tokio::try_join!`).

- [ ] **Step 4: Verify it compiles and starts**

Run: `cargo run -p argus-server` (with `DATABASE_URL`, `ARGUS_FIELD_KEY=$(head -c32 /dev/urandom | base64)`).
Expected: logs "browser HTTP surface listening" and an agent-surface listening line; no panic. Export the CA cert for the agent: add a temporary log or a one-shot `--dump-ca` path that writes `ca.crt`. (Manual E2E in Task 11 uses it.)

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/grpc.rs crates/server/src/config.rs crates/server/src/main.rs
git commit -m "feat(server): Enroll handler over server-auth TLS"
```

---

## Task 5: Agent — AgentInfo, keypair+CSR, enroll flow

**Files:**
- Create: `crates/agent/src/info.rs`, `crates/agent/src/identity.rs`
- Modify: `crates/agent/src/enroll.rs`, `crates/agent/src/main.rs`

**Interfaces:**
- Produces:
  - `info::gather(agent_version: &str) -> Result<argus_proto::v1::AgentInfo>`
  - `identity::load_or_generate_csr(data_dir: &str) -> Result<PendingIdentity>` where `PendingIdentity { key_pem, csr_pem }` (writes `agent.key` 0600 if absent)
  - `identity::persist_cert(data_dir, cert_pem, ca_pem) -> Result<()>`; `identity::load(data_dir) -> Result<Option<Identity>>`
  - `enroll::ensure_enrolled(cfg) -> Result<Identity>` (already declared; now real)

- [ ] **Step 1: Write failing unit tests for `info` and `identity`**

`info`: assert `gather` returns non-empty `hostname`, `arch`, and a `machine_id` (read from `/etc/machine-id`, fall back to a generated-and-persisted uuid under `data_dir` in tests). `identity`: `load_or_generate_csr` on an empty temp dir writes `agent.key` and returns a PEM CSR that `rcgen`/`x509-parser` can parse; a second call reuses the same key.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p argus-agent 2>&1 | tail -20` → FAIL.

- [ ] **Step 3: Implement**

- `info::gather`: `machine_id` from `/etc/machine-id`; `hostname`/`kernel`/`arch` from `rustix::system::uname()` (`nodename`/`release`/`machine`); `os` from `/etc/os-release` `PRETTY_NAME`; `primary_ip` via the UDP-connect trick (`UdpSocket::bind("0.0.0.0:0")`, `connect("192.168.150.1:80")`, `local_addr()`); `agent_version` from arg.
- `identity`: use `rcgen::KeyPair::generate()`; write `agent.key` with `0o600` (via `std::os::unix::fs::OpenOptionsExt`); build CSR with CN = machine-id using `CertificateParams::serialize_request`. `persist_cert` writes `agent.crt` + `ca.crt`. `load` returns `Identity` if both key+cert exist.

- [ ] **Step 4: Implement `ensure_enrolled`**

If `identity::load` returns `Some` → return it. Else: `load_or_generate_csr` → build a tonic `Channel` to `cfg.endpoint` with `ClientTlsConfig::new().ca_certificate(Certificate::from_pem(read(cfg.ca_cert_path)))` → `AgentServiceClient::new(channel).enroll(EnrollRequest { join_token, csr_pem, info })` → `persist_cert` → return `Identity`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p argus-agent 2>&1 | tail -20` → PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/agent/src/info.rs crates/agent/src/identity.rs crates/agent/src/enroll.rs crates/agent/src/main.rs
git commit -m "feat(agent): AgentInfo + keypair/CSR + enroll flow"
```

---

## Task 6: mTLS — optional client auth + agent_id from peer cert

**SPIKE FIRST:** confirm tonic 0.14 exposes optional client auth. Look for `ServerTlsConfig::client_auth_optional` (or `client_ca_root` + a "not required" mode) and that `Request::peer_certs()` returns the client chain. If tonic 0.14 dropped optional client auth, fall back to a manual `tokio-rustls` acceptor with `WebPkiClientVerifier::builder(roots).allow_unauthenticated().build()` fed via `Server::serve_with_incoming`. Record the confirmed path in a comment before coding.

**Files:**
- Create: `crates/server/src/identity.rs`
- Modify: `crates/server/src/grpc.rs`

**Interfaces:**
- Produces: `identity::agent_id_from_peer(certs: &[CertificateDer]) -> Result<Uuid>` (parse leaf, read CN, parse uuid).

- [ ] **Step 1: Write the failing test for `agent_id_from_peer`**

Generate a CA + client cert (reuse `CertAuthority::self_signed_for_test` + `sign_csr`), DER-encode the leaf, and assert `agent_id_from_peer` returns the same uuid that was put in the CN. Empty slice → `Err`.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p argus-server identity:: 2>&1 | tail -20` → FAIL.

- [ ] **Step 3: Implement `agent_id_from_peer`**

Parse with `x509_parser::X509Certificate::from_der`, read the CN, `Uuid::parse_str`.

- [ ] **Step 4: Enable client auth on the server**

Per the spike: add `.client_ca_root(Certificate::from_pem(ca_cert_pem))` and set optional mode on the `ServerTlsConfig` from Task 4 so `Enroll` still works without a client cert.

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p argus-server identity:: 2>&1 | tail -20` → PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/server/src/identity.rs crates/server/src/grpc.rs
git commit -m "feat(server): optional mTLS client auth + agent_id from peer cert"
```

---

## Task 7: Session handler (server) + offline sweeper

**Files:**
- Modify: `crates/server/src/grpc.rs`, `crates/server/src/jobs.rs`, `crates/server/src/main.rs`

**Interfaces:**
- Consumes: `agent_id_from_peer` (Task 6), repo status fns (Task 3).
- Produces: real `AgentService::session`; `jobs::run` marks stale machines offline every N seconds.

- [ ] **Step 1: Implement `session`**

Require a peer cert: `request.peer_certs()` → `None` ⇒ `Err(Status::unauthenticated("client cert required"))`; else `agent_id_from_peer` and confirm `repo::cert_is_active(fingerprint)` matches. Spawn a task reading the inbound `Streaming<AgentFrame>`: on `Hello` → `repo::upsert_machine` (refresh inventory) + `repo::mark_online` + `audit(agent, agent.online, ...)`; on `Heartbeat` → `repo::touch_last_seen`. Return an empty (or `HelloAck`-emitting) outbound stream — for the Spine, reply once with `ServerFrame { hello_ack }` then keep the stream open. Use `tokio_stream` + a channel for the outbound side.

- [ ] **Step 2: Implement the offline sweeper**

Replace `jobs::run`: every 10s, `repo::mark_stale_offline(Duration::from_secs(45))` (≈3 missed 15s heartbeats), logging how many flipped. This is a `tokio` interval task per the background-work rule (loss-tolerant).

- [ ] **Step 3: Wire the sweeper into `main.rs`** and add it to `try_join!`.

- [ ] **Step 4: Verify compile**

Run: `cargo check --workspace` → `Finished`.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/grpc.rs crates/server/src/jobs.rs crates/server/src/main.rs
git commit -m "feat(server): Session handler (Hello/Heartbeat) + offline sweeper"
```

---

## Task 8: Agent Session loop — Hello, Heartbeat, backoff+jitter reconnect

**Files:**
- Modify: `crates/agent/src/session.rs`

**Interfaces:**
- Consumes: `Identity` (Task 5).
- Produces: real `session::run(cfg, identity)` that never returns under normal operation (reconnect loop).

- [ ] **Step 1: Write a unit test for the backoff schedule**

Add a pure `fn next_backoff(attempt: u32) -> Duration` (exponential, capped at 30s, with ±20% jitter via `rand`) and test that: attempt 0 is < 2s, values are monotonic-ish and never exceed ~36s (cap+jitter). Jitter uses `rand::rng()`.

- [ ] **Step 2: Run to verify it fails**, then implement `next_backoff`. Run to verify pass.

- [ ] **Step 3: Implement `run`**

Loop: build an mTLS `Channel` (`ClientTlsConfig` with `ca_certificate` + `identity(Identity::from_pem(client_cert, client_key))`) to `cfg.endpoint`; open `session(stream)` with an outbound stream that first sends `AgentFrame { hello: Hello { info } }` (re-send on every reconnect so the fleet self-heals — PRD §2.5), then sends `Heartbeat` every 15s. On any error/disconnect: log, sleep `next_backoff(attempt)`, increment attempt (reset to 0 after a successful connect). Use `tokio_stream` for the outbound stream and `tokio::time::interval` for heartbeats.

- [ ] **Step 4: Verify compile**

Run: `cargo check --workspace` → `Finished`.

- [ ] **Step 5: Commit**

```bash
git add crates/agent/src/session.rs
git commit -m "feat(agent): persistent mTLS session with heartbeat + backoff reconnect"
```

---

## Task 9: Fleet HTTP API — `GET /api/fleet`

**Files:**
- Modify: `crates/server/src/http.rs`, `crates/server/src/main.rs` (pass `PgPool` into the router state)

**Interfaces:**
- Produces: `GET /api/fleet -> 200 [FleetRow]` where
  `struct FleetRow { id: Uuid, hostname: String, os: Option<String>, primary_ip: Option<String>, status: String, last_seen_at: Option<OffsetDateTime>, tags: Vec<String> }` (serde `Serialize`).

- [ ] **Step 1: Write a failing test**

`#[tokio::test] #[ignore]` (live DB): seed two machines (one `online`, one `offline`), build the router with the pool, call `/api/fleet` via `tower::ServiceExt::oneshot`, assert 200 and a 2-element JSON array with the right `status` values.

- [ ] **Step 2: Run to verify it fails.**

- [ ] **Step 3: Implement**

Add `AppState { pool: PgPool }`, convert the router to `Router<AppState>` / `with_state`, add `.route("/api/fleet", get(fleet))`. `fleet` runs `query_as!(FleetRow, "SELECT id, hostname, os, host(primary_ip) as \"primary_ip?\", status, last_seen_at, tags FROM machines ORDER BY hostname")` and returns `Json(rows)`.

- [ ] **Step 4: Run test to verify it passes.** Then `cargo sqlx prepare --workspace`.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/http.rs crates/server/src/main.rs .sqlx
git commit -m "feat(server): GET /api/fleet"
```

---

## Task 10: Fleet page (frontend)

**Files:**
- Create: `frontend/src/api.ts`, `frontend/src/FleetPage.tsx`
- Modify: `frontend/src/App.tsx`

**Interfaces:**
- Consumes: `GET /api/fleet`.
- Produces: a table of machines with a status badge per row (`online` green, `offline` gray, and a `reconnecting` treatment when a previously-online row goes stale during a poll).

- [ ] **Step 1: Implement `api.ts`**

```ts
export type FleetRow = {
  id: string; hostname: string; os: string | null; primary_ip: string | null;
  status: "pending" | "online" | "offline"; last_seen_at: string | null; tags: string[];
};
export async function getFleet(): Promise<FleetRow[]> {
  const r = await fetch("/api/fleet");
  if (!r.ok) throw new Error(`fleet ${r.status}`);
  return r.json();
}
```

- [ ] **Step 2: Implement `FleetPage.tsx`**

A component that polls `getFleet()` every 5s (`setInterval` in `useEffect`), renders a table with an `@e412/rnui-react` `Badge` (or a plain span if the component name differs — confirm against the installed package's exports) colored by `status`, and shows a "reconnecting…" hint on rows whose `last_seen_at` is older than 45s while `status !== "offline"`. Handle loading + error states.

- [ ] **Step 3: Render it from `App.tsx`.**

- [ ] **Step 4: Verify the frontend builds**

Run: `npm --prefix frontend run build`
Expected: `dist/` rebuilt with no TS/build errors.

- [ ] **Step 5: Commit**

```bash
git add frontend/src/api.ts frontend/src/FleetPage.tsx frontend/src/App.tsx
git commit -m "feat(web): fleet page with status badges"
```

---

## Task 11: End-to-end manual verification (the brief's mandate)

> "Build and manually verify this slice with a real agent before anything else layers on top." No downstream slice starts until this passes.

**Files:** none (may add `docs/DEV.md` notes).

- [ ] **Step 1: Rebuild the embedded app and the binaries**

```bash
npm --prefix frontend run build
cargo build --workspace
```

- [ ] **Step 2: Start the control plane**

```bash
export DATABASE_URL='postgres://postgres:argus@localhost:5432/argus'
export ARGUS_FIELD_KEY=$(head -c 32 /dev/urandom | base64)
export ARGUS_HTTP_ADDR=127.0.0.1:8080
export ARGUS_AGENT_ADDR=127.0.0.1:9443
cargo run -p argus-server
```
Expected: migrations applied; CA generated + persisted (log says "generated"); both surfaces listening. Retrieve the CA cert to `ca.crt` (via the `--dump-ca` path from Task 4 or by reading `ca_material.cert_pem`).

- [ ] **Step 3: Create an enrollment token**

```bash
# Until the token UI exists, insert one directly (raw token 'devtoken'):
psql "$DATABASE_URL" -c "INSERT INTO enrollment_tokens (name, token_hash) \
  VALUES ('dev', digest('devtoken','sha256'));"   # needs pgcrypto; else insert the sha256 bytes
```

- [ ] **Step 4: Run the agent against it**

```bash
export ARGUS_AGENT_ENDPOINT=https://localhost:9443
export ARGUS_JOIN_TOKEN=devtoken
export ARGUS_CA_CERT=$PWD/ca.crt
export ARGUS_DATA_DIR=$PWD/agent-data   # if you parameterized it; else /var/lib/argus
cargo run -p argus-agent
```
Expected agent logs: generated keypair/CSR → enrolled (got cert) → session connected → heartbeat. Server logs: `agent.enroll` → machine online.

- [ ] **Step 5: Confirm the fleet page**

Open `http://127.0.0.1:8080`. Expected: the machine appears with an **online** badge; `psql` shows `status='online'` and a fresh `last_seen_at`; `audit_log` has an `agent.enroll` row.

- [ ] **Step 6: Prove self-heal on control-plane restart**

Stop the control plane (Ctrl-C), wait ~20s (fleet page should show **reconnecting**, then **offline** after the sweeper window), restart it. Expected: the agent reconnects (backoff), re-sends `Hello`, and the row returns to **online** without restarting the agent. Terminal drop-on-restart is acceptable and expected.

- [ ] **Step 7: Record results in `docs/DEV.md` and commit**

```bash
git add docs/DEV.md
git commit -m "docs: spine end-to-end verification notes"
```

---

## Self-review notes (author)

- **Spec coverage vs PRD §5:** token validation (T3), CSR sign with agent_id (T2/T4), agent CSR/key-never-leaves (T5), optional client auth + per-RPC policy (T4 enroll open / T6+T7 session requires cert), online/offline + self-heal (T7/T8/T11), audit `agent.enroll` (T4). CA persistence for stateless pod (T2). Covered.
- **Explicit out-of-scope (restated so it isn't silently dropped):** OIDC on `/api/fleet`, metrics/docker/systemd/logs/terminal, cert renewal/revocation UI, self-update, k8s manifests. These are later slices/plans.
- **Two spikes are intentional**, not placeholders: rcgen 0.14 signing (T2.S1) and tonic 0.14 optional client auth (T6.S1) are the two APIs most likely to have shifted between releases; each spike has a concrete fallback.
- **sqlx offline cache** is regenerated whenever queries change (T3, T9) so CI builds without a DB.
