//! `reset_local_admin` is the ONE "rotate" implementation (design §6/§7/§8)
//! -- the CLI and `rotate` below both call it, so there's never a second
//! generate-hash-store sequence to drift out of sync.

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

/// Generates a new password, stores only its hash, returns it once for
/// display. Shared by the CLI and `rotate` -- the ONE "rotate" impl.
///
/// `hash_password` runs inside `spawn_blocking` HERE, not at either call
/// site, so `login`'s worker-thread-pinning defect class can't reappear at a
/// site that forgets to wrap it. `rotate` is authenticated (not `login`'s
/// unauthenticated-DoS risk) but pays the same cost on the same kind of
/// thread, so gets equal treatment.
pub async fn reset_local_admin(pool: &PgPool, username: &str) -> Result<String> {
    let password = crate::auth::password::generate_password();
    let hash = {
        let password = password.clone();
        tokio::task::spawn_blocking(move || crate::auth::password::hash_password(&password))
            .await
            .map_err(|e| anyhow::anyhow!("argon2 hash task panicked: {e}"))??
    };
    crate::repo::upsert_local_admin(pool, username, &hash).await?;
    Ok(password)
}

#[derive(Deserialize)]
pub struct LoginRequest {
    username: String,
    password: String,
}

/// The one response for every failing case (wrong username/password, no
/// admin configured are indistinguishable, design §11): same status, body,
/// and argon2id cost paid on every path, so timing carries no signal.
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
    // Round UP: `Retry-After` is seconds-resolution; rounding down could tell
    // the client to retry before the limiter would actually admit it.
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

/// `POST /auth/local` -- the break-glass login (design §8). Public.
///
/// Properties: (1) the limiter is consulted BEFORE any query -- see
/// `ratelimit`'s doc for why `check` reserves the slot it grants; (2) every
/// failure path -- wrong user, wrong password, no admin configured -- pays
/// the same argon2id cost via `generic_failure` (design §11); (3) that verify
/// always runs in `spawn_blocking`, never inline, to avoid pinning a tokio
/// worker under unauthenticated load; (4) success mints an ORDINARY session
/// via `repo::create_session`, the same path OIDC uses.
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

    // The username comparison is deliberately ordinary (not constant-time):
    // it is public info the moment ANY account exists, unlike the password.
    //
    // Both arms move an owned password copy into `spawn_blocking` -- never a
    // reference -- so argon2id's ~100ms runs on the blocking pool, not this
    // worker. Neither arm logs the password or lets it reach Debug/error;
    // only the boolean result crosses back.
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
        // No `record_failure` here: `check` already reserved this attempt as a
        // pessimistic failure before the verify ran (see `ratelimit`'s doc).
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
        // Not fatal: bookkeeping (design §6), not the security decision itself.
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
    // Reusing `repo::create_session` -- same as OIDC's callback -- is the
    // point (design §8): expiry, revocation, logout, `/api/*` middleware, and
    // the audit trail all behave identically. No second session concept.
    if let Err(e) = repo::create_session(&state.pool, &hash, &identity, expires_at).await {
        tracing::error!(error = %e, "local login: failed to create session");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // Fail closed (CLAUDE.md's audit rule): if this write fails, revoke the
    // session just created rather than let anyone sign in unaudited --
    // mirrors the OIDC callback's own `auth.login` write.
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
    // No explicit `StatusCode`: `Json`'s default is `200 OK`, and `StatusCode`
    // doesn't impl `IntoResponseParts` (only `IntoResponse`), so it can't ride
    // alongside the cookie jar in the same tuple.
    (jar, Json(serde_json::json!({ "ok": true }))).into_response()
}

/// `POST /api/local-admin/rotate` -- authenticated in-app rotation (design
/// §5.2). Mounted INSIDE `/api` so `require_auth`/`SameSite=Lax` protect it
/// like every other verb; deliberately NOT under `/auth`.
///
/// Reuses the current row's username (falling back to `admin` only if none
/// exists) so rotating through the browser can never silently rename an
/// account the CLI created under a different `--username`.
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

    // Deliberately fails OPEN, unlike `login`'s fail-closed audit write above:
    // rotation already destroyed the old credential, so withholding the
    // one-time replacement over an unrelated logging failure would leave the
    // caller with no working credential and no way to see the new one. The
    // CLI's `run_local_admin_cli` makes the identical call for the same reason.
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

    /// Mirrors `auth::oidc::tests::test_app_state_no_oidc` -- fresh limiter
    /// and cipher, no OIDC (irrelevant to this handler).
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
            agent_endpoints: vec!["https://agents.test:9443".into()],
            agent_binary: None,
        }
    }

    /// Only elapsed time reveals a skipped dummy verify (design §11) --
    /// every status/body/cookie stays unchanged otherwise. Real gap is ~1000x
    /// (microseconds vs ~100ms), so a 2x tolerance won't flake but won't hide
    /// a skip either.
    ///
    /// Deliberately ONE-SIDED: only "no-admin much FASTER" carries security
    /// meaning. Case 1 runs first and pays one-time costs (thread spawn,
    /// argon2 arena) that bias it slower -- a two-sided bound would flake on
    /// that bias.
    #[sqlx::test]
    async fn no_admin_path_is_not_much_faster_than_wrong_password(
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

        // Case 2: real-hash path (wrong password). Fresh `test_state`/limiter
        // so neither case pays for the other's reservation.
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

        let (no_admin, wrong_password) = (
            no_admin_elapsed.as_secs_f64(),
            wrong_password_elapsed.as_secs_f64(),
        );
        let ratio = wrong_password / no_admin.max(f64::EPSILON);
        assert!(
            ratio <= 2.0,
            "no-admin ({no_admin_elapsed:?}) must not be much faster than wrong-password \
             ({wrong_password_elapsed:?}) -- a {ratio:.1}x gap suggests the dummy \
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
        assert!(row.password_hash.starts_with("$argon2id$"));
        assert!(!row.password_hash.contains(&password));
        assert!(crate::auth::password::verify_password(
            &password,
            &row.password_hash
        ));

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
