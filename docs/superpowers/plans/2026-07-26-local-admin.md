# Local Admin Account Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One local admin account with a generated password that mints an ordinary session, so the control plane stays reachable when the identity provider is not.

**Architecture:** A new `local_admin` single-row table holds an argon2id hash. `POST /auth/local` verifies it and calls the *same* `create_session` the OIDC callback uses, so expiry, revocation, logout, middleware and audit are unchanged. The load-bearing change is the boot rule: `Config.oidc` becomes `Option<OidcConfig>`, and startup requires OIDC config **or** a local admin row rather than OIDC specifically. Provisioning is a CLI subcommand (works with the server stopped) plus authenticated in-app rotation.

**Tech Stack:** `argon2` 0.5 (RustCrypto, pure Rust), existing `rand` for password generation, `sqlx` for the table, existing session/cookie/middleware machinery from the OIDC slice.

**Design of record:** `docs/superpowers/specs/2026-07-26-local-admin-design.md`. Read it before starting; this plan implements it and does not restate its rationale.

## Global Constraints

- **`ring` everywhere. Never `aws-lc-rs`, never OpenSSL, never cmake.** `argon2` is pure Rust; re-verify in the workspace anyway.
- **The agent crate gains no dependency.** `argus-agent` must still build static for `x86_64-unknown-linux-musl`.
- **The password is always generated, never chosen.** There is no "set this specific password" path anywhere, including the CLI. Rotation generates.
- **Only the argon2id PHC string is stored.** Never the password, never a reversible form, never in a log.
- **No permanent lockout.** Rate limiting escalates to a cap and stops. A hard lock is a denial of service on the one credential that exists to rescue the operator.
- **No unauthenticated write path.** There is no first-run setup page. The only unauthenticated endpoint this slice adds is `POST /auth/local`, which verifies a credential and creates nothing.
- **Wrong username, wrong password, and no-admin-configured are indistinguishable** to the caller, in both response and timing.
- **The CLI must run without OIDC configuration.** It loads only `ARGUS_DATABASE_URL`. A recovery command that requires the thing that broke is not a recovery command.
- **Every verb writes `audit_log`.** `auth.login` / `auth.denied` carry `detail = {"method":"local"}`.
- **Migrations are embedded and run on startup.**
- **New `sqlx::query!` requires `cargo sqlx prepare --workspace -- --all-targets`** with `.sqlx` committed. `cargo check` passing does NOT prove the cache is current — force it with `touch <file> && SQLX_OFFLINE=true cargo check`.
- **Host-dependent tests must be named `live_*`.** Do not run `cargo test --workspace -- --ignored` casually — it rotates the dev CA (`docs/DEV.md:611`).

## File Structure

| File | Responsibility |
|---|---|
| `crates/server/src/auth/password.rs` (create) | Generation, argon2id hashing/verification, the dummy hash. Pure, no I/O. |
| `crates/server/src/auth/ratelimit.rs` (create) | In-memory global limiter: bucket + escalating capped delay. Pure logic + a clock. |
| `crates/server/src/auth/local.rs` (create) | `POST /auth/local`, the rotate endpoint, and the CLI's shared reset routine. |
| `crates/server/migrations/0005_local_admin.sql` (create) | The single-row table. |
| `crates/server/src/repo.rs` (modify) | `local_admin` CRUD + existence check. |
| `crates/server/src/config.rs` (modify) | `oidc: Option<OidcConfig>`; partial-config rejection. |
| `crates/server/src/main.rs` (modify) | CLI dispatch before config load; the boot rule. |
| `crates/server/src/http.rs` (modify) | Mount the routes; `AppState.oidc` becomes optional. |
| `crates/server/src/auth/oidc.rs` (modify) | Handle absent OIDC config; add `{"method":"oidc"}` to its audit. |
| `frontend/src/components/SignIn.tsx` (modify) | Collapsed local-account disclosure. |
| `frontend/src/components/RotateLocalAdmin.tsx` (create) | Authenticated rotation dialog, shows the password once. |

---

### Task 1: Password generation and hashing

**Files:**
- Modify: `Cargo.toml`, `crates/server/Cargo.toml`
- Create: `crates/server/src/auth/password.rs`
- Modify: `crates/server/src/auth/mod.rs`

**Interfaces:**
- Produces: `generate_password() -> String`; `hash_password(&str) -> anyhow::Result<String>`; `verify_password(password: &str, phc: &str) -> bool`; `DUMMY_PHC: &str`; `verify_against_dummy(password: &str)`.

- [ ] **Step 1: Add the dependency and prove the gate**

In `Cargo.toml` under `[workspace.dependencies]`:

```toml
# Local admin password hashing. Pure Rust (no -sys crates, no OpenSSL, no cmake).
argon2 = { version = "0.5", features = ["std"] }
```

In `crates/server/Cargo.toml`, beside the other auth deps: `argon2.workspace = true`

Then verify in the real workspace, where feature unification can differ from a scratch crate:

