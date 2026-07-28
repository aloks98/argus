# Fleet Identity & Navigation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Machines get display names, tags, and notes (settable at enrollment via the token, editable in the UI); the fleet page gains search/filter/grouping; an Enroll page mints tokens; Ctrl+K opens a command palette.

**Architecture:** One migration adds `display_name` (machines + tokens) and token-side `tags` — `machines.tags`/`notes` and the token's `max_uses`/`expires_at`/`revoked` already exist from 0001. Server: one validation module, a `PATCH /api/machines/{id}`, an enrollment-token CRUD surface, and identity application inside the existing Enroll transaction. Frontend: pure filter/group/palette logic in `lib/fleet.ts`, consumed by the fleet page, a detail-page identity card, an `/enroll` page, and an rnui `CommandDialog`.

**Tech Stack:** axum 0.8 + sqlx (compile-time-checked, `.sqlx` offline cache), rnui (`Command*`, `Combobox*`, `ToggleGroup`, `CopyButton`, `Badge`, `Kbd`), react-hook-form + zod (mint form), TanStack Query, react-router `useSearchParams`.

**Design of record:** `docs/superpowers/specs/2026-07-28-fleet-identity-design.md`.

## Global Constraints

- **Validation (server-side, one implementation):** tags: trim → lowercase → order-preserving dedupe → each must match `^[a-z0-9][a-z0-9._-]{0,31}$` → max **16** per machine/token; violations are a 400 naming the offending tag. `display_name`: trimmed, empty ⇒ null, max **64** chars. `notes`: max **4000** chars.
- **Token mint defaults:** `max_uses = 1`, `expires_at = now() + 24h`. Explicit JSON `null` for either means unlimited/never.
- **Every mutation writes an audit row** (`machine.update`, `enroll_token.create`, `enroll_token.revoke`) via `repo::audit`/`audit_with_detail`. A verb without an audit write is incomplete.
- **The raw token appears exactly once** — in the mint response. Only the sha256 is ever stored; the list endpoint never returns it.
- **sqlx cache:** after adding/altering any `query!`, run `cargo sqlx prepare --workspace` (dev DB up: container `argus-pg`). `cargo check` passing does NOT prove the cache is current — CI runs `cargo sqlx prepare --workspace --check`.
- **Gates per task:** `cargo fmt --all --check`, `SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p argus-server` (DB tests are `#[sqlx::test]`), and for frontend tasks `npm --prefix frontend run typecheck && npm --prefix frontend run build`.
- **UI components come from `@e412/rnui-react`** — verify a component's props against `frontend/node_modules/@e412/rnui-react/dist/index.d.ts` before use; do not hand-roll what the library ships. No new frontend dependencies.
- **Copy style:** UI labels are sentence case; mono/uppercase/tracking-widest treatment for small labels matches existing pages.

---

### Task 1: Migration + display_name/capabilities in the read paths

**Files:**
- Create: `crates/server/migrations/0006_display_name.sql`
- Modify: `crates/server/src/http.rs` (FleetRow + fleet query + MachineDetailDto), `crates/server/src/repo.rs` (`MachineDetail` + `machine_detail` query), `frontend/src/api.ts` (types)

**Interfaces:**
- Produces: `machines.display_name text`, `enrollment_tokens.display_name text`, `enrollment_tokens.tags text[] not null default '{}'`; `FleetRow.display_name: Option<String>` + `FleetRow.capabilities: Option<Vec<String>>` (JSON: `string[] | null`); `MachineDetailDto.display_name`.

- [ ] **Step 1: Write the migration**

```sql
-- 0006_display_name.sql
-- Identity metadata (fleet-identity slice). machines.tags/notes and the
-- token's max_uses/expires_at/revoked already exist from 0001 — this adds
-- only what's missing. display_name is nullable ON PURPOSE: null means
-- "show the hostname", so a machine never renamed keeps tracking hostname
-- changes instead of freezing a stale copy taken at enroll time.
alter table machines add column display_name text;
alter table enrollment_tokens add column display_name text;
alter table enrollment_tokens add column tags text[] not null default '{}';
```

- [ ] **Step 2: Extend the fleet query and `FleetRow`** in `crates/server/src/http.rs`

In the `fleet` handler, change the query to include the two new columns:

```rust
let rows = sqlx::query!(
    r#"SELECT id, hostname, display_name, os, host(primary_ip) as "primary_ip?", status,
              last_seen_at, tags, capabilities FROM machines ORDER BY hostname"#
)
```

Add to the `FleetRow` struct (after `hostname`):

```rust
    /// Operator-set name; `None` = "display the hostname". The fallback lives
    /// client-side so a hostname change keeps showing through.
    display_name: Option<String>,
```

and (after `tags`):

```rust
    /// Same tri-state as the detail payload: `None` = agent never reported =
    /// gate nothing. Carried on the fleet row for the command palette, which
    /// builds per-machine tab entries without fetching every detail page.
    capabilities: Option<Vec<String>>,
```

Populate both in the `FleetRow { .. }` construction (`display_name: r.display_name`, `capabilities: r.capabilities`).

- [ ] **Step 3: Extend `MachineDetail`/`machine_detail`** in `crates/server/src/repo.rs`: add `pub display_name: Option<String>` to the struct and `display_name` to the SELECT column list of the `machine_detail` query. Add `display_name: Option<String>` to `MachineDetailDto` in http.rs and map it in the `From` impl.

- [ ] **Step 4: Extend the TS types** in `frontend/src/api.ts`: add `display_name: string | null` to `FleetRow` (after `hostname`) and to `MachineDetail`; add `capabilities: string[] | null` to `FleetRow` with the same "`null` = gate nothing" doc comment `MachineDetail` uses.

- [ ] **Step 5: Refresh the sqlx cache and run the gates**

```bash
docker start argus-pg 2>/dev/null; cargo sqlx prepare --workspace
cargo fmt --all --check && SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings
cargo test -p argus-server
npm --prefix frontend run typecheck && npm --prefix frontend run build
```

Existing `#[sqlx::test]` http tests must stay green (the migration runs automatically in their throwaway databases).

- [ ] **Step 6: Extend one existing test** — in `http.rs` tests, `fleet_lists_machines_with_status`: assert the serialized row carries `"display_name": null` and a `"capabilities"` key, so the new fields are locked into the payload shape.

- [ ] **Step 7: Commit** — `git add -A && git commit -m "feat(fleet): display_name columns and identity fields in read payloads"`

---

### Task 2: The validation module

**Files:**
- Create: `crates/server/src/identity.rs`
- Modify: `crates/server/src/main.rs` (add `mod identity;`)

