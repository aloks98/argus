//! The local admin break-glass credential: provisioning/rotation/login
//! (design §6/§7/§8).
//!
//! `reset_local_admin` is the ONE implementation of "rotate" -- both the
//! `argus local-admin reset` CLI (`main.rs`) and `rotate` (`POST
//! /api/local-admin/rotate`, below) call this same function, so there is
//! never a second generate-hash-store sequence to drift out of sync with the
//! first.

use crate::http::AppState;
use crate::repo::{self, Actor, Identity};
use anyhow::Result;
use axum::{
    extract::State,
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Json, Response},
};
use axum_extra::extract::cookie::CookieJar;
use serde::Deserialize;
use sqlx::PgPool;
use std::time::{Duration, Instant};
use time::OffsetDateTime;

/// Generate a new password, store only its hash, and return the password for
/// one-time display. Shared by the CLI and the in-app rotation endpoint so
/// there is exactly one implementation of "rotate".
pub async fn reset_local_admin(pool: &PgPool, username: &str) -> Result<String> {
    let password = crate::auth::password::generate_password();
    let hash = crate::auth::password::hash_password(&password)?;
    crate::repo::upsert_local_admin(pool, username, &hash).await?;
    Ok(password)
}

#[derive(Deserialize)]
pub struct LoginRequest {
    username: String,
    password: String,
}

/// The one response for every failing case: wrong username, wrong password,
/// and no admin configured are indistinguishable (design §11) -- same status,
/// same body, and (critically) the same argon2id verification cost paid on
/// every path, so timing carries no signal either.
fn generic_failure() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "error": "invalid username or password" })),
    )
        .into_response()
}

/// `429` with `Retry-After`, emitted before the database is ever touched --
/// the whole point of consulting the limiter first (design §10).
fn rate_limited(retry_after: Duration) -> Response {
    // Round UP to whole seconds: `Retry-After` is a seconds-resolution header,
    // and rounding down could tell the client to retry before the limiter
    // would actually admit it.
    let secs = retry_after.as_millis().div_ceil(1000).max(1);
    let mut resp = (
        StatusCode::TOO_MANY_REQUESTS,
        Json(serde_json::json!({ "error": "too many attempts" })),
    )
        .into_response();
    if let Ok(value) = HeaderValue::from_str(&secs.to_string()) {
        resp.headers_mut().insert(header::RETRY_AFTER, value);
    }
    resp
}