```bash
cargo tree -e normal -p argus-server | grep -iE "openssl-sys|aws-lc|\bopenssl v" || echo "GATE OK"
cargo build -p argus-agent --target x86_64-unknown-linux-musl 2>&1 | tail -2
```

Expected: `GATE OK` (an `openssl-probe` line is pre-existing and fine — pure Rust, links nothing), and the agent still builds. If `aws-lc-rs` or `openssl-sys` appears, STOP and report BLOCKED.

- [ ] **Step 2: Write the failing tests**

Create `crates/server/src/auth/password.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_passwords_are_long_and_unique() {
        let a = generate_password();
        let b = generate_password();
        assert_ne!(a, b, "every generated password must be fresh");
        assert_eq!(a.chars().count(), PASSWORD_LEN);
        // Every character must come from the declared alphabet -- a bug that
        // silently narrowed it would otherwise pass unnoticed.
        assert!(a.chars().all(|c| PASSWORD_ALPHABET.contains(c)));
    }

    #[test]
    fn hash_verifies_the_right_password_and_rejects_others() {
        let phc = hash_password("correct horse battery staple").expect("hash");
        // A PHC string, not the password: a bug that stored the plaintext must fail here.
        assert!(phc.starts_with("$argon2id$"), "expected argon2id PHC, got {phc}");
        assert!(!phc.contains("correct horse"));

        assert!(verify_password("correct horse battery staple", &phc));
        assert!(!verify_password("wrong", &phc));
        assert!(!verify_password("", &phc));
    }

    #[test]
    fn each_hash_uses_a_fresh_salt() {
        let a = hash_password("same").expect("hash");
        let b = hash_password("same").expect("hash");
        assert_ne!(a, b, "identical passwords must not produce identical hashes");
        assert!(verify_password("same", &a) && verify_password("same", &b));
    }

    #[test]
    fn verify_rejects_a_malformed_phc_instead_of_panicking() {
        assert!(!verify_password("anything", "not-a-phc-string"));
        assert!(!verify_password("anything", ""));
    }

    #[test]
    fn the_dummy_hash_is_valid_and_matches_nothing_usable() {
        // The no-admin-configured path verifies against this so that response
        // timing cannot distinguish "no local admin exists" from "wrong
        // password". If it were malformed, verification would return early and
        // the timing signal would reappear.
        assert!(DUMMY_PHC.starts_with("$argon2id$"));
        assert!(!verify_password("", DUMMY_PHC));
        assert!(!verify_password("admin", DUMMY_PHC));
    }
}
```

- [ ] **Step 3: Run to verify they fail**

Add `pub mod password;` to `crates/server/src/auth/mod.rs`.

Run: `cargo test -p argus-server auth::password`
Expected: FAIL — `cannot find function generate_password in this scope`.

- [ ] **Step 4: Implement**

Prepend to `crates/server/src/auth/password.rs`:

```rust
//! Local admin password generation and argon2id hashing.
//!
//! Passwords are always generated, never chosen (design §7): 24 random
//! characters is arithmetically unguessable online, which is what demotes rate
//! limiting to a backstop rather than the primary control.

use anyhow::{anyhow, Result};
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use rand::Rng;

pub const PASSWORD_LEN: usize = 24;

/// Deliberately excludes visually ambiguous characters (0/O, 1/l/I). This is a
/// credential a human transcribes from a terminal under pressure during an
/// outage, and a misread character is indistinguishable from a wrong password.
pub const PASSWORD_ALPHABET: &str =
    "ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";

pub fn generate_password() -> String {
    let alphabet: Vec<char> = PASSWORD_ALPHABET.chars().collect();
    let mut rng = rand::rng();
    (0..PASSWORD_LEN)
        .map(|_| alphabet[rng.random_range(0..alphabet.len())])
        .collect()
}

pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow!("argon2 hash failed: {e}"))
}

/// Returns false for a malformed stored hash rather than propagating: a corrupt
/// row must deny the login, never crash the handler or admit the caller.
pub fn verify_password(password: &str, phc: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// A real argon2id hash of a value nobody can present, used when no local admin
/// row exists so the handler still pays the verification cost. Without this,
/// "no local admin configured" returns in microseconds while a wrong password
/// takes ~100ms, and that difference tells an attacker whether the credential
/// exists at all.
pub const DUMMY_PHC: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$8Wq0nWCXwx6d0Ck3S3ZQMlR0LZ8fA0Z8m5xE7QqjZ2s";

pub fn verify_against_dummy(password: &str) {
    let _ = verify_password(password, DUMMY_PHC);
}
```

> **Implementer note:** `DUMMY_PHC` above must be a *real* argon2id hash or the
> timing-equalisation is a no-op. Generate one locally
> (`hash_password("unused")`), paste the result, and confirm
> `the_dummy_hash_is_valid_and_matches_nothing_usable` passes. Do not ship the
> literal above unverified.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p argus-server auth::password`
Expected: PASS, 5 tests.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/server/Cargo.toml crates/server/src/auth
git commit -m "feat(auth): local admin password generation and argon2id hashing

Passwords are always generated, never chosen. The alphabet excludes
visually ambiguous characters because this is transcribed by a human
during an outage, where a misread character is indistinguishable from a
wrong password. A real dummy hash exists so the no-admin path pays the
same verification cost."
```