**Interfaces:**
- Produces:
  - `pub fn normalize_tags(raw: &[String]) -> Result<Vec<String>, String>` — Err = human-readable 400 message naming the offending tag.
  - `pub fn normalize_display_name(raw: &str) -> Result<Option<String>, String>` — `Ok(None)` = clear (empty after trim).
  - `pub fn validate_notes(raw: &str) -> Result<(), String>`
  - `pub const MAX_TAGS: usize = 16;`

- [ ] **Step 1: Write the failing tests** (in-module `#[cfg(test)]`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_are_trimmed_lowercased_and_deduped_in_order() {
        let raw = vec![" Infra ".into(), "media".into(), "infra".into()];
        assert_eq!(normalize_tags(&raw).unwrap(), vec!["infra", "media"]);
    }

    #[test]
    fn each_tag_rejection_class_names_the_offender() {
        // Whitespace-only normalizes to empty and is rejected (not dropped).
        assert!(normalize_tags(&["  ".into()]).is_err());
        let long = "a".repeat(33);
        assert!(normalize_tags(&[long.clone()]).unwrap_err().contains(&long));
        assert!(normalize_tags(&["has space".into()]).unwrap_err().contains("has space"));
        assert!(normalize_tags(&["-leading".into()]).is_err()); // must start [a-z0-9]
        assert!(normalize_tags(&["ok_tag.v1-x".into()]).is_ok());
    }

    #[test]
    fn more_than_max_tags_is_rejected_after_dedupe() {
        let raw: Vec<String> = (0..MAX_TAGS + 1).map(|i| format!("t{i}")).collect();
        assert!(normalize_tags(&raw).is_err());
        // ...but 17 raw entries that dedupe to <= 16 are fine.
        let mut dup = vec!["same".to_string(); 2];
        dup.extend((0..MAX_TAGS - 1).map(|i| format!("t{i}")));
        assert_eq!(dup.len(), MAX_TAGS + 1);
        assert!(normalize_tags(&dup).is_ok());
    }

    #[test]
    fn display_name_trims_clears_and_caps() {
        assert_eq!(normalize_display_name("  Media box  ").unwrap(), Some("Media box".into()));
        assert_eq!(normalize_display_name("   ").unwrap(), None);
        assert!(normalize_display_name(&"x".repeat(65)).is_err());
        assert!(normalize_display_name(&"x".repeat(64)).is_ok());
    }

    #[test]
    fn notes_cap_at_4000() {
        assert!(validate_notes(&"x".repeat(4000)).is_ok());
        assert!(validate_notes(&"x".repeat(4001)).is_err());
    }
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p argus-server identity` → FAIL (module missing).

- [ ] **Step 3: Implement**

```rust
//! Identity-field validation (fleet-identity slice, design "Validation
//! rules"): ONE implementation shared by every write path — the PATCH
//! handler, token minting, and enroll-time application all funnel through
//! here so the rules cannot drift apart. No regex crate: the character class
//! is trivial and the server has no regex dependency to justify.

pub const MAX_TAGS: usize = 16;
const MAX_TAG_LEN: usize = 32;
const MAX_DISPLAY_NAME_LEN: usize = 64;
const MAX_NOTES_LEN: usize = 4000;

/// trim → lowercase → order-preserving dedupe → validate each. Errors carry
/// the offending value so the 400 is actionable; nothing is silently dropped.
pub fn normalize_tags(raw: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for r in raw {
        let t = r.trim().to_lowercase();
        if t.is_empty() {
            return Err("tags must not be empty".into());
        }
        if t.len() > MAX_TAG_LEN {
            return Err(format!("tag too long (max {MAX_TAG_LEN} chars): {t}"));
        }
        let mut chars = t.chars();
        let head_ok = chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
        let tail_ok = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
        if !head_ok || !tail_ok {
            return Err(format!(
                "invalid tag (lowercase letters, digits, '.', '_', '-'; must start with a letter or digit): {t}"
            ));
        }
        if !out.contains(&t) {
            out.push(t);
        }
    }
    if out.len() > MAX_TAGS {
        return Err(format!("too many tags (max {MAX_TAGS})"));
    }
    Ok(out)
}

/// `Ok(None)` = clear back to "display the hostname".
pub fn normalize_display_name(raw: &str) -> Result<Option<String>, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Ok(None);
    }
    if t.len() > MAX_DISPLAY_NAME_LEN {
        return Err(format!("display name too long (max {MAX_DISPLAY_NAME_LEN} chars)"));
    }
    Ok(Some(t.to_string()))
}

pub fn validate_notes(raw: &str) -> Result<(), String> {
    if raw.len() > MAX_NOTES_LEN {
        return Err(format!("notes too long (max {MAX_NOTES_LEN} chars)"));
    }
    Ok(())
}
```

Add `mod identity;` to `main.rs` alongside the existing module list.

- [ ] **Step 4: Run tests** — `cargo test -p argus-server identity` → PASS. Run fmt/clippy gates.

- [ ] **Step 5: Commit** — `git commit -am "feat(fleet): identity-field validation module"`

---

### Task 3: `PATCH /api/machines/{id}`

**Files:**
- Modify: `crates/server/src/repo.rs` (add `update_machine_identity`), `crates/server/src/http.rs` (handler + route), then `cargo sqlx prepare`.

**Interfaces:**
- Consumes: `identity::{normalize_tags, normalize_display_name, validate_notes}` (Task 2); `repo::audit_with_detail(pool, Actor, action, machine_id, outcome, detail)` (existing).
- Produces: `PATCH /api/machines/{id}` — body `{ display_name?: string|null, notes?: string|null, tags?: string[] }`; absent = untouched, `null` (or empty string for display_name) = clear; 200 → the refreshed `MachineDetailDto`; 400 invalid; 404 unknown machine. Audit `machine.update`, detail `{"fields": ["tags", ...]}`.

- [ ] **Step 1: Add the repo function**

```rust
/// Partial identity update. Each field is guarded by its own `apply` flag so
/// one static, compile-time-checked query covers every PATCH combination —
/// no dynamic SQL. Returns false when the machine id does not exist.
pub async fn update_machine_identity(
    executor: impl sqlx::PgExecutor<'_>,
    machine_id: Uuid,
    display_name: Option<Option<&str>>,
    notes: Option<Option<&str>>,
    tags: Option<&[String]>,
) -> Result<bool> {
    let res = sqlx::query!(
        r#"
        UPDATE machines SET
            display_name = CASE WHEN $2 THEN $3::text ELSE display_name END,
            notes        = CASE WHEN $4 THEN $5::text ELSE notes END,
            tags         = CASE WHEN $6 THEN $7::text[] ELSE tags END,
            updated_at   = now()
        WHERE id = $1
        "#,
        machine_id,
        display_name.is_some(),
        display_name.flatten(),
        notes.is_some(),
        notes.flatten(),
        tags.is_some(),
        tags.unwrap_or(&[]),
    )
    .execute(executor)
    .await?;
    Ok(res.rows_affected() == 1)
}
```

- [ ] **Step 2: Add the handler + route** in `http.rs`

```rust
/// Body of `PATCH /api/machines/{id}`. The double `Option` distinguishes an
/// absent key (leave the field alone) from an explicit `null` (clear it) —
/// plain `Option<String>` cannot represent that difference after serde.
#[derive(serde::Deserialize)]
struct MachinePatch {
    #[serde(default, deserialize_with = "double_option")]
    display_name: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    notes: Option<Option<String>>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

fn double_option<'de, D>(d: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    Ok(Some(Option::<String>::deserialize(d)?))
}