/// `POST /auth/local` -- the break-glass login (design §8). Public, mounted
/// alongside the other `/auth/*` routes.
///
/// The properties that matter here:
/// 1. The limiter is consulted BEFORE any query, so a caller who is being
///    delayed never costs the database anything. `LoginLimiter::check`
///    itself RESERVES the slot it grants (see that module's doc comment) --
///    that is what keeps concurrent callers from all observing "under
///    budget" while each other's argon2 verify is still in flight.
/// 2. Wrong username, wrong password, and no-admin-configured all resolve
///    through exactly one `generic_failure()` return, and every one of those
///    paths pays the same argon2id verification cost -- the real hash when an
///    admin row exists, `password::DUMMY_PHC` when it doesn't. Skipping the
///    dummy verification is the bug design §11 exists to prevent: without it
///    "no local admin configured" would return in microseconds while a wrong
///    password takes ~100ms, leaking whether the credential exists at all.
/// 3. The argon2id verify runs inside `spawn_blocking`, never directly on the
///    async task. At ~100ms and ~19 MiB per call, running it in-line would
///    pin a tokio worker thread for that whole span; a few dozen concurrent
///    unauthenticated requests would then be enough to stall the ENTIRE
///    browser HTTP surface, which is a denial of service on exactly the
///    recovery path §10.2 exists to keep available.
/// 4. Success mints an ORDINARY session via the same `repo::create_session`
///    the OIDC callback uses, with the `local:` -prefixed subject that can
///    never collide with a provider's `sub`. There is no second session
///    concept.
pub async fn login(State(state): State<AppState>, Json(req): Json<LoginRequest>) -> Response {
    if let Some(retry_after) = state.limiter.check(Instant::now()) {
        return rate_limited(retry_after);
    }

    let admin = match repo::get_local_admin(&state.pool).await {
        Ok(admin) => admin,
        Err(e) => {
            tracing::error!(error = %e, "local login: failed to load the local admin row");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Exactly one argon2id verification happens below on every path -- real
    // hash if a row exists, the dummy hash if not -- so the cost (and hence
    // the timing) is identical regardless of which of the three failure
    // reasons applies. The username comparison is deliberately ordinary
    // (not constant-time): it is public information the moment ANY account
    // exists (the CLI's usage text names the default), unlike the password.
    //
    // Both arms move an owned `String` copy of the submitted password into
    // `spawn_blocking` -- never a reference to `req.password` -- so the
    // verify runs on tokio's blocking pool rather than pinning this request's
    // async worker for argon2id's ~100ms. Neither arm logs the password or
    // lets it reach a `Debug`/error path; only the boolean result crosses
    // back.
    let valid = match &admin {
        Some(row) => {
            let password = req.password.clone();
            let phc = row.password_hash.clone();
            let password_ok = tokio::task::spawn_blocking(move || {
                crate::auth::password::verify_password(&password, &phc)
            })
            .await
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, "local login: argon2 verify task panicked");
                false
            });
            password_ok && req.username == row.username
        }
        None => {
            let password = req.password.clone();
            if let Err(e) = tokio::task::spawn_blocking(move || {
                crate::auth::password::verify_against_dummy(&password)
            })
            .await
            {
                tracing::error!(error = %e, "local login: dummy verify task panicked");
            }
            false
        }
    };

    if !valid {
        // No `record_failure` call: `check` above already reserved this
        // attempt as a pessimistic failure BEFORE the verify ran, which is
        // the fix for the concurrency gap -- see `ratelimit`'s doc comment.
        if let Err(e) = repo::audit_with_detail(
            &state.pool,
            Actor::System,
            "auth.denied",
            None,
            "denied",
            serde_json::json!({ "method": "local" }),
        )
        .await
        {
            tracing::error!(error = %e, "local login: failed to write auth.denied audit row");
        }
        return generic_failure();
    }

    state.limiter.record_success();
    if let Err(e) = repo::touch_local_admin_login(&state.pool).await {
        // Not fatal to the login: this is bookkeeping (design §6's "cheapest
        // way to notice the break-glass credential being used"), not the
        // security decision itself.
        tracing::error!(error = %e, "local login: failed to stamp last_login_at");
    }

    // The `local:` prefix namespaces the subject so it can never collide with
    // a provider's `sub`, no matter what that provider issues (design §8).
    let identity = Identity {
        subject: "local:admin".into(),
        email: None,
        display_name: Some("Local admin".into()),
    };
    let (token_value, hash) = crate::auth::session::new_session_token();
    let expires_at =
        OffsetDateTime::now_utc() + time::Duration::hours(argus_common::SESSION_TTL_HOURS);
    // Reusing `repo::create_session` -- the SAME function the OIDC callback
    // calls -- is the point (design §8): the resulting session is ordinary,
    // so expiry, revocation, logout, the `/api/*` middleware and the audit
    // trail all behave identically, with no second session concept.
    if let Err(e) = repo::create_session(&state.pool, &hash, &identity, expires_at).await {
        tracing::error!(error = %e, "local login: failed to create session");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // Fail closed (CLAUDE.md: every verb goes through the audit log from the
    // start): if the row can't be written, revoke the session we just
    // created rather than let anyone sign in unaudited -- mirrors the OIDC
    // callback's own `auth.login` write.
    if let Err(e) = repo::audit_with_detail(
        &state.pool,
        Actor::User(&identity),
        "auth.login",
        None,
        "ok",
        serde_json::json!({ "method": "local" }),
    )
    .await
    {
        tracing::error!(error = %e, "local login: auth.login audit write failed; revoking session");
        if let Err(e) = repo::delete_session(&state.pool, &hash).await {
            tracing::error!(error = %e, "local login: failed to revoke unaudited session");
        }
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let jar = CookieJar::new().add(crate::auth::oidc::session_cookie(
        crate::config::cookie_secure(&state.public_url),
        token_value,
    ));
    // No explicit `StatusCode` here: `Json`'s default response status is
    // already `200 OK`, and `StatusCode` itself does not implement
    // `IntoResponseParts` (only `IntoResponse`), so it cannot ride alongside
    // the cookie jar in the same tuple.
    (jar, Json(serde_json::json!({ "ok": true }))).into_response()
}

/// `POST /api/local-admin/rotate` -- authenticated in-app rotation (design
/// §5.2). Mounted INSIDE the `/api` router so `require_auth` and
/// `SameSite=Lax` protect it exactly like every other verb; it is deliberately
/// NOT under `/auth`.
///
/// Calls `reset_local_admin` -- the one implementation of "rotate" -- rather
/// than re-running generate-hash-store itself. Reuses the current row's
/// username (falling back to `admin` only if no row exists yet) so rotating
/// through the browser can never silently rename an account the CLI created
/// under a different `--username`.
pub async fn rotate(
    State(state): State<AppState>,
    crate::auth::AuthUser(identity): crate::auth::AuthUser,
) -> Response {
    let username = match repo::get_local_admin(&state.pool).await {
        Ok(Some(row)) => row.username,
        Ok(None) => "admin".to_string(),
        Err(e) => {
            tracing::error!(error = %e, "local admin rotate: failed to load the local admin row");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let password = match reset_local_admin(&state.pool, &username).await {
        Ok(password) => password,
        Err(e) => {
            tracing::error!(error = %e, "local admin rotate: reset failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    if let Err(e) = repo::audit(
        &state.pool,
        Actor::User(&identity),
        "local_admin.rotate",
        None,
        "ok",
    )
    .await
    {
        tracing::error!(error = %e, "local admin rotate: audit write failed");
    }

    Json(serde_json::json!({ "password": password })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use sqlx::PgPool;

    /// A minimal `AppState` for driving `login` directly (no router/oneshot
    /// needed: it takes its extractors as plain arguments, mirroring
    /// `auth::oidc`'s own test helpers). No OIDC configured -- irrelevant to
    /// this handler -- but a fresh `LoginLimiter` and cipher, matching
    /// `auth::oidc::tests::test_app_state_no_oidc`.
    fn test_state(pool: PgPool) -> AppState {
        AppState {
            pool,
            hub: std::sync::Arc::new(crate::hub::Hub::default()),
            oidc: None,
            cipher: std::sync::Arc::new(
                crate::crypto::FieldCipher::from_b64_key(
                    &base64::engine::general_purpose::STANDARD.encode([4u8; 32]),
                )
                .expect("test cipher"),
            ),
            oidc_client: None,
            public_url: "http://localhost:8080".into(),
            limiter: std::sync::Arc::new(crate::auth::ratelimit::LoginLimiter::new()),
        }
    }

    /// The timing-indistinguishability property (design §11) has no coverage
    /// anywhere else: deleting `verify_against_dummy` from the `None` arm of
    /// `login` leaves every status/body/cookie assertion in `http.rs`
    /// unchanged -- the only observable difference is elapsed time. The real
    /// gap without the dummy verify is ~1000x (microseconds vs. argon2id's
    /// ~100ms); a 2x ("50%") tolerance is nowhere near tight enough to flake
    /// under CI jitter but is nowhere near loose enough to hide a skipped
    /// verify either.
    #[sqlx::test]
    async fn no_admin_and_wrong_password_cost_about_the_same_time(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        // Case 1: nothing configured at all -- the dummy-hash path.
        let t0 = std::time::Instant::now();
        let _ = login(
            State(test_state(pool.clone())),
            Json(LoginRequest {
                username: "admin".into(),
                password: "whatever-1".into(),
            }),
        )
        .await;
        let no_admin_elapsed = t0.elapsed();

        // Case 2: a row exists, but the password is wrong -- the real-hash
        // path. Fresh `test_state` (hence a fresh limiter) so neither case
        // pays for the other's reservation.
        let hash = crate::auth::password::hash_password("the-real-one")?;
        repo::upsert_local_admin(&pool, "admin", &hash).await?;
        let t1 = std::time::Instant::now();
        let _ = login(
            State(test_state(pool.clone())),
            Json(LoginRequest {
                username: "admin".into(),
                password: "wrong-2".into(),
            }),
        )
        .await;
        let wrong_password_elapsed = t1.elapsed();

        let (a, b) = (
            no_admin_elapsed.as_secs_f64(),
            wrong_password_elapsed.as_secs_f64(),
        );
        let ratio = a.max(b) / a.min(b).max(f64::EPSILON);
        assert!(
            ratio <= 2.0,
            "no-admin ({no_admin_elapsed:?}) and wrong-password ({wrong_password_elapsed:?}) \
             paths must cost about the same -- a {ratio:.1}x gap suggests the dummy \
             verification was skipped"
        );

        Ok(())
    }

    #[sqlx::test]
    async fn reset_generates_a_working_password_and_stores_only_its_hash(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let password = reset_local_admin(&pool, "admin").await?;
        assert_eq!(
            password.chars().count(),
            crate::auth::password::PASSWORD_LEN
        );

        let row = crate::repo::get_local_admin(&pool).await?.expect("row");
        assert_eq!(row.username, "admin");
        // The stored value must be a hash, and must not contain the password.
        assert!(row.password_hash.starts_with("$argon2id$"));
        assert!(!row.password_hash.contains(&password));
        assert!(crate::auth::password::verify_password(
            &password,
            &row.password_hash
        ));

        // Rotation issues a different password and invalidates the old one.
        let second = reset_local_admin(&pool, "admin").await?;
        assert_ne!(password, second);
        let row2 = crate::repo::get_local_admin(&pool).await?.expect("row");
        assert!(crate::auth::password::verify_password(
            &second,
            &row2.password_hash
        ));
        assert!(
            !crate::auth::password::verify_password(&password, &row2.password_hash),
            "the previous password must stop working after rotation"
        );
        Ok(())
    }
}