---

### Task 2: The `local_admin` table

**Files:**
- Create: `crates/server/migrations/0005_local_admin.sql`
- Modify: `crates/server/src/repo.rs`

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces: `repo::LocalAdmin { username: String, password_hash: String }`; `repo::get_local_admin(&PgPool) -> Result<Option<LocalAdmin>>`; `repo::upsert_local_admin(&PgPool, &str, &str) -> Result<()>`; `repo::local_admin_exists(&PgPool) -> Result<bool>`; `repo::touch_local_admin_login(&PgPool) -> Result<()>`.

- [ ] **Step 1: Write the migration**

Create `crates/server/migrations/0005_local_admin.sql`:

```sql
-- The break-glass credential (design §6). At most ONE row, enforced by the
-- schema rather than by application discipline: `id` is a boolean primary key
-- that may only be true, so a second insert collides on the primary key.
--
-- Only the argon2id PHC string is stored. `last_login_at` exists to make use of
-- the break-glass credential visible -- it is meant to be the exception, and
-- noticing it being used routinely is the cheapest signal available.
create table local_admin (
    id            boolean     primary key default true,
    username      text        not null,
    password_hash text        not null,
    created_at    timestamptz not null default now(),
    updated_at    timestamptz not null default now(),
    last_login_at timestamptz,
    constraint local_admin_single_row check (id)
);
```

- [ ] **Step 2: Write the failing tests**

Append to `crates/server/src/repo.rs`'s `mod tests`:

```rust
    #[sqlx::test]
    async fn local_admin_round_trips_and_rotation_updates_the_hash(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        assert!(!local_admin_exists(&pool).await?, "starts absent");
        assert!(get_local_admin(&pool).await?.is_none());

        upsert_local_admin(&pool, "admin", "$argon2id$first").await?;
        assert!(local_admin_exists(&pool).await?);
        let a = get_local_admin(&pool).await?.expect("row");
        assert_eq!(a.username, "admin");
        assert_eq!(a.password_hash, "$argon2id$first");

        // Rotation replaces the hash in place rather than adding a row.
        upsert_local_admin(&pool, "admin", "$argon2id$second").await?;
        let b = get_local_admin(&pool).await?.expect("row");
        assert_eq!(b.password_hash, "$argon2id$second");

        let count = sqlx::query_scalar!("SELECT count(*) FROM local_admin")
            .fetch_one(&pool)
            .await?
            .unwrap_or(0);
        assert_eq!(count, 1, "rotation must not create a second row");
        Ok(())
    }

    #[sqlx::test]
    async fn a_second_local_admin_row_is_rejected_by_the_schema(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        upsert_local_admin(&pool, "admin", "$argon2id$x").await?;
        // Bypass the repo helper to prove the SCHEMA enforces single-row, not
        // just our upsert. Without the constraint this insert would succeed.
        let second = sqlx::query!(
            "INSERT INTO local_admin (id, username, password_hash) VALUES (true, 'other', 'y')"
        )
        .execute(&pool)
        .await;
        assert!(second.is_err(), "the schema must reject a second row");
        Ok(())
    }

    #[sqlx::test]
    async fn touch_login_stamps_last_login_at(pool: PgPool) -> anyhow::Result<()> {
        upsert_local_admin(&pool, "admin", "$argon2id$x").await?;
        let before = sqlx::query_scalar!("SELECT last_login_at FROM local_admin")
            .fetch_one(&pool)
            .await?;
        assert!(before.is_none(), "not stamped until a login happens");

        touch_local_admin_login(&pool).await?;
        let after = sqlx::query_scalar!("SELECT last_login_at FROM local_admin")
            .fetch_one(&pool)
            .await?;
        assert!(after.is_some());
        Ok(())
    }
```

- [ ] **Step 3: Run to verify they fail**

Run: `cargo test -p argus-server repo::tests::local_admin repo::tests::a_second_local repo::tests::touch_login`
Expected: FAIL — `cannot find function local_admin_exists`.

- [ ] **Step 4: Implement**

In `crates/server/src/repo.rs`:

```rust
pub struct LocalAdmin {
    pub username: String,
    pub password_hash: String,
}

pub async fn get_local_admin(pool: &PgPool) -> Result<Option<LocalAdmin>> {
    let row = sqlx::query!("SELECT username, password_hash FROM local_admin WHERE id = true")
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| LocalAdmin {
        username: r.username,
        password_hash: r.password_hash,
    }))
}

/// Create or rotate. `ON CONFLICT` on the single-row primary key makes this an
/// in-place rotation rather than an accumulation of credentials.
pub async fn upsert_local_admin(pool: &PgPool, username: &str, password_hash: &str) -> Result<()> {
    sqlx::query!(
        "INSERT INTO local_admin (id, username, password_hash) VALUES (true, $1, $2)
         ON CONFLICT (id) DO UPDATE SET username = $1, password_hash = $2, updated_at = now()",
        username,
        password_hash,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Used by the boot rule: the control plane may start without OIDC config only
/// if this returns true.
pub async fn local_admin_exists(pool: &PgPool) -> Result<bool> {
    let n = sqlx::query_scalar!("SELECT count(*) FROM local_admin")
        .fetch_one(pool)
        .await?
        .unwrap_or(0);
    Ok(n > 0)
}

pub async fn touch_local_admin_login(pool: &PgPool) -> Result<()> {
    sqlx::query!("UPDATE local_admin SET last_login_at = now() WHERE id = true")
        .execute(pool)
        .await?;
    Ok(())
}
```