/// `PATCH /api/machines/{id}` — display name / notes / tags. Only the fields
/// present in the body change. Every outcome that MUTATED is audited; a 400
/// mutates nothing and is not.
async fn patch_machine(
    State(state): State<AppState>,
    crate::auth::AuthUser(identity): crate::auth::AuthUser,
    Path(id): Path<Uuid>,
    Json(patch): Json<MachinePatch>,
) -> Response {
    // Validate everything BEFORE touching the database.
    let display_name = match &patch.display_name {
        None => None,
        Some(None) => Some(None),
        Some(Some(raw)) => match crate::identity::normalize_display_name(raw) {
            Ok(v) => Some(v),
            Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
        },
    };
    let notes = match &patch.notes {
        None => None,
        Some(None) => Some(None),
        Some(Some(raw)) => match crate::identity::validate_notes(raw) {
            Ok(()) => Some(Some(raw.clone())),
            Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
        },
    };
    let tags = match &patch.tags {
        None => None,
        Some(raw) => match crate::identity::normalize_tags(raw) {
            Ok(v) => Some(v),
            Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
        },
    };
    let mut fields: Vec<&str> = Vec::new();
    if display_name.is_some() { fields.push("display_name"); }
    if notes.is_some() { fields.push("notes"); }
    if tags.is_some() { fields.push("tags"); }
    if fields.is_empty() {
        return (StatusCode::BAD_REQUEST, "no fields to update").into_response();
    }

    let dn_arg: Option<Option<&str>> = display_name.as_ref().map(|o| o.as_deref());
    let notes_arg: Option<Option<&str>> = notes.as_ref().map(|o| o.as_deref());
    let updated = match repo::update_machine_identity(
        &state.pool, id, dn_arg, notes_arg, tags.as_deref(),
    )
    .await
    {
        Ok(u) => u,
        Err(err) => {
            tracing::error!(error = %err, "failed to update machine identity");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if !updated {
        return StatusCode::NOT_FOUND.into_response();
    }

    // detail = which fields changed, never the values: notes may hold anything.
    if let Err(err) = repo::audit_with_detail(
        &state.pool,
        repo::Actor::from(&identity),
        "machine.update",
        Some(id),
        "ok",
        serde_json::json!({ "fields": fields }),
    )
    .await
    {
        tracing::error!(error = %err, "failed to audit machine.update");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // Return the refreshed detail so the client can cache-swap in one round trip.
    match repo::machine_detail(&state.pool, id).await {
        Ok(Some(d)) => Json(MachineDetailDto::from(d)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(err) => {
            tracing::error!(error = %err, "failed to reload machine detail");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
```

Check how existing authenticated handlers obtain the actor (`crate::auth::AuthUser` extractor and the `Actor` conversion the local-admin slice introduced — mirror `rotate`'s pattern exactly; if `Actor::from(&identity)` does not exist, use whatever `rotate` uses to mint the actor). Register the route in the `api` router:

```rust
        .route("/api/machines/{id}", get(machine).patch(patch_machine))
```

(merge with the existing `get(machine)` line rather than adding a second `route` call for the same path).

- [ ] **Step 3: `cargo sqlx prepare --workspace`**, then write the tests (`#[sqlx::test]`, following the file's existing auth-cookie pattern via `auth_cookie(&pool)`):

```rust
#[sqlx::test]
async fn patch_machine_partial_update_and_audit(pool: PgPool) -> anyhow::Result<()> {
    // Seed a machine directly (raw SQL precondition, per the standing lesson).
    let id: Uuid = sqlx::query_scalar!(
        r#"INSERT INTO machines (machine_id, hostname, tags) VALUES ('m-1', 'host-1', '{old}')
           RETURNING id"#
    )
    .fetch_one(&pool)
    .await?;
    let cookie = auth_cookie(&pool).await?;
    let app = router(test_state(&pool));

    // tags only: display_name must be untouched (null), old tags replaced.
    let res = request_json(
        &app,
        "PATCH",
        &format!("/api/machines/{id}"),
        &cookie,
        serde_json::json!({ "tags": [" Infra ", "media", "infra"] }),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let row = sqlx::query!("SELECT display_name, tags FROM machines WHERE id = $1", id)
        .fetch_one(&pool)
        .await?;
    assert_eq!(row.display_name, None);
    assert_eq!(row.tags, vec!["infra", "media"]);

    // display_name set, then cleared with null.
    request_json(&app, "PATCH", &format!("/api/machines/{id}"), &cookie,
        serde_json::json!({ "display_name": "Media box" })).await;
    request_json(&app, "PATCH", &format!("/api/machines/{id}"), &cookie,
        serde_json::json!({ "display_name": null })).await;
    let dn = sqlx::query_scalar!("SELECT display_name FROM machines WHERE id = $1", id)
        .fetch_one(&pool)
        .await?;
    assert_eq!(dn, None);

    // Audit rows written, naming the changed fields.
    let audits = sqlx::query!(
        r#"SELECT detail FROM audit_log WHERE action = 'machine.update' ORDER BY ts"#
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(audits.len(), 3);
    assert_eq!(audits[0].detail.as_ref().unwrap()["fields"][0], "tags");
    Ok(())
}

#[sqlx::test]
async fn patch_machine_rejects_bad_input(pool: PgPool) -> anyhow::Result<()> {
    let id: Uuid = sqlx::query_scalar!(
        r#"INSERT INTO machines (machine_id, hostname) VALUES ('m-2', 'host-2') RETURNING id"#
    )
    .fetch_one(&pool)
    .await?;
    let cookie = auth_cookie(&pool).await?;
    let app = router(test_state(&pool));

    for bad in [
        serde_json::json!({ "tags": ["has space"] }),
        serde_json::json!({ "display_name": "x".repeat(65) }),
        serde_json::json!({}),
    ] {
        let res = request_json(&app, "PATCH", &format!("/api/machines/{id}"), &cookie, bad).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }
    // A 400 mutates nothing and writes no audit row.
    let n: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) as "n!" FROM audit_log WHERE action = 'machine.update'"#
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(n, 0);
    // Unknown machine: 404.
    let res = request_json(&app, "PATCH", &format!("/api/machines/{}", Uuid::new_v4()), &cookie,
        serde_json::json!({ "tags": [] })).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    Ok(())
}
```

The test module already has helpers for building the router and issuing
requests — reuse them (`test_state`, tower's `oneshot`, etc.); add a small
`request_json(app, method, uri, cookie, body)` helper next to them if one
doesn't exist, following the file's existing request-building style.

- [ ] **Step 4: Run the gates** (fmt, clippy offline, `cargo test -p argus-server`).

- [ ] **Step 5: Commit** — `git commit -am "feat(fleet): PATCH machine identity with audit"`

---

### Task 4: Enrollment-token API + `GET /api/ca.pem`

**Files:**
- Modify: `crates/server/src/repo.rs` (list/mint/revoke), `crates/server/src/http.rs` (three handlers + ca.pem + routes), then `cargo sqlx prepare`.

**Interfaces:**
- Consumes: `identity::{normalize_tags, normalize_display_name}`; the actor pattern from Task 3.
- Produces:
  - `GET /api/enrollment-tokens` → `[{id, name, display_name, tags, max_uses, uses, expires_at, revoked, created_by, created_at}]`, newest first. Never the hash or raw token.
  - `POST /api/enrollment-tokens` — body `{name, display_name?, tags?, max_uses?: number|null, expires_in_hours?: number|null}`; absent `max_uses` ⇒ 1, absent `expires_in_hours` ⇒ 24, explicit `null` ⇒ unlimited/never; 201 → the row plus `token` (raw, once).
  - `DELETE /api/enrollment-tokens/{id}` → 204 (revoked), 404 unknown.
  - `GET /api/ca.pem` → `text/plain` CA certificate.
  - repo: `list_enrollment_tokens(pool)`, `mint_enrollment_token(pool, name, display_name, tags, max_uses, expires_in_hours) -> Result<(TokenRow, String)>`, `revoke_enrollment_token(pool, id) -> Result<bool>`.

- [ ] **Step 1: repo functions.** Raw token: 32 chars from the same alphanumeric-generation approach `auth/password.rs` uses (check `generate_password` and reuse its mechanism/crate — do not add a new RNG dependency). Mint inserts `sha256(raw)` and computes expiry in SQL:

```rust
pub struct TokenRow {
    pub id: Uuid,
    pub name: String,
    pub display_name: Option<String>,
    pub tags: Vec<String>,
    pub max_uses: Option<i32>,
    pub uses: i32,
    pub expires_at: Option<OffsetDateTime>,
    pub revoked: bool,
    pub created_by: Option<String>,
    pub created_at: OffsetDateTime,
}

pub async fn mint_enrollment_token(
    executor: impl sqlx::PgExecutor<'_>,
    name: &str,
    display_name: Option<&str>,
    tags: &[String],
    max_uses: Option<i32>,
    expires_in_hours: Option<i64>,
    created_by: &str,
) -> Result<(TokenRow, String)> {
    let raw = generate_token(); // 32 alphanumeric chars, same mechanism as password.rs
    let token_hash = Sha256::digest(raw.as_bytes()).to_vec();
    let row = sqlx::query_as!(
        TokenRow,
        r#"
        INSERT INTO enrollment_tokens (name, token_hash, display_name, tags, max_uses, expires_at, created_by)
        VALUES ($1, $2, $3, $4, $5, now() + make_interval(hours => $6), $7)
        RETURNING id, name, display_name, tags as "tags!", max_uses, uses, expires_at, revoked, created_by, created_at
        "#,
        name, token_hash, display_name, tags, max_uses, expires_in_hours, created_by,
    )
    .fetch_one(executor)
    .await?;
    Ok((row, raw))
}
```

`make_interval(hours => NULL)` yields NULL, so a `None` expiry naturally stores "never" — verify this against the dev database (`SELECT now() + make_interval(hours => NULL)` → NULL) and note it in a comment. `list_enrollment_tokens` is a `query_as!` SELECT of the same columns `ORDER BY created_at DESC`; `revoke_enrollment_token` is `UPDATE enrollment_tokens SET revoked = true WHERE id = $1` returning `rows_affected() == 1`.

- [ ] **Step 2: handlers.** POST validates `name` (trimmed, non-empty, ≤64 — reuse `normalize_display_name` for the trim/cap semantics), `display_name`, `tags` via the Task 2 module; clamps `max_uses` to ≥1 when `Some`; clamps `expires_in_hours` to 1..=8760 when `Some`. `created_by` = the authenticated actor string. Audit `enroll_token.create` (detail: `{"name": ...}`) and `enroll_token.revoke` (detail: `{"name": ...}` — fetch the name before revoking, 404 if absent). The mint response serializes the row **plus** `"token": raw`. `GET /api/ca.pem`:

```rust
/// The CA certificate for the enroll page's download button. Behind auth like
/// the rest of /api (the cert is not secret, but nothing here is served
/// unauthenticated). 503 when the CA hasn't been initialized yet.
async fn ca_pem(State(state): State<AppState>) -> Response {
    match sqlx::query_scalar!("SELECT cert_pem FROM ca_material WHERE id = 1")
        .fetch_optional(&state.pool)
        .await
    {
        Ok(Some(pem)) => ([(header::CONTENT_TYPE, "text/plain")], pem).into_response(),
        Ok(None) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        Err(err) => {
            tracing::error!(error = %err, "failed to load CA cert");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
```

Routes (in the `api` router): `/api/enrollment-tokens` `get(list_tokens).post(mint_token)`, `/api/enrollment-tokens/{id}` `delete(revoke_token)`, `/api/ca.pem` `get(ca_pem)`.

- [ ] **Step 3: `cargo sqlx prepare --workspace`**, then tests:

```rust
#[sqlx::test]
async fn token_mint_defaults_and_raw_once(pool: PgPool) -> anyhow::Result<()> {
    let cookie = auth_cookie(&pool).await?;
    let app = router(test_state(&pool));
    let res = request_json(&app, "POST", "/api/enrollment-tokens", &cookie,
        serde_json::json!({ "name": "pve1-media", "tags": ["media"] })).await;
    assert_eq!(res.status(), StatusCode::CREATED);
    let body: serde_json::Value = body_json(res).await?;
    let raw = body["token"].as_str().unwrap();
    assert_eq!(raw.len(), 32);
    assert_eq!(body["max_uses"], 1);
    assert!(!body["expires_at"].is_null()); // ~now+24h

    // The raw token is NOT in the list payload — only in the mint response.
    let list = request(&app, "GET", "/api/enrollment-tokens", &cookie).await;
    let rows: serde_json::Value = body_json(list).await?;
    assert!(rows[0].get("token").is_none());
    assert!(rows[0].get("token_hash").is_none());

    // The minted token actually works against the consume path.
    let check = repo::consume_enrollment_token(&pool, raw).await?;
    assert!(matches!(check, repo::TokenCheck::Valid { .. }));
    // ...and, being single-use, only once.
    let again = repo::consume_enrollment_token(&pool, raw).await?;
    assert!(matches!(again, repo::TokenCheck::Invalid));
    Ok(())
}

#[sqlx::test]
async fn token_revoke_and_audit_rows(pool: PgPool) -> anyhow::Result<()> {
    let cookie = auth_cookie(&pool).await?;
    let app = router(test_state(&pool));
    let res = request_json(&app, "POST", "/api/enrollment-tokens", &cookie,
        serde_json::json!({ "name": "t", "max_uses": null })).await;
    let body: serde_json::Value = body_json(res).await?;
    assert!(body["max_uses"].is_null()); // explicit null = unlimited
    let id = body["id"].as_str().unwrap().to_string();
    let raw = body["token"].as_str().unwrap().to_string();

    let del = request(&app, "DELETE", &format!("/api/enrollment-tokens/{id}"), &cookie).await;
    assert_eq!(del.status(), StatusCode::NO_CONTENT);
    // Revoked ⇒ the consume path refuses it.
    let check = repo::consume_enrollment_token(&pool, &raw).await?;
    assert!(matches!(check, repo::TokenCheck::Invalid));

    let n: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) as "n!" FROM audit_log
           WHERE action IN ('enroll_token.create', 'enroll_token.revoke')"#
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(n, 3); // two creates (this test + none else) — count exactly per your test's own mints
    Ok(())
}
```

(Adjust the audit count assertion to exactly what the test minted; do not
assert a number copied from this plan without recounting.)

- [ ] **Step 4: Run the gates.**

- [ ] **Step 5: Commit** — `git commit -am "feat(fleet): enrollment token mint/list/revoke API and CA download"`

---

### Task 5: Enroll-time identity application

**Files:**
- Modify: `crates/server/src/repo.rs` (`TokenCheck::Valid` + `consume_enrollment_token` + new `apply_token_identity`), `crates/server/src/grpc.rs` (call it in the success path), then `cargo sqlx prepare`.

**Interfaces:**
- Consumes: the Enroll transaction structure in `grpc.rs` (token consumed first, `upsert_machine` → `agent_id` inside the same `tx`).
- Produces: `TokenCheck::Valid { token_name: String, display_name: Option<String>, tags: Vec<String> }`.

- [ ] **Step 1: Extend the consume query** to `RETURNING name, display_name, tags` and the `Valid` variant to carry all three. Fix the two existing pattern-match sites (`grpc.rs`, any tests) — the compiler finds them.

- [ ] **Step 2: Add the application function**

```rust
/// Apply a token's identity fields to the machine it just enrolled — ONLY
/// where the token actually set them (design "Enrollment flow"): a null
/// display_name / empty tags on the token leaves the machine's existing
/// values untouched, which is what makes re-enrollment after CA rotation
/// identity-preserving by default.
pub async fn apply_token_identity(
    executor: impl sqlx::PgExecutor<'_>,
    machine_id: Uuid,
    display_name: Option<&str>,
    tags: &[String],
) -> Result<()> {
    sqlx::query!(
        r#"
        UPDATE machines SET
            display_name = coalesce($2, display_name),
            tags         = CASE WHEN cardinality($3::text[]) > 0 THEN $3 ELSE tags END,
            updated_at   = now()
        WHERE id = $1
        "#,
        machine_id,
        display_name,
        tags,
    )
    .execute(executor)
    .await?;
    Ok(())
}
```

- [ ] **Step 3: Call it in `grpc.rs`** — inside the success-path async block, immediately after `upsert_machine` returns `agent_id` (same `tx`, so a later rollback undoes it):

```rust
            repo::apply_token_identity(&mut *tx, agent_id, token_display_name.as_deref(), &token_tags)
                .await
                .map_err(|e| internal_error("applying token identity", &e))?;
```

(bind `token_display_name` / `token_tags` out of the `Valid` destructure next to `token_name`). Token tags were normalized at mint time (Task 4); tokens inserted by raw SQL (the DEV.md path) bypass that, which is acceptable — the psql path is operator-owned.

- [ ] **Step 4: `cargo sqlx prepare --workspace`**, then tests (repo-level, in `repo.rs`'s test module or the existing enroll test location — follow where `consume_enrollment_token` is currently tested):

```rust
#[sqlx::test]
async fn token_identity_applies_on_enroll_and_preserves_on_reenroll(pool: PgPool) -> anyhow::Result<()> {
    // A token WITH identity: applies both fields.
    sqlx::query!(
        r#"INSERT INTO enrollment_tokens (name, token_hash, display_name, tags)
           VALUES ('t1', sha256('raw1'::bytea), 'Media box', '{media,infra}')"#
    )
    .execute(&pool)
    .await?;
    let repo::TokenCheck::Valid { display_name, tags, .. } =
        repo::consume_enrollment_token(&pool, "raw1").await?
    else { panic!("token should be valid") };
    let info = test_agent_info("m-ident"); // reuse/extend the file's existing AgentInfoRow test helper
    let id = repo::upsert_machine(&pool, &info).await?;
    repo::apply_token_identity(&pool, id, display_name.as_deref(), &tags).await?;
    let row = sqlx::query!("SELECT display_name, tags FROM machines WHERE id = $1", id)
        .fetch_one(&pool)
        .await?;
    assert_eq!(row.display_name.as_deref(), Some("Media box"));
    assert_eq!(row.tags, vec!["media", "infra"]);

    // Re-enroll with a BARE token (no identity): everything preserved.
    sqlx::query!(
        r#"INSERT INTO enrollment_tokens (name, token_hash) VALUES ('t2', sha256('raw2'::bytea))"#
    )
    .execute(&pool)
    .await?;
    let repo::TokenCheck::Valid { display_name, tags, .. } =
        repo::consume_enrollment_token(&pool, "raw2").await?
    else { panic!() };
    let id2 = repo::upsert_machine(&pool, &info).await?; // same machine_id ⇒ same row
    assert_eq!(id, id2);
    repo::apply_token_identity(&pool, id2, display_name.as_deref(), &tags).await?;
    let row = sqlx::query!("SELECT display_name, tags FROM machines WHERE id = $1", id)
        .fetch_one(&pool)
        .await?;
    assert_eq!(row.display_name.as_deref(), Some("Media box"), "bare re-enroll must not clear the name");
    assert_eq!(row.tags, vec!["media", "infra"], "bare re-enroll must not clear tags");
    Ok(())
}
```

(`sha256('raw1'::bytea)` is Postgres's own digest of the literal — it matches
`Sha256::digest("raw1")` on the Rust side; verify once with a quick
`SELECT encode(sha256('raw1'::bytea), 'hex')` against the dev DB if in doubt.)

- [ ] **Step 5: Run the gates.**

- [ ] **Step 6: Commit** — `git commit -am "feat(fleet): enrollment token carries and applies machine identity"`

---

### Task 6: Frontend API + pure fleet logic

**Files:**
- Modify: `frontend/src/api.ts`
- Create: `frontend/src/lib/fleet.ts`

**Interfaces:**
- Produces (api.ts): `patchMachine(id, patch: {display_name?: string|null; notes?: string|null; tags?: string[]}): Promise<MachineDetail>`; `EnrollmentToken` type; `listTokens()`, `mintToken(req): Promise<EnrollmentToken & {token: string}>`, `revokeToken(id)`.
- Produces (lib/fleet.ts): `displayName(m)`, `visibleFleet(rows, q, tags)`, `fleetTags(rows)`, `groupFleet(rows)`, `paletteEntries(rows)` — all pure.

- [ ] **Step 1: api.ts additions** (all through `unauthenticatedOr`, matching the file's style):

```ts
export type MachinePatchBody = {
  display_name?: string | null;
  notes?: string | null;
  tags?: string[];
};

export async function patchMachine(id: string, patch: MachinePatchBody): Promise<MachineDetail> {
  const r = unauthenticatedOr(
    await fetch(`/api/machines/${id}`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(patch),
    }),
  );
  if (!r.ok) throw new Error(await r.text().catch(() => `update failed: ${r.status}`));
  return r.json();
}

export type EnrollmentToken = {
  id: string;
  name: string;
  display_name: string | null;
  tags: string[];
  max_uses: number | null;
  uses: number;
  expires_at: string | null;
  revoked: boolean;
  created_by: string | null;
  created_at: string;
};

export type MintTokenBody = {
  name: string;
  display_name?: string;
  tags?: string[];
  max_uses?: number | null;
  expires_in_hours?: number | null;
};

export async function listTokens(): Promise<EnrollmentToken[]> {
  const r = unauthenticatedOr(await fetch("/api/enrollment-tokens"));
  if (!r.ok) throw new Error(`tokens ${r.status}`);
  return r.json();
}

/** The `token` field is the raw secret, present ONLY in this response. */
export async function mintToken(body: MintTokenBody): Promise<EnrollmentToken & { token: string }> {
  const r = unauthenticatedOr(
    await fetch("/api/enrollment-tokens", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    }),
  );
  if (!r.ok) throw new Error(await r.text().catch(() => `mint failed: ${r.status}`));
  return r.json();
}

export async function revokeToken(id: string): Promise<void> {
  const r = unauthenticatedOr(await fetch(`/api/enrollment-tokens/${id}`, { method: "DELETE" }));
  if (!r.ok) throw new Error(`revoke failed: ${r.status}`);
}
```

- [ ] **Step 2: lib/fleet.ts** — pure, DOM-free (the reviewable core, like `lib/units.ts`):

```ts
// Pure fleet-page logic: filtering, grouping, palette entries. No DOM, no
// fetch — everything here is a plain function of the fleet payload so the
// behavior is reviewable (and one day testable) without a browser.
import type { FleetRow } from "../api";
import { CAP_DOCKER, CAP_JOURNAL, CAP_SYSTEMD } from "../api";

export type FleetView = "flat" | "grouped";

/** The name a machine renders under everywhere: operator-set, else hostname. */
export function displayName(m: Pick<FleetRow, "display_name" | "hostname">): string {
  return m.display_name ?? m.hostname;
}

/**
 * Case-insensitive substring match on display name, hostname and tags,
 * then AND across every selected tag chip (OR across a homelab-sized fleet
 * just reads as "everything").
 */
export function visibleFleet(rows: FleetRow[], q: string, tags: string[]): FleetRow[] {
  const needle = q.trim().toLowerCase();
  return rows.filter((r) => {
    if (tags.some((t) => !r.tags.includes(t))) return false;
    if (needle === "") return true;
    return (
      displayName(r).toLowerCase().includes(needle) ||
      r.hostname.toLowerCase().includes(needle) ||
      r.tags.some((t) => t.includes(needle))
    );
  });
}

/** Every tag in the fleet, sorted, with counts — chips and autocomplete. */
export function fleetTags(rows: FleetRow[]): { tag: string; count: number }[] {
  const counts = new Map<string, number>();
  for (const r of rows) for (const t of r.tags) counts.set(t, (counts.get(t) ?? 0) + 1);
  return [...counts.entries()]
    .map(([tag, count]) => ({ tag, count }))
    .sort((a, b) => a.tag.localeCompare(b.tag));
}

/**
 * Grouped view: one section per tag (alphabetical), a machine under EVERY
 * tag it carries (groups are views, not a partition), untagged machines
 * last under `tag: null`.
 */
export function groupFleet(rows: FleetRow[]): { tag: string | null; rows: FleetRow[] }[] {
  const sections = fleetTags(rows).map(({ tag }) => ({
    tag: tag as string | null,
    rows: rows.filter((r) => r.tags.includes(tag)),
  }));
  const untagged = rows.filter((r) => r.tags.length === 0);
  if (untagged.length > 0) sections.push({ tag: null, rows: untagged });
  return sections;
}

export type PaletteEntry = {
  /** Unique per entry — machine id, or `${id}:${tab}`. */
  key: string;
  label: string;
  /** Muted context after the label (hostname, or the machine for a tab). */
  hint: string;
  to: string;
  /** Extra match text handed to Command's filter (hostname, tags). */
  keywords: string;
};

/**
 * Machines first, then their tabs — a tab entry only exists when the
 * machine's capabilities allow it, mirroring the detail page's own gating
 * (`null` capabilities = agent never reported = gate nothing).
 */
export function paletteEntries(rows: FleetRow[]): PaletteEntry[] {
  const out: PaletteEntry[] = [];
  for (const r of rows) {
    const name = displayName(r);
    const kw = `${r.hostname} ${r.tags.join(" ")}`;
    out.push({ key: r.id, label: name, hint: r.hostname, to: `/machines/${r.id}`, keywords: kw });
    const caps = r.capabilities;
    const has = (c: string) => caps === null || caps.includes(c);
    const tabs: [string, string, boolean][] = [
      ["Docker", "docker", has(CAP_DOCKER)],
      ["Units", "units", has(CAP_SYSTEMD)],
      ["Logs", "logs", has(CAP_JOURNAL)],
      ["Terminal", "terminal", true],
    ];
    for (const [label, tab, allowed] of tabs) {
      if (!allowed) continue;
      out.push({
        key: `${r.id}:${tab}`,
        label: `${name} — ${label}`,
        hint: r.hostname,
        to: `/machines/${r.id}?tab=${tab}`,
        keywords: kw,
      });
    }
  }
  return out;
}
```

Before writing `paletteEntries`, check `MachineDetailPage.tsx` for the tabs'
actual `?tab=` values and capability-gating conditions, and mirror them
exactly — the values above are the plan's best knowledge, the page is the
source of truth.

- [ ] **Step 3: Gates** — `npm --prefix frontend run typecheck && npm --prefix frontend run build`.

- [ ] **Step 4: Commit** — `git commit -am "feat(fleet): identity API client and pure fleet logic"`

---

### Task 7: Fleet page — search, chips, flat/grouped toggle

**Files:**
- Modify: `frontend/src/pages/FleetPage.tsx`

**Interfaces:**
- Consumes: `visibleFleet`, `fleetTags`, `groupFleet`, `displayName`, `FleetView` (Task 6). URL params: `q`, `tags` (comma-joined), `view` (`grouped` | absent=flat) via `useSearchParams`.

- [ ] **Step 1: Extract the current table into a local `FleetTable({ rows })` component** in the same file (the grouped view renders it once per section — extraction, not duplication).

- [ ] **Step 2: Hostname cell → identity cell.** The `AssetTag` shows `displayName(row)`; when `row.display_name !== null`, render the hostname beneath in `font-mono text-[11px] text-muted-foreground` (the machine-header treatment).

- [ ] **Step 3: Filter bar + URL state**, above the table (a `role="search"` form with `preventDefault`, like UnitsCard's):

```tsx
const [params, setParams] = useSearchParams();
const q = params.get("q") ?? "";
const selectedTags = (params.get("tags") ?? "").split(",").filter(Boolean);
const view: FleetView = params.get("view") === "grouped" ? "grouped" : "flat";

// One updater so every control writes URL state the same way; deleting
// empty params keeps URLs canonical (absent = default, per the design).
function setParam(key: string, value: string) {
  setParams((prev) => {
    const next = new URLSearchParams(prev);
    if (value === "") next.delete(key); else next.set(key, value);
    return next;
  }, { replace: true });
}
```

Controls: an `Input type="search"` bound to `q` (`aria-label="Filter machines"`, same styling as UnitsCard's filter); one `Badge`-styled toggle per `fleetTags(rows)` entry (clickable — render as `<button>` wrapping/using `Badge`, `variant` solid when selected, `outline` when not, with `aria-pressed`), toggling membership in `selectedTags`; an rnui `ToggleGroup` with `Flat` / `Grouped` items bound to `view`. Verify `ToggleGroup`'s controlled-value prop names against `index.d.ts` before wiring.

- [ ] **Step 4: Render** — `const filtered = visibleFleet(rows, q, selectedTags)`. Flat: `<FleetTable rows={filtered} />`. Grouped: for each `groupFleet(filtered)` section, a header (`font-display text-sm uppercase tracking-widest`, the tag name or `Untagged`, plus a muted count) then `<FleetTable rows={section.rows} />`. Keep the existing empty state for zero machines; add a "no machine matches the filter" `EmptyState` when `rows.length > 0 && filtered.length === 0` (mirroring UnitsCard).

- [ ] **Step 5: Gates + browser check** — typecheck + build; then verify in a real browser, BOTH themes: chips toggle, URL round-trips (paste the URL in a new tab → same view), grouped sections duplicate multi-tag machines, empty-filter state renders.

- [ ] **Step 6: Commit** — `git commit -am "feat(fleet): search, tag filter and grouped view on the fleet page"`

---

### Task 8: Machine detail — identity card

**Files:**
- Create: `frontend/src/components/MachineIdentity.tsx`
- Modify: `frontend/src/pages/MachineDetailPage.tsx` (render it in the Overview tab; show `displayName` in the page header with the hostname beneath when renamed), `frontend/src/lib/queries.ts` (add the mutation hook if the file's pattern calls for it — follow how `useUnitAction` is structured).

**Interfaces:**
- Consumes: `patchMachine` (Task 6), `MachineDetail` (has `display_name`, `notes`, `tags`), `useFleet`'s cached rows for the tag vocabulary (`fleetTags`).

- [ ] **Step 1: Component.** One card, three fields, one save model:

```tsx
// Identity editing for one machine: display name, tags, notes. All three
// commit through PATCH /api/machines/:id with an explicit Save — nothing
// saves on blur, so a stray edit can't persist silently (design "Machine
// detail"). Tags use the rnui Combobox chips surface with the fleet-wide
// vocabulary as suggestions; free entry stays allowed (free-form tags with
// autocomplete, not a curated list).
```

Structure: react-hook-form + zod (the SignIn.tsx pattern — `Controller`, rnui `Field`/`FieldLabel`/`FieldError`, `noValidate`):

```tsx
const identitySchema = z.object({
  display_name: z.string().max(64, "At most 64 characters.").optional(),
  tags: z.array(z.string()).max(16, "At most 16 tags."),
  notes: z.string().max(4000, "At most 4000 characters.").optional(),
});
```

Client-side zod mirrors only the cheap shape checks; the server remains the
authority (its 400 message renders in the card's error Alert). Defaults from
the loaded `MachineDetail` (`display_name ?? ""`, `tags`, `notes ?? ""`);
submit maps `""` → `null` for display_name/notes. The mutation calls
`patchMachine` and on success writes the returned `MachineDetail` into the
query cache (`queryClient.setQueryData(["machine", id], data)`) and
invalidates `["fleet"]`. Tags field: `Combobox` with `ComboboxChips` +
`ComboboxChipsInput`, `items` = `fleetTags(fleetRows).map((t) => t.tag)` —
check the Combobox multiple/chips API in `index.d.ts` first and follow it
exactly (the compiled-probe lesson: a bare compile proves nothing about
rendering — verify the chips render labels in the browser).

- [ ] **Step 2: Header treatment** in `MachineDetailPage.tsx`: the `PageHeader` title becomes `displayName(detail)`; when renamed, the hostname renders beneath/beside in muted mono (match how the header area already lays out its meta line).

- [ ] **Step 3: Gates + browser check** — typecheck, build; in the browser (both themes): rename → header and fleet page update; add a tag by typing a new one; add one from autocomplete; clear the name → falls back to hostname; server 400 (17 tags) renders in the card's Alert.

- [ ] **Step 4: Commit** — `git commit -am "feat(fleet): identity card on the machine detail page"`

---

### Task 9: Enroll page

**Files:**
- Create: `frontend/src/pages/EnrollPage.tsx`
- Modify: the route table (`App.tsx` — where `/machines/:id` is declared) adding `/enroll`; `frontend/src/app/AppShell.tsx` adding a sidebar nav entry ("Enroll", `Plus`-style lucide icon, matching existing nav items); `docs/DEV.md` "Enroll an agent" gains one leading line: the UI at `/enroll` is the normal path, psql remains the no-UI fallback.

**Interfaces:**
- Consumes: `mintToken`, `listTokens`, `revokeToken` (Task 6); react-hook-form + zod (SignIn pattern); rnui `CopyButton`, `Collapsible`, `AlertDialog` (revoke confirm), `Table`, `Badge`.

- [ ] **Step 1: Mint form** (top of page). Fields: label (required), display name (optional), tags (comma-separated `Input` — parsed with `.split(",").map(s => s.trim()).filter(Boolean)`; the Combobox chips editor is Task 8's concern, a plain input is fine here where the vocabulary is usually new), and an advanced `Collapsible` holding `max_uses` (number, default 1, checkbox "unlimited" ⇒ `null`) and `expires_in_hours` (number, default 24, checkbox "never expires" ⇒ `null`). zod schema:

```tsx
const mintSchema = z.object({
  name: z.string().min(1, "Enter a label.").max(64, "At most 64 characters."),
  display_name: z.string().max(64, "At most 64 characters.").optional(),
  tags: z.string().optional(), // comma-separated; parsed on submit
  unlimited_uses: z.boolean(),
  max_uses: z.coerce.number().int().min(1).optional(),
  never_expires: z.boolean(),
  expires_in_hours: z.coerce.number().int().min(1).max(8760).optional(),
});
```

- [ ] **Step 2: Result panel**, rendered from the mutation's success data (component state only — navigating away loses the raw token, which is the point; say so in the panel: "Shown once. The server stores only a hash."):
  - the raw token in a mono block with `CopyButton`;
  - a CA download link — `<a href="/api/ca.pem" download="argus-ca.crt">` (plain link: same-origin, cookie-authenticated);
  - the run block in a `<pre>` with its own `CopyButton`:

```
sudo -n env \
  ARGUS_AGENT_ENDPOINT=https://<agent-endpoint>:9443 \
  ARGUS_JOIN_TOKEN=<the minted token> \
  ARGUS_CA_CERT=/etc/argus/argus-ca.crt \
  ARGUS_DATA_DIR=/var/lib/argus-agent \
  ./argus-agent
```

  with a muted caption: "Replace `<agent-endpoint>` with the address agents reach the control plane on — Argus cannot know its externally routable address." The token IS substituted; the endpoint is NOT (design "Enroll page").

- [ ] **Step 3: Token table** below (TanStack query on `listTokens`, key `["enrollment-tokens"]`, invalidated by mint and revoke): columns label, name/tags it applies, uses (`uses`/`max_uses` or `∞`), expires (relative via the existing `formatRelative`), state, revoke. State derives in that order: `revoked` → "revoked"; `max_uses !== null && uses >= max_uses` → "used up"; `expires_at` in the past → "expired"; else "active" (as a `StatusBadge` tone: fail/warn/warn/ok). Revoke is an `AlertDialog` confirm; only "active" rows get the button.

- [ ] **Step 4: Gates + browser check** (both themes): mint with defaults → single-use/24h shown; copy buttons work; revoke flips the row; the DEV.md line reads correctly.

- [ ] **Step 5: Commit** — `git commit -am "feat(fleet): enroll page — mint tokens in the UI"`

---

### Task 10: Command palette

**Files:**
- Create: `frontend/src/components/CommandPalette.tsx`
- Modify: `frontend/src/app/AppShell.tsx` (mount it once; add the sidebar trigger button showing `⌘K`/`Ctrl K` via rnui `Kbd`).

**Interfaces:**
- Consumes: `paletteEntries` (Task 6), `useFleet` (existing hook), rnui `CommandDialog`/`CommandInput`/`CommandList`/`CommandEmpty`/`CommandGroup`/`CommandItem`, `useNavigate`.

- [ ] **Step 1: Component**

```tsx
// Ctrl/Cmd+K palette: machines and their tabs, plus static routes. Entirely
// client-side over the cached fleet list — nothing is fetched on open beyond
// what the fleet page already polls. The fleet query here is enabled only
// while the dialog is open, so mounting the palette app-wide does not add a
// permanent background poll on pages that don't otherwise need the fleet.
import { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import {
  CommandDialog,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@e412/rnui-react";
import { paletteEntries } from "../lib/fleet";
import { useFleet } from "../lib/queries";

export default function CommandPalette({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const navigate = useNavigate();
  const { data: rows = [] } = useFleet({ enabled: open });

  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if (e.key === "k" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        onOpenChange(!open);
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onOpenChange]);

  function go(to: string) {
    onOpenChange(false);
    navigate(to);
  }

  const entries = paletteEntries(rows);
  return (
    <CommandDialog open={open} onOpenChange={onOpenChange}>
      <CommandInput placeholder="Jump to a machine…" />
      <CommandList>
        <CommandEmpty>No matches.</CommandEmpty>
        <CommandGroup heading="Machines">
          {entries.map((e) => (
            <CommandItem key={e.key} value={`${e.label} ${e.keywords}`} onSelect={() => go(e.to)}>
              <span>{e.label}</span>
              <span className="ml-auto font-mono text-[11px] text-muted-foreground">{e.hint}</span>
            </CommandItem>
          ))}
        </CommandGroup>
        <CommandGroup heading="Pages">
          <CommandItem value="fleet machines" onSelect={() => go("/")}>Fleet</CommandItem>
          <CommandItem value="enroll token" onSelect={() => go("/enroll")}>Enroll a machine</CommandItem>
        </CommandGroup>
      </CommandList>
    </CommandDialog>
  );
}
```

`useFleet` may not accept options today — if not, extend it to pass
`enabled` through to `useQuery` (default `true`), rather than adding a
second hook. Verify `CommandDialog`'s prop names and `CommandItem`'s
`value`/`onSelect` contract against `index.d.ts` before assuming the
shadcn shapes above.

- [ ] **Step 2: Mount in AppShell** — `open` state lives in AppShell; the sidebar gets a trigger (a `SidebarMenuButton`-style entry or a footer control matching the existing three-control footer): a search icon, "Search", and `<Kbd>⌘K</Kbd>` (render `Ctrl` on non-Mac platforms via `navigator.platform.includes("Mac")` — one small helper, no dependency).

- [ ] **Step 3: Gates + browser check** (both themes): Ctrl+K opens/closes; typing a tag matches its machines; selecting a Logs entry lands on the machine's Logs tab; a machine with `capabilities: {}` (bare host) offers no Docker/Units/Logs entries; Escape closes.

- [ ] **Step 4: Commit** — `git commit -am "feat(fleet): Ctrl+K command palette"`

---

## Final verification (controller, not a task)

- Whole-branch review (subagent-driven-development's final reviewer).
- Live E2E per the spec's script, recorded in `docs/DEV.md`: mint single-use token with name+tag on `/enroll` → enroll a fresh agent with the pasted block → machine appears named+tagged → second use refused → rename/retag/notes from the detail page → fleet filter + grouped view + URL round-trip → Ctrl+K jump to Logs tab → audit rows for `machine.update`, `enroll_token.create`, `enroll_token.revoke` present.
- Browser pass in both themes on: fleet filter bar, grouped sections, identity card, enroll page, palette.