- [ ] **Step 5: Refresh the sqlx cache and test**

```bash
export DATABASE_URL='postgres://postgres:argus@localhost:5432/argus'
cargo sqlx prepare --workspace -- --all-targets
cargo test -p argus-server -- repo::tests::local_admin repo::tests::a_second_local repo::tests::touch_login
touch crates/server/src/repo.rs && SQLX_OFFLINE=true cargo check -p argus-server --all-targets
```

Expected: tests PASS; the offline check succeeds with no "no cached data" error.

- [ ] **Step 6: Commit**

```bash
git add crates/server/migrations/0005_local_admin.sql crates/server/src/repo.rs .sqlx
git commit -m "feat(auth): single-row local_admin table

At most one row is a schema guarantee, not application discipline: a
boolean primary key that may only be true. A test bypasses the repo
helper to prove the schema enforces it rather than the upsert."
```

---

### Task 3: OIDC config becomes optional, and the boot rule

This is the load-bearing task. It is what makes the feature a real recovery path
rather than half of one.

**Files:**
- Modify: `crates/server/src/config.rs`, `crates/server/src/main.rs`, `crates/server/src/http.rs`, `crates/server/src/auth/oidc.rs`

**Interfaces:**
- Consumes: `repo::local_admin_exists` (Task 2).
- Produces: `Config.oidc: Option<OidcConfig>`; `AppState.oidc: Option<Arc<OidcConfig>>`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/server/src/config.rs`'s `mod tests`:

```rust
    use super::oidc_from_env_values;

    /// All five present -> configured.
    #[test]
    fn complete_oidc_config_is_loaded() {
        let got = oidc_from_env_values(
            Some("https://idp.example".into()),
            Some("cid".into()),
            Some("secret".into()),
            Some("any".into()),
            Some("https://argus.example".into()),
            None,
            None,
            None,
        )
        .expect("complete config must load");
        assert!(got.is_some());
    }

    /// None present -> not configured, and NOT an error: this is the state a
    /// local-admin-only deployment boots in.
    #[test]
    fn absent_oidc_config_is_none_not_an_error() {
        let got = oidc_from_env_values(None, None, None, None, None, None, None, None)
            .expect("absent config is a valid state");
        assert!(got.is_none());
    }

    /// Partially present -> hard error. A half-configured IdP is a mistake, not
    /// a mode: silently treating it as "not configured" would let a typo in one
    /// variable quietly disable SSO for everyone.
    #[test]
    fn partial_oidc_config_is_rejected() {
        let err = oidc_from_env_values(
            Some("https://idp.example".into()),
            Some("cid".into()),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect_err("partial config must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("ARGUS_OIDC_CLIENT_SECRET"),
            "the error must name a missing variable, got: {msg}"
        );
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p argus-server config::tests`
Expected: FAIL — `cannot find function oidc_from_env_values`.

- [ ] **Step 3: Implement the optional config**

In `crates/server/src/config.rs`, change `Config.oidc` to `Option<OidcConfig>` and add:

```rust
/// Build the OIDC settings from already-read values, so the all-or-nothing rule
/// is testable without mutating process env (racy in parallel tests).
///
/// Three outcomes, and the middle one is the point:
///   - all five required values present -> `Ok(Some(_))`
///   - none present                     -> `Ok(None)`, a valid state for a
///     local-admin-only deployment (design §4)
///   - some present                     -> `Err`, naming the first missing one.
///     A half-configured provider is a mistake, not a mode: treating it as
///     "not configured" would let one typo silently disable SSO for everyone.
#[allow(clippy::too_many_arguments)]
fn oidc_from_env_values(
    issuer: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    required_role: Option<String>,
    public_url: Option<String>,
    roles_claim: Option<String>,
    scopes: Option<String>,
    ca_cert_path: Option<String>,
) -> Result<Option<OidcConfig>> {
    use argus_common::env;
    let required = [
        (env::OIDC_ISSUER, &issuer),
        (env::OIDC_CLIENT_ID, &client_id),
        (env::OIDC_CLIENT_SECRET, &client_secret),
        (env::OIDC_REQUIRED_ROLE, &required_role),
        (env::PUBLIC_URL, &public_url),
    ];
    let present = required.iter().filter(|(_, v)| v.is_some()).count();
    if present == 0 {
        return Ok(None);
    }
    if present < required.len() {
        let missing = required
            .iter()
            .find(|(_, v)| v.is_none())
            .map(|(k, _)| *k)
            .unwrap_or("");
        return Err(anyhow::anyhow!(
            "OIDC is partially configured: {missing} is missing. Set all five \
             OIDC variables, or none of them to run with only a local admin."
        ));
    }
    let take = |k: &str, v: Option<String>| -> Result<String> { reject_empty(k, v.unwrap()) };
    Ok(Some(OidcConfig {
        issuer: take(env::OIDC_ISSUER, issuer)?,
        client_id: take(env::OIDC_CLIENT_ID, client_id)?,
        client_secret: take(env::OIDC_CLIENT_SECRET, client_secret)?,
        required_role: parse_required_role(&take(env::OIDC_REQUIRED_ROLE, required_role)?),
        roles_claim: roles_claim.unwrap_or_else(|| "groups".into()),
        scopes: parse_scopes(scopes.as_deref()),
        public_url: take(env::PUBLIC_URL, public_url)?,
        ca_cert_path,
    }))
}
```

`Config::from_env` calls it with `std::env::var(...).ok()` for each.

- [ ] **Step 4: Thread the Option through the server**

- `AppState.oidc` becomes `Option<Arc<OidcConfig>>`, and `AppState` gains nothing else.
- `OidcClient` is constructed only when config is present; `AppState` carries `Option<Arc<OidcClient>>`.
- `/auth/login` and `/auth/callback` return `error_page(StatusCode::NOT_FOUND, "Single sign-on is not configured on this server.")` when it is `None`. **They must not panic or unwrap.**
- `session_cookie` / `flow_cookie` currently take `&OidcConfig` for `cookie_secure()`. Move that decision to a `Config`-level value so cookies work with no OIDC config — the local login must set a cookie in exactly that state. Add `Config.public_url: String` (required always, independent of OIDC) and derive `cookie_secure` from it, with `OidcConfig` reading the same value.

> **Implementer note:** `ARGUS_PUBLIC_URL` moving out of `OidcConfig` and becoming
> always-required is a deliberate consequence: the cookie's `Secure` attribute
> and the OIDC `redirect_uri` both derive from it, and only the second is
> OIDC-specific. Update the OIDC design doc's §5 table in the same commit so the
> two specs do not disagree.

- [ ] **Step 5: Implement the boot rule**

In `crates/server/src/main.rs`, after `db::migrate`:

```rust
    // Design §4: the invariant is "authentication is configured", not "OIDC is
    // configured". Booting with neither would serve the browser surface -- and
    // a root PTY on every machine -- to anyone who can reach the port.
    if cfg.oidc.is_none() && !repo::local_admin_exists(&pool).await? {
        anyhow::bail!(
            "no authentication is configured: set the OIDC variables, or create \
             a local admin with `argus local-admin reset`"
        );
    }
```

- [ ] **Step 6: Run the gates**

```bash
cargo test -p argus-server
SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings
```

Expected: PASS and clean. Existing OIDC tests must still pass — `test_state` needs its `oidc` field wrapped in `Some(...)`.

- [ ] **Step 7: Commit**

```bash
git add crates/server/src docs/superpowers/specs
git commit -m "feat(auth): OIDC config becomes optional; boot requires any auth method

The rule was never 'OIDC must be configured' -- it was 'authentication
must be configured', and stating it the first way was an
overspecification. Without this the local admin would rescue an
unreachable IdP but not a lost client secret, since the process would
refuse to start at all.

Partial OIDC config is a hard error rather than treated as absent: one
typo must not silently disable SSO for everyone."
```

---

### Task 4: The CLI subcommand

**Files:**
- Create/modify: `crates/server/src/auth/local.rs`, `crates/server/src/main.rs`

**Interfaces:**
- Consumes: `password::{generate_password, hash_password}`, `repo::upsert_local_admin`.
- Produces: `auth::local::reset_local_admin(&PgPool, &str) -> Result<String>` returning the generated password.

- [ ] **Step 1: Write the failing test**

In `crates/server/src/auth/local.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::PgPool;

    #[sqlx::test]
    async fn reset_generates_a_working_password_and_stores_only_its_hash(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let password = reset_local_admin(&pool, "admin").await?;
        assert_eq!(password.chars().count(), crate::auth::password::PASSWORD_LEN);

        let row = crate::repo::get_local_admin(&pool).await?.expect("row");
        assert_eq!(row.username, "admin");
        // The stored value must be a hash, and must not contain the password.
        assert!(row.password_hash.starts_with("$argon2id$"));
        assert!(!row.password_hash.contains(&password));
        assert!(crate::auth::password::verify_password(&password, &row.password_hash));

        // Rotation issues a different password and invalidates the old one.
        let second = reset_local_admin(&pool, "admin").await?;
        assert_ne!(password, second);
        let row2 = crate::repo::get_local_admin(&pool).await?.expect("row");
        assert!(crate::auth::password::verify_password(&second, &row2.password_hash));
        assert!(
            !crate::auth::password::verify_password(&password, &row2.password_hash),
            "the previous password must stop working after rotation"
        );
        Ok(())
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p argus-server auth::local`
Expected: FAIL — `cannot find function reset_local_admin`.

- [ ] **Step 3: Implement the shared routine**

```rust
/// Generate a new password, store only its hash, and return the password for
/// one-time display. Shared by the CLI and the in-app rotation endpoint so
/// there is exactly one implementation of "rotate".
pub async fn reset_local_admin(pool: &PgPool, username: &str) -> anyhow::Result<String> {
    let password = crate::auth::password::generate_password();
    let hash = crate::auth::password::hash_password(&password)?;
    crate::repo::upsert_local_admin(pool, username, &hash).await?;
    Ok(password)
}
```

- [ ] **Step 4: Wire the CLI**

In `crates/server/src/main.rs`, before any config load:

```rust
    // CLI dispatch happens BEFORE `Config::from_env`, and loads only the
    // database URL. A recovery command that requires the configuration that
    // broke is not a recovery command -- `argus local-admin reset` has to work
    // when the OIDC variables are absent or wrong, which is exactly when it is
    // needed. One subcommand does not justify an argument-parsing dependency.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("local-admin") {
        return run_local_admin_cli(&args).await;
    }
```

`run_local_admin_cli` accepts `reset [--username <name>]` (default `admin`), connects using `ARGUS_DATABASE_URL` only, calls `reset_local_admin`, and prints:

```
Local admin created.

  username: admin
  password: <generated>

This password is shown ONCE and is not recoverable. Store it now.
Run this command again to issue a new one.
```

Any other subcommand prints usage and exits non-zero.

- [ ] **Step 5: Verify the CLI runs without OIDC config**

```bash
env -u ARGUS_OIDC_ISSUER -u ARGUS_OIDC_CLIENT_ID -u ARGUS_OIDC_CLIENT_SECRET \
    -u ARGUS_OIDC_REQUIRED_ROLE -u ARGUS_PUBLIC_URL \
    cargo run -p argus-server -- local-admin reset
```

Expected: prints a username and password. **If this errors about a missing OIDC variable, the split in Step 4 is wrong** — that is the single most important check in this task.

- [ ] **Step 6: Commit**

```bash
git add crates/server/src
git commit -m "feat(auth): argus local-admin reset

Dispatched before config load and loads only ARGUS_DATABASE_URL: a
recovery command that requires the configuration that broke is not a
recovery command. One implementation of rotate, shared with the in-app
endpoint."
```

---

### Task 5: The rate limiter

**Files:**
- Create: `crates/server/src/auth/ratelimit.rs`

**Interfaces:**
- Produces: `LoginLimiter::new() -> Self`; `LoginLimiter::check(&self, now: Instant) -> Option<Duration>` (None = allowed, Some = retry-after); `LoginLimiter::record_failure(&self, now: Instant)`; `LoginLimiter::record_success(&self)`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn allows_until_the_burst_is_spent_then_delays() {
        let l = LoginLimiter::new();
        let t0 = Instant::now();
        for i in 0..BURST {
            assert!(l.check(t0).is_none(), "attempt {i} within burst must be allowed");
            l.record_failure(t0);
        }
        assert!(l.check(t0).is_some(), "the attempt after the burst must be delayed");
    }

    #[test]
    fn the_delay_escalates_but_is_capped() {
        let l = LoginLimiter::new();
        let t0 = Instant::now();
        for _ in 0..(BURST + 20) {
            l.record_failure(t0);
        }
        let d = l.check(t0).expect("should be delaying");
        assert!(d <= MAX_DELAY, "delay {d:?} must not exceed the cap {MAX_DELAY:?}");
    }

    /// A hard lockout would be a denial of service on the one credential that
    /// exists to rescue the operator. However many failures occur, waiting must
    /// eventually allow another attempt.
    #[test]
    fn no_sequence_of_failures_produces_a_permanent_lock() {
        let l = LoginLimiter::new();
        let t0 = Instant::now();
        for _ in 0..10_000 {
            l.record_failure(t0);
        }
        let later = t0 + MAX_DELAY + Duration::from_secs(1);
        assert!(
            l.check(later).is_none(),
            "after waiting out the capped delay, an attempt must be allowed"
        );
    }

    #[test]
    fn success_clears_the_penalty() {
        let l = LoginLimiter::new();
        let t0 = Instant::now();
        for _ in 0..(BURST + 5) {
            l.record_failure(t0);
        }
        assert!(l.check(t0).is_some());
        l.record_success();
        assert!(l.check(t0).is_none(), "a successful login resets the limiter");
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p argus-server auth::ratelimit`
Expected: FAIL — `cannot find type LoginLimiter`.

- [ ] **Step 3: Implement**

A `Mutex`-guarded struct holding `consecutive_failures: u32` and `last_attempt: Option<Instant>`. `BURST = 5`; delay is `min(2^(failures - BURST) seconds, MAX_DELAY)` with `MAX_DELAY = Duration::from_secs(30)`. `check` returns `None` when `failures < BURST` or when `now - last_attempt >= delay`, else `Some(remaining)`. `record_success` zeroes the counter.

Document at the top: global rather than per-IP (behind a proxy the peer address is the proxy's, and trusting `X-Forwarded-For` means trusting a client-controlled header an attacker rotates freely); in memory rather than a table (one replica, and an attacker cannot force the restart that would reset it); capped rather than locking (design §10.2).

- [ ] **Step 4: Run the tests**

Run: `cargo test -p argus-server auth::ratelimit`
Expected: PASS, 4 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/auth/ratelimit.rs
git commit -m "feat(auth): global login rate limiter with a capped delay

Global rather than per-IP: behind a proxy the peer address is the
proxy's, and trusting X-Forwarded-For means trusting a header an
attacker rotates freely. Capped rather than locking: a hard lock is a
denial of service on the one credential that exists to rescue the
operator."
```

---

### Task 6: `POST /auth/local` and the rotate endpoint

**Files:**
- Modify: `crates/server/src/auth/local.rs`, `crates/server/src/http.rs`, `crates/server/src/auth/oidc.rs`

**Interfaces:**
- Consumes: everything from Tasks 1-5, plus `repo::create_session`, `auth::session::new_session_token`, the session cookie builder, `repo::audit_with_detail`.
- Produces: `POST /auth/local` (public), `POST /api/local-admin/rotate` (behind the auth middleware).

- [ ] **Step 1: Write the failing router tests**

In `crates/server/src/http.rs`'s tests, using the existing `test_state` helper:

```rust
    #[sqlx::test]
    async fn local_login_rejects_bad_credentials_without_setting_a_cookie(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let hash = crate::auth::password::hash_password("the-real-one")?;
        repo::upsert_local_admin(&pool, "admin", &hash).await?;

        let res = router(test_state(pool.clone()))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auth/local")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"username":"admin","password":"wrong"}"#))?,
            )
            .await?;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert!(
            res.headers().get("set-cookie").is_none(),
            "a failed login must not set a session cookie"
        );
        Ok(())
    }

    #[sqlx::test]
    async fn local_login_with_correct_credentials_sets_a_working_session(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let hash = crate::auth::password::hash_password("the-real-one")?;
        repo::upsert_local_admin(&pool, "admin", &hash).await?;
        let app = router(test_state(pool.clone()));

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auth/local")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"username":"admin","password":"the-real-one"}"#))?,
            )
            .await?;
        assert_eq!(res.status(), StatusCode::OK);
        let cookie = res
            .headers()
            .get("set-cookie")
            .expect("session cookie")
            .to_str()?
            .to_string();
        assert!(cookie.contains("HttpOnly"), "session cookie must be HttpOnly");

        // The session must resolve through the SAME middleware OIDC sessions use.
        let me = app
            .oneshot(
                Request::builder()
                    .uri("/api/me")
                    .header("cookie", cookie.split(';').next().unwrap())
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(me.status(), StatusCode::OK);
        Ok(())
    }

    /// No admin configured must be indistinguishable from a wrong password.
    #[sqlx::test]
    async fn local_login_with_no_admin_configured_returns_the_same_failure(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let res = router(test_state(pool.clone()))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auth/local")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"username":"admin","password":"anything"}"#))?,
            )
            .await?;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert!(res.headers().get("set-cookie").is_none());
        Ok(())
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p argus-server http::tests::local_login`
Expected: FAIL — no route matches, so the status is 404 rather than 401.

- [ ] **Step 3: Implement the handler**

`POST /auth/local` takes `Json<{username, password}>` and:

1. Asks the limiter; on `Some(retry_after)` returns `429` with a `Retry-After` header **without** touching the database.
2. Loads the admin row. If absent, calls `password::verify_against_dummy(&password)` and returns the generic failure — the dummy verification is what equalises timing, so it must not be skipped.
3. Compares the username and verifies the password. On failure: `record_failure`, write `auth.denied` with `detail = {"method":"local"}`, return the generic failure.
4. On success: `record_success`, `touch_local_admin_login`, mint a session exactly as the OIDC callback does with `Identity { subject: "local:admin".into(), email: None, display_name: Some("Local admin".into()) }`, write `auth.login` with `detail = {"method":"local"}`, set the session cookie, return `200`.

The failure response is identical in body and status for every failing case.

Add `{"method":"oidc"}` to the OIDC callback's existing `auth.login` / `auth.denied` writes in the same commit.

`POST /api/local-admin/rotate` sits **inside** the authenticated `/api` router, takes `AuthUser`, calls `reset_local_admin`, audits `local_admin.rotate`, and returns `{"password": "..."}`. It is a `POST` under `/api`, so `SameSite=Lax` plus the middleware protect it.

- [ ] **Step 4: Run the tests**

```bash
cargo sqlx prepare --workspace -- --all-targets
cargo test -p argus-server
SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings
```

Expected: PASS and clean.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src .sqlx
git commit -m "feat(auth): POST /auth/local and authenticated rotation

A local login mints an ordinary session through the same create_session
the OIDC callback uses, so expiry, revocation, logout, middleware and
audit are unchanged -- there is no second session concept. Wrong
username, wrong password and no-admin-configured are indistinguishable,
including in timing: the no-admin path still verifies against a dummy
hash."
```

---

### Task 7: Frontend

**Files:**
- Modify: `frontend/src/components/SignIn.tsx`, `frontend/src/api.ts`
- Create: `frontend/src/components/RotateLocalAdmin.tsx`

- [ ] **Step 1: Local sign-in disclosure**

`SignIn.tsx` keeps the SSO button primary and adds a collapsed "Use a local account" disclosure containing username and password fields and a submit that `POST`s JSON to `/auth/local`. On success, invalidate `["me"]` so the gate re-evaluates; on failure show one generic message ("Sign-in failed") — do not surface whether the account exists.

When SSO is not configured the server returns 404 from `/auth/login`; the SSO button should therefore not be the only affordance. Keep both visible.

- [ ] **Step 2: Rotation dialog**

`RotateLocalAdmin.tsx` posts to `/api/local-admin/rotate` and shows the returned password once in an rnui `Dialog` with a copy control and an explicit "this will not be shown again" warning. Place it on the settings/sidebar surface beside the existing sign-out control.

- [ ] **Step 3: Verify**

```bash
npm --prefix frontend run typecheck && npm --prefix frontend run build
```

Then load the app and check **both light and dark mode**: the disclosure collapsed and expanded, a failed local login, a successful one, and the rotation dialog. Three defects in this project have been dark-mode-only.

- [ ] **Step 4: Commit**

```bash
git add frontend/src
git commit -m "feat(frontend): local sign-in disclosure and rotation dialog"
```

---

### Task 8: Docs, gates, and live verification

**Files:**
- Modify: `docs/DEV.md`

- [ ] **Step 1: Full gates**

```bash
export DATABASE_URL='postgres://postgres:argus@localhost:5432/argus'
npm --prefix frontend run build
cargo fmt --all --check
SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings
cargo sqlx prepare --workspace --check -- --all-targets
cargo test --workspace
cargo test --workspace -- --ignored --skip live_
```

> The `--ignored` run rewrites `ca_material` and orphans the enrolled dev agent
> (`docs/DEV.md:611`). Re-enroll before the live checks.

- [ ] **Step 2: The live check that is the whole feature**

With the server stopped:

```bash
cargo run -p argus-server -- local-admin reset          # note the password
env -u ARGUS_OIDC_ISSUER -u ARGUS_OIDC_CLIENT_ID -u ARGUS_OIDC_CLIENT_SECRET \
    -u ARGUS_OIDC_REQUIRED_ROLE cargo run -p argus-server
```

Then in a browser: sign in with the generated password, confirm the fleet page
loads, and confirm the audit row records `method=local`:

```bash
docker exec argus-pg psql -U postgres -d argus -tAc \
  "SELECT actor, action, result, detail FROM audit_log WHERE action LIKE 'auth.%' ORDER BY id DESC LIMIT 3"
```

Expected: the server boots with **no OIDC configuration at all**, the login
succeeds, and the audit row names the local method. If that sequence passes, the
recovery path works.

Also verify: rotation invalidates the previous password; the boot rule refuses to
start when the `local_admin` table is empty *and* OIDC is unset, with an error
naming the CLI.

- [ ] **Step 3: Document**

Add a "Local admin (break-glass)" section to `docs/DEV.md`: the CLI command, that
the password is shown once and is rotatable, the boot rule, and the measured
results of Step 2. Record only what was observed.

- [ ] **Step 4: Commit**

```bash
git add docs/DEV.md
git commit -m "docs: local admin runbook and live verification"
```

---

## Self-Review

**Spec coverage.** §4 boot rule → Task 3. §5.1 CLI → Task 4. §5.2 in-app rotation → Tasks 6, 7. §5.3 no setup page → nothing to build, enforced by omission. §6 table → Task 2. §7 generation → Task 1. §8 login → Task 6. §9 audit method → Task 6. §10 rate limiting → Task 5. §11 indistinguishable failure → Tasks 1 (dummy hash), 6 (handler). §12 UI → Task 7. §13 dependency gate → Task 1 Step 1. §14 testing → Tasks 1-6. §15 risks → `last_login_at` (Task 2) and the audit method field (Task 6).

**Known gap, deliberate:** `DUMMY_PHC` in Task 1 is given as an illustrative literal with an explicit instruction to regenerate and verify it locally. A hash I cannot execute is not one I should assert is valid, and the accompanying test fails if it is wrong.

**Type consistency.** `LocalAdmin { username, password_hash }` (Task 2) is used unchanged in Tasks 4 and 6. `reset_local_admin` (Task 4) is reused by Task 6's rotate endpoint. `verify_password`/`hash_password`/`generate_password` (Task 1) are used in Tasks 4 and 6. `local_admin_exists` (Task 2) is used by Task 3's boot rule. `Config.oidc: Option<OidcConfig>` (Task 3) is consumed by Task 6's handlers.
