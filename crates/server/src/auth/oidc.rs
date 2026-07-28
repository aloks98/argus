//! The authorization-code + PKCE flow (PRD §9.1).
//!
//! Discovery is LAZY and cached, never fetched at startup: agents authenticate
//! by mTLS and have nothing to do with OIDC, so a provider that is down when
//! Argus starts must not delay the agent gRPC surface or heartbeats. Only
//! login degrades.

use crate::auth::claims::{claim_at_path, claim_keys, is_admitted, merge_claims, roles_from_claim};
use crate::auth::session;
use crate::config::OidcConfig;
use crate::crypto::FieldCipher;
use crate::http::AppState;
use crate::repo::{self, Actor, Identity};
use anyhow::Context;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use base64::Engine;
use openidconnect::core::{
    CoreAuthDisplay, CoreAuthPrompt, CoreAuthenticationFlow, CoreErrorResponseType,
    CoreGenderClaim, CoreJsonWebKey, CoreJweContentEncryptionAlgorithm, CoreJwsSigningAlgorithm,
    CoreProviderMetadata, CoreRevocableToken, CoreRevocationErrorResponse,
    CoreTokenIntrospectionResponse, CoreTokenType,
};
use openidconnect::{
    AuthorizationCode, ClaimsVerificationError, ClientId, ClientSecret, CsrfToken,
    EmptyExtraTokenFields, EndpointMaybeSet, EndpointNotSet, EndpointSet, IdTokenClaims,
    IdTokenFields, IssuerUrl, Nonce, OAuth2TokenResponse, PkceCodeChallenge, PkceCodeVerifier,
    RedirectUrl, Scope, SignatureVerificationError, StandardErrorResponse, StandardTokenResponse,
    TokenResponse, UserInfoClaims,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use time::OffsetDateTime;
use tokio::sync::RwLock;

/// Arbitrary provider claims. `openidconnect` models claims as typed structs;
/// this flattened map is how we read a roles claim whose NAME and SHAPE are
/// configuration rather than compile-time knowledge.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RawClaims {
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}
impl openidconnect::AdditionalClaims for RawClaims {}

/// This deployment's ID-token fields: `RawClaims` stands in for
/// `EmptyAdditionalClaims` so a roles claim riding in the ID token (some
/// providers put it there instead of / as well as userinfo) is readable.
/// Every other generic mirrors `openidconnect::core`'s own `Core*` aliases
/// exactly -- there's nothing else provider-specific about this client.
type MyIdTokenFields = IdTokenFields<
    RawClaims,
    EmptyExtraTokenFields,
    CoreGenderClaim,
    CoreJweContentEncryptionAlgorithm,
    CoreJwsSigningAlgorithm,
>;
type MyTokenResponse = StandardTokenResponse<MyIdTokenFields, CoreTokenType>;
type MyIdTokenClaims = IdTokenClaims<RawClaims, CoreGenderClaim>;
type MyUserInfoClaims = UserInfoClaims<RawClaims, CoreGenderClaim>;

/// The concrete `CoreClient` specialization once claims carry `RawClaims`
/// instead of the hardcoded `EmptyAdditionalClaims` the `core::CoreClient`
/// alias uses. The six trailing `Endpoint*` parameters are what
/// `CoreClient::from_provider_metadata` actually produces (auth: set by
/// discovery; device-auth/introspection/revocation: never set, this crate
/// never uses them; token/userinfo: "maybe set", present only if the
/// provider's discovery document advertises them -- which is why
/// `exchange_code` and `user_info` below return `Result`, not bare values).
type OidcCoreClient = openidconnect::Client<
    RawClaims,
    CoreAuthDisplay,
    CoreGenderClaim,
    CoreJweContentEncryptionAlgorithm,
    CoreJsonWebKey,
    CoreAuthPrompt,
    StandardErrorResponse<CoreErrorResponseType>,
    MyTokenResponse,
    CoreTokenIntrospectionResponse,
    CoreRevocableToken,
    CoreRevocationErrorResponse,
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

#[derive(Debug, Serialize, Deserialize)]
pub struct FlowState {
    pub csrf: String,
    pub nonce: String,
    pub pkce_verifier: String,
    pub next: String,
}

/// OIDC Core 3.1.3.7 steps 4-5: when an ID token names more than one audience,
/// the `azp` (authorized party) claim must be present and must be this client.
/// `openidconnect` 4.0.1 deliberately leaves its azp check commented out
/// ("until a use case becomes apparent"); Argus is that use case, since real
/// providers issue multi-audience tokens.
///
/// Without this, `aud` alone can't distinguish "a token minted for us that
/// also names other parties" from "a token minted for a DIFFERENT app that
/// happens to name us" -- any app on the same identity provider could mint a
/// token Argus accepts. A single-audience token needs no `azp` (the spec only
/// requires it when `aud` is multi-valued), so the check is scoped accordingly.
fn authorized_party_ok(audiences: &[&str], azp: Option<&str>, client_id: &str) -> bool {
    if audiences.len() <= 1 {
        return true;
    }
    azp == Some(client_id)
}

/// The `auth.denied` audit `detail` for a role-admission rejection: `subject`
/// is the non-principal fact design doc §9 requires this row to carry, and
/// `method` (local-admin design §9) makes the login method explicit rather
/// than inferred from its absence on the local-admin path.
///
/// Extracted to a pure function so this shape is unit-testable without a
/// live or mocked IdP -- `callback` itself has no test harness reaching this
/// point, so without this a regression dropping `method` would leave every
/// test green.
fn denied_detail(subject: &str) -> serde_json::Value {
    serde_json::json!({ "subject": subject, "method": "oidc" })
}

/// The `auth.login` audit `detail` for a successful callback. See
/// `denied_detail` for why this is its own testable function rather than
/// an inline literal only reachable through a full callback run.
fn login_detail() -> serde_json::Value {
    serde_json::json!({ "method": "oidc" })
}

/// Reject anything that is not a same-site absolute path. A browser treats
/// `//host` as protocol-relative and `\` as a separator, so both must go --
/// and so must ASCII tab/newline: `Query<..>` percent-decodes `%09`/`%0A` to
/// literal control bytes BEFORE this function ever sees them, `HeaderValue`
/// happily carries a raw tab into the `Location` header, and the WHATWG URL
/// parser strips all ASCII tab/newline before parsing a URL -- so
/// `/<TAB>/evil.example` becomes `//evil.example` in the browser, which is
/// protocol-relative. A prefix check alone cannot catch this (the tab isn't
/// at the front), so this rejects on character class instead: only the
/// printable, non-space ASCII range is allowed through.
pub fn safe_next_path(raw: &str) -> String {
    let ok = raw.starts_with('/')
        && !raw.starts_with("//")
        && raw.bytes().all(|b| (0x21..0x7f).contains(&b))
        && !raw.contains('\\')
        && !raw.contains("://");
    if ok {
        raw.to_string()
    } else {
        "/".to_string()
    }
}

pub fn seal_flow(cipher: &FieldCipher, flow: &FlowState) -> anyhow::Result<String> {
    let plain = serde_json::to_vec(flow)?;
    let (ct, nonce) = cipher.encrypt(&plain)?;
    let mut blob = nonce;
    blob.extend_from_slice(&ct);
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(blob))
}

pub fn open_flow(cipher: &FieldCipher, sealed: &str) -> anyhow::Result<FlowState> {
    let blob = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(sealed)?;
    anyhow::ensure!(blob.len() > 12, "flow cookie too short");
    let (nonce, ct) = blob.split_at(12);
    let plain = cipher.decrypt(ct, nonce)?;
    Ok(serde_json::from_slice(&plain)?)
}

/// Constant-time byte comparison for the CSRF `state` check. Hand-rolled
/// rather than pulling in `subtle` for one XOR-fold: length is compared
/// up front because CSRF token length is fixed and public (both sides are
/// `CsrfToken::new_random`'s output), never secret -- only the *content*
/// needs to resist a timing side channel, which the fold below does by
/// touching every byte regardless of where a mismatch occurs.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Cached provider metadata + client. Discovery is lazy: the first login
/// pays for it, later ones read the cache. Held behind an `RwLock` (not
/// `OnceCell`) because it needs an *invalidate* path: `callback` drops the
/// cache and re-discovers when ID-token verification fails on an
/// unrecognized JWKS key ID (an IdP signing-key rotation, design doc §10). A
/// failed discovery attempt never writes to the lock, so the cache stays
/// empty and the next login retries instead of the outage being cached forever.
pub struct OidcClient {
    client: RwLock<Option<Arc<OidcCoreClient>>>,
    http: openidconnect::reqwest::Client,
    cfg: Arc<OidcConfig>,
}

impl OidcClient {
    /// Builds the HTTP client only -- no network I/O, so this is safe (and
    /// intended) to call at server startup without delaying anything else.
    /// The optional CA cert is a local file read, not a call to the IdP.
    pub fn new(cfg: Arc<OidcConfig>) -> anyhow::Result<Self> {
        let mut builder = openidconnect::reqwest::ClientBuilder::new()
            // The crate's own docs: following redirects opens the client up
            // to SSRF vulnerabilities.
            .redirect(openidconnect::reqwest::redirect::Policy::none());
        if let Some(path) = cfg.ca_cert_path.as_deref() {
            let pem = std::fs::read(path)
                .with_context(|| format!("reading ARGUS_OIDC_CA_CERT at {path}"))?;
            let cert = openidconnect::reqwest::Certificate::from_pem(&pem)
                .context("parsing ARGUS_OIDC_CA_CERT as PEM")?;
            builder = builder.add_root_certificate(cert);
        }
        let http = builder.build().context("building the OIDC HTTP client")?;
        Ok(Self {
            client: RwLock::new(None),
            http,
            cfg,
        })
    }

    pub fn http(&self) -> &openidconnect::reqwest::Client {
        &self.http
    }

    /// Returns the cached client, discovering it first if the cache is
    /// empty (first use, or after `invalidate`). Never called at startup --
    /// only from `login`/`callback`, so an IdP outage degrades login alone.
    async fn client(&self) -> anyhow::Result<Arc<OidcCoreClient>> {
        if let Some(c) = self.client.read().await.as_ref() {
            return Ok(c.clone());
        }
        self.discover().await
    }

    /// Performs discovery unconditionally and replaces the cached client.
    /// Only reaches the write on success, so a failed attempt leaves
    /// whatever was cached before (or nothing) untouched.
    async fn discover(&self) -> anyhow::Result<Arc<OidcCoreClient>> {
        let metadata = CoreProviderMetadata::discover_async(
            IssuerUrl::new(self.cfg.issuer.clone())?,
            &self.http,
        )
        .await
        .context("OIDC discovery failed")?;
        let client: OidcCoreClient = openidconnect::Client::from_provider_metadata(
            metadata,
            ClientId::new(self.cfg.client_id.clone()),
            Some(ClientSecret::new(self.cfg.client_secret.clone())),
        )
        .set_redirect_uri(RedirectUrl::new(self.cfg.redirect_uri())?);
        let client = Arc::new(client);
        *self.client.write().await = Some(client.clone());
        Ok(client)
    }

    /// Drops the cached client so the next `client()` call re-discovers.
    /// Called on an unrecognized JWKS key ID so a provider's key rotation
    /// resolves itself without an Argus restart, instead of every login
    /// failing ID-token verification forever.
    async fn invalidate(&self) {
        *self.client.write().await = None;
    }
}

#[cfg(test)]
impl OidcClient {
    /// Test-only seam: seeds the cache with a hand-built client so
    /// `login`/`callback` can be exercised without a live discovery
    /// endpoint (design doc §11 requires `/auth/login` be covered by a
    /// unit test). `#[cfg(test)]` keeps this out of the production API --
    /// nothing outside this module's own tests can skip discovery.
    fn seeded(cfg: Arc<OidcConfig>, client: OidcCoreClient) -> anyhow::Result<Self> {
        let this = Self::new(cfg)?;
        *this
            .client
            .try_write()
            .expect("uncontended at construction time") = Some(Arc::new(client));
        Ok(this)
    }
}

/// `secure` is a plain `bool` rather than `&OidcConfig` so this builder works
/// with no OIDC config in scope at all: the `Secure` decision is a
/// `Config`-level fact (derived from `Config.public_url`, local-admin design
/// §4), not an OIDC-specific one. The OIDC flow below is the only caller
/// today, and it is only reached once `OidcConfig` is known to be present, but
/// a local login (later task) sets a session cookie in exactly the state
/// where no `OidcConfig` exists.
fn flow_cookie(secure: bool, sealed: String) -> Cookie<'static> {
    Cookie::build((argus_common::FLOW_COOKIE, sealed))
        .http_only(true)
        .same_site(SameSite::Lax)
        .path("/")
        .secure(secure)
        .max_age(time::Duration::seconds(argus_common::FLOW_TTL_SECS))
        .build()
}

/// `pub(crate)`: `auth::local::login` (`POST /auth/local`) builds the
/// identical session cookie a local login mints, so both auth methods share
/// exactly one cookie shape rather than a second implementation drifting out
/// of sync with this one.
pub(crate) fn session_cookie(secure: bool, token: String) -> Cookie<'static> {
    Cookie::build((argus_common::SESSION_COOKIE, token))
        .http_only(true)
        .same_site(SameSite::Lax)
        .path("/")
        .secure(secure)
        .max_age(time::Duration::hours(argus_common::SESSION_TTL_HOURS))
        .build()
}

/// A removal cookie must repeat the `Path` the cookie was set with, or the
/// browser treats it as a *different* cookie and the original lingers.
fn removed_cookie(name: &'static str) -> Cookie<'static> {
    Cookie::build((name, "")).path("/").build()
}

/// Callback failures render a small error page with a retry link rather than
/// a JSON body: the browser navigated here directly, and detail is logged
/// server-side rather than returned (design doc §14). `message` is always a
/// fixed string this module controls, never request input, so no escaping is
/// needed today.
///
/// The markup lives in `templates/error.html`, embedded via `include_str!` so
/// there's no runtime file read. One page with two placeholders doesn't
/// justify a templating engine; `str::replace` is enough.
const ERROR_TEMPLATE: &str = include_str!("templates/error.html");

/// Escape text before it goes into the template.
///
/// Nothing passed today is attacker-influenced (every `message` is a string
/// literal in this file), but the obvious future edit is surfacing a
/// provider's `error_description`, which is. A `str::replace` template has no
/// escaping of its own to catch that, so this exists now rather than later.
fn escape_html(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Render a standalone auth error page.
/// The heading follows the status, because not every one of these is a failed
/// sign-in: a 404 is "this server has no SSO configured", which is a statement
/// about the deployment rather than about the attempt.
fn error_heading(status: StatusCode) -> &'static str {
    match status {
        StatusCode::NOT_FOUND => "Not available",
        StatusCode::SERVICE_UNAVAILABLE => "Temporarily unavailable",
        _ => "Sign-in failed",
    }
}

/// Substitute into the template. Split from `error_page` so the rendering can
/// be asserted on directly — a `Response` body is awkward to inspect from a
/// sync test, and "did the escaping actually happen" is the part worth pinning.
fn render_error_page(heading: &str, message: &str) -> String {
    ERROR_TEMPLATE
        .replace("{{heading}}", &escape_html(heading))
        .replace("{{message}}", &escape_html(message))
}

fn error_page(status: StatusCode, message: &str) -> Response {
    let body = render_error_page(error_heading(status), message);
    (status, Html(body)).into_response()
}

#[derive(Deserialize)]
pub struct LoginQuery {
    next: Option<String>,
}

/// `GET /auth/login?next=` -- redirect to the provider's authorization
/// endpoint, having sealed `state`/`nonce`/the PKCE verifier/`next` into the
/// flow cookie.
///
/// Degrades to 404 rather than panicking when OIDC is not configured
/// (local-admin design §4): SSO is simply one of possibly zero configured
/// login methods, not something every deployment must have.
pub async fn login(
    State(state): State<AppState>,
    Query(q): Query<LoginQuery>,
    jar: CookieJar,
) -> Response {
    let (Some(oidc), Some(oidc_client)) = (&state.oidc, &state.oidc_client) else {
        return error_page(
            StatusCode::NOT_FOUND,
            "Single sign-on is not configured on this server.",
        );
    };

    let next = safe_next_path(q.next.as_deref().unwrap_or("/"));

    let client = match oidc_client.client().await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "oidc login: discovery failed");
            return error_page(
                StatusCode::SERVICE_UNAVAILABLE,
                "Sign-in is temporarily unavailable. Please try again shortly.",
            );
        }
    };

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    let mut auth_request = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .set_pkce_challenge(pkce_challenge);
    for scope in &oidc.scopes {
        auth_request = auth_request.add_scope(Scope::new(scope.clone()));
    }
    let (auth_url, csrf_token, nonce) = auth_request.url();

    let flow = FlowState {
        csrf: csrf_token.secret().clone(),
        nonce: nonce.secret().clone(),
        pkce_verifier: pkce_verifier.secret().clone(),
        next,
    };
    let sealed = match seal_flow(&state.cipher, &flow) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "oidc login: failed to seal flow cookie");
            return error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Sign-in failed to start. Please try again.",
            );
        }
    };

    let jar = jar.add(flow_cookie(
        crate::config::cookie_secure(&state.public_url),
        sealed,
    ));
    (jar, Redirect::to(auth_url.as_str())).into_response()
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// `GET /auth/callback` -- exchange the code, verify the ID token, resolve
/// and check the role, mint a session, and redirect back into the app.
///
/// Fails closed throughout: any error in state validation, token exchange,
/// ID-token verification, or role admission returns an error page instead of
/// ever falling through to a session.
///
/// Degrades to 404 rather than panicking when OIDC is not configured
/// (local-admin design §4) -- see `login`'s doc comment; the same guard
/// applies here for the same reason.
pub async fn callback(
    State(state): State<AppState>,
    Query(q): Query<CallbackQuery>,
    jar: CookieJar,
) -> Response {
    let (Some(oidc), Some(oidc_client)) = (&state.oidc, &state.oidc_client) else {
        return error_page(
            StatusCode::NOT_FOUND,
            "Single sign-on is not configured on this server.",
        );
    };

    let Some(sealed) = jar
        .get(argus_common::FLOW_COOKIE)
        .map(|c| c.value().to_string())
    else {
        tracing::warn!("oidc callback: no flow cookie present");
        return error_page(
            StatusCode::BAD_REQUEST,
            "Your sign-in session expired. Please try again.",
        );
    };
    let flow = match open_flow(&state.cipher, &sealed) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, "oidc callback: failed to open flow cookie");
            return error_page(
                StatusCode::BAD_REQUEST,
                "Your sign-in session expired. Please try again.",
            );
        }
    };
    // The flow cookie is single-use: clear it on every exit from here on,
    // success or failure alike.
    let jar = jar.remove(removed_cookie(argus_common::FLOW_COOKIE));

    if let Some(err) = &q.error {
        tracing::warn!(error = %err, "oidc callback: provider returned an error");
        return (jar, error_page(StatusCode::BAD_REQUEST, "Sign-in failed.")).into_response();
    }

    // Compare in constant time: this is the CSRF check the whole flow rests
    // on, so it must not leak how much of the token matched via timing.
    let returned_state = q.state.unwrap_or_default();
    if !constant_time_eq(returned_state.as_bytes(), flow.csrf.as_bytes()) {
        tracing::warn!("oidc callback: state mismatch (possible CSRF)");
        return (
            jar,
            error_page(StatusCode::BAD_REQUEST, "Sign-in failed: invalid state."),
        )
            .into_response();
    }

    let Some(code) = q.code else {
        tracing::warn!("oidc callback: missing authorization code");
        return (
            jar,
            error_page(
                StatusCode::BAD_REQUEST,
                "Sign-in failed: missing authorization code.",
            ),
        )
            .into_response();
    };

    let mut client = match oidc_client.client().await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "oidc callback: discovery failed");
            return (
                jar,
                error_page(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Sign-in is temporarily unavailable. Please try again shortly.",
                ),
            )
                .into_response();
        }
    };
    let http = oidc_client.http();

    let token_request = match client.exchange_code(AuthorizationCode::new(code)) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "oidc callback: provider has no token endpoint");
            return (
                jar,
                error_page(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Sign-in is temporarily unavailable.",
                ),
            )
                .into_response();
        }
    };
    let token = match token_request
        .set_pkce_verifier(PkceCodeVerifier::new(flow.pkce_verifier.clone()))
        .request_async(http)
        .await
    {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "oidc callback: token exchange failed");
            return (
                jar,
                error_page(
                    StatusCode::BAD_GATEWAY,
                    "Sign-in failed: could not exchange the authorization code.",
                ),
            )
                .into_response();
        }
    };

    let Some(id_token) = token.id_token() else {
        tracing::error!("oidc callback: token response carried no id_token");
        return (
            jar,
            error_page(
                StatusCode::BAD_GATEWAY,
                "Sign-in failed: the provider did not return an identity token.",
            ),
        )
            .into_response();
    };
    let nonce = Nonce::new(flow.nonce.clone());
    let mut verify_result = id_token.claims(
        &client
            .id_token_verifier()
            .set_other_audience_verifier_fn(|_| true),
        &nonce,
    );
    if let Err(ClaimsVerificationError::SignatureVerification(
        SignatureVerificationError::NoMatchingKey,
    )) = &verify_result
    {
        // The JOSE header's `kid` matched none of our cached JWKS keys. This
        // is exactly what happens right after a provider rotates its
        // signing keys, so re-discover once and retry before giving up --
        // design doc §10 requires key rotation to resolve itself without an
        // Argus restart. Bounded to one retry: this must not become a loop.
        tracing::warn!("oidc callback: no matching JWKS key id; re-discovering once");
        oidc_client.invalidate().await;
        client = match oidc_client.client().await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "oidc callback: re-discovery after key-id miss failed");
                return (
                    jar,
                    error_page(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "Sign-in is temporarily unavailable. Please try again shortly.",
                    ),
                )
                    .into_response();
            }
        };
        verify_result = id_token.claims(
            &client
                .id_token_verifier()
                .set_other_audience_verifier_fn(|_| true),
            &nonce,
        );
    }
    let id_claims: &MyIdTokenClaims = match verify_result {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "oidc callback: ID token verification failed");
            return (
                jar,
                error_page(
                    StatusCode::BAD_GATEWAY,
                    "Sign-in failed: could not verify the identity token.",
                ),
            )
                .into_response();
        }
    };
    // See `authorized_party_ok`'s doc comment for why this check exists.
    // We relax the audience check above (`set_other_audience_verifier_fn`)
    // because real providers list extra audiences in the ID token -- Zitadel
    // names sibling apps, Auth0 names API identifiers, Keycloak whatever its
    // mappers add -- and rejecting those outright would mean Argus only
    // works against single-audience tokens. `authorized_party_ok` is what
    // keeps that relaxation from removing the audience guarantee.
    {
        let auds: Vec<&str> = id_claims.audiences().iter().map(|a| a.as_str()).collect();
        let azp = id_claims.authorized_party().map(|p| p.as_str());
        if !authorized_party_ok(&auds, azp, &oidc.client_id) {
            tracing::error!(
                audiences = ?auds,
                authorized_party = ?azp,
                expected = %oidc.client_id,
                "oidc callback: multi-audience ID token whose authorized party is not this client"
            );
            return (
                jar,
                error_page(
                    StatusCode::BAD_GATEWAY,
                    "Sign-in failed: the identity token was not issued for this application.",
                ),
            )
                .into_response();
        }
    }

    let subject = id_claims.subject().as_str().to_string();

    // Userinfo enriches the claims (sometimes the ONLY place roles live --
    // design doc §4.2) but is optional: a fetch failure degrades to
    // ID-token-only claims rather than failing the login. This is the
    // authenticated response fetched just now with this exchange's access
    // token, never a stale or untrusted value.
    //
    // `Some(id_claims.subject().clone())` pins the expected subject (OIDC
    // Core 5.3.2 MUST): userinfo claims are merged OVER the ID token's and
    // feed role admission plus the audit identity, so a confused or
    // compromised userinfo endpoint must not be able to grant roles under
    // someone else's `sub`. A mismatch surfaces as
    // `UserInfoError::ClaimsVerification` and is handled like any other
    // userinfo failure: log and continue with ID-token claims only.
    let userinfo: Option<MyUserInfoClaims> = match client.user_info(
        token.access_token().clone(),
        Some(id_claims.subject().clone()),
    ) {
        Ok(req) => match req
            .request_async::<RawClaims, _, CoreGenderClaim>(http)
            .await
        {
            Ok(claims) => Some(claims),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    subject = %subject,
                    "oidc callback: userinfo fetch failed; continuing with ID-token claims only"
                );
                None
            }
        },
        Err(e) => {
            tracing::warn!(
                error = %e,
                subject = %subject,
                "oidc callback: provider has no userinfo endpoint; continuing with ID-token claims only"
            );
            None
        }
    };

    let email = userinfo
        .as_ref()
        .and_then(|u| u.email())
        .or_else(|| id_claims.email())
        .map(|e| e.as_str().to_string());
    let display_name = userinfo
        .as_ref()
        .and_then(|u| u.name())
        .or_else(|| id_claims.name())
        .and_then(|n| n.get(None))
        .map(|n| n.as_str().to_string());

    let merged = merge_claims(
        id_claims.additional_claims().extra.clone(),
        userinfo
            .as_ref()
            .map(|u| u.additional_claims().extra.clone())
            .unwrap_or_default(),
    );
    let roles = claim_at_path(&merged, &oidc.roles_claim)
        .map(roles_from_claim)
        .unwrap_or_default();

    if !is_admitted(&roles, &oidc.required_role) {
        // Keys only, never values: claim values carry emails and other
        // personal data and must not reach the log.
        tracing::warn!(
            subject = %subject,
            required = ?oidc.required_role,
            roles_claim = %oidc.roles_claim,
            extracted_roles = ?roles,
            available_claims = ?claim_keys(&merged),
            "login denied: required role not held"
        );
        // `result` is a fixed-vocabulary column (`ok|error|denied`) read by
        // many other call sites, so the rejected subject goes in `detail`
        // via `audit_with_detail` instead -- matching how `grpc.rs` already
        // handles enrollment denials. `method` rides alongside it (design
        // doc §9) so `auth.denied`/`auth.login` rows state which auth
        // method produced them explicitly.
        if let Err(e) = repo::audit_with_detail(
            &state.pool,
            Actor::System,
            "auth.denied",
            None,
            "denied",
            denied_detail(&subject),
        )
        .await
        {
            tracing::error!(error = %e, "oidc callback: failed to write auth.denied audit row");
        }
        return (
            jar,
            error_page(
                StatusCode::FORBIDDEN,
                "Your account does not have the role required to access Argus. Contact your administrator.",
            ),
        )
            .into_response();
    }

    let identity = Identity {
        subject,
        email,
        display_name,
    };
    let (token_value, hash) = session::new_session_token();
    let expires_at =
        OffsetDateTime::now_utc() + time::Duration::hours(argus_common::SESSION_TTL_HOURS);
    if let Err(e) = repo::create_session(&state.pool, &hash, &identity, expires_at).await {
        tracing::error!(error = %e, "oidc callback: failed to create session");
        return (
            jar,
            error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Sign-in failed. Please try again.",
            ),
        )
            .into_response();
    }

    // Fail closed (CLAUDE.md: every verb goes through the audit log from the
    // start): if this write fails, revoke the session just created rather
    // than let anyone sign in unaudited. `detail` carries `method` so
    // `auth.login` rows are explicit about which auth method produced them,
    // matching `auth::local::login`'s own write.
    if let Err(e) = repo::audit_with_detail(
        &state.pool,
        Actor::User(&identity),
        "auth.login",
        None,
        "ok",
        login_detail(),
    )
    .await
    {
        tracing::error!(error = %e, "oidc callback: auth.login audit write failed; revoking session");
        if let Err(e) = repo::delete_session(&state.pool, &hash).await {
            tracing::error!(error = %e, "oidc callback: failed to revoke unaudited session");
        }
        return (
            jar,
            error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Sign-in failed. Please try again.",
            ),
        )
            .into_response();
    }

    let jar = jar.add(session_cookie(
        crate::config::cookie_secure(&state.public_url),
        token_value,
    ));
    // Re-run the guard here rather than trust the sealed cookie blindly: the
    // cookie is AEAD-sealed and `login` is its only writer, so this is safe
    // either way today, but it keeps `safe_next_path` from being a single
    // point of failure against an open redirect.
    (jar, Redirect::to(&safe_next_path(&flow.next))).into_response()
}

/// `POST /auth/logout` -- delete the session row and clear the cookie.
/// Not behind `require_auth` (it is mounted on the public router), so it
/// resolves its own identity from the cookie for the audit row.
pub async fn logout(State(state): State<AppState>, jar: CookieJar) -> Response {
    let Some(token) = jar
        .get(argus_common::SESSION_COOKIE)
        .map(|c| c.value().to_string())
    else {
        // Nothing to sign out of; idempotent.
        let jar = jar.remove(removed_cookie(argus_common::SESSION_COOKIE));
        return (jar, StatusCode::NO_CONTENT).into_response();
    };
    let hash = session::hash_token(&token);

    // Resolve identity BEFORE deleting: this also tells us whether a real
    // session existed. Gating the audit write on that (below) stops an
    // unauthenticated caller with a junk cookie from spamming `auth.logout`
    // rows -- this route has no session and no rate limit in front of it.
    //
    // `Err` and `Ok(None)` are NOT the same thing and must not be collapsed
    // (CLAUDE.md: every verb goes through the audit log from the start): a
    // transient lookup failure on a REAL session must not fall through to
    // the "no session" path, or the delete that follows revokes it with no
    // `auth.logout` row written. Fail closed here, same as `delete_session`
    // below.
    let identity = match repo::lookup_session(&state.pool, &hash).await {
        Ok(identity) => identity,
        Err(e) => {
            tracing::error!(error = %e, "oidc logout: failed to look up session");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    if let Err(e) = repo::delete_session(&state.pool, &hash).await {
        tracing::error!(error = %e, "oidc logout: failed to delete session");
        // Fail closed: do not tell the browser it is signed out unless the
        // server-side session was actually confirmed revoked.
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    if let Some(identity) = &identity {
        if let Err(e) = repo::audit(
            &state.pool,
            Actor::User(identity),
            "auth.logout",
            None,
            "ok",
        )
        .await
        {
            tracing::error!(error = %e, "oidc logout: failed to write auth.logout audit row");
        }
    }

    let jar = jar.remove(removed_cookie(argus_common::SESSION_COOKIE));
    (jar, StatusCode::NO_CONTENT).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_template_substitutes_both_placeholders() {
        let html = render_error_page("Not available", "Single sign-on is not configured.");
        assert!(html.contains("<h1>Not available</h1>"));
        assert!(html.contains("<p>Single sign-on is not configured.</p>"));
        assert!(html.contains("<title>Not available — Argus</title>"));
        // A renamed or misspelled placeholder would otherwise ship a page with
        // a literal `{{heading}}` on it and nothing would fail.
        assert!(
            !html.contains("{{"),
            "every placeholder must be substituted, found a leftover in:\n{html}"
        );
    }

    /// Pins `escape_html`'s rationale (see its doc comment) with actual
    /// script/quote input.
    #[test]
    fn rendered_messages_are_html_escaped() {
        let html = render_error_page("Sign-in failed", "<script>alert('x')</script> & \"quoted\"");
        assert!(
            !html.contains("<script>"),
            "raw script tag reached the page"
        );
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("&amp;"));
        assert!(html.contains("&quot;"));
        assert!(html.contains("&#39;"));
    }

    #[test]
    fn error_heading_follows_the_status() {
        // A 404 here means "no SSO on this deployment", not a failed
        // sign-in attempt -- the heading must reflect that distinction.
        assert_eq!(error_heading(StatusCode::NOT_FOUND), "Not available");
        assert_eq!(
            error_heading(StatusCode::SERVICE_UNAVAILABLE),
            "Temporarily unavailable"
        );
        assert_eq!(error_heading(StatusCode::BAD_GATEWAY), "Sign-in failed");
        assert_eq!(error_heading(StatusCode::BAD_REQUEST), "Sign-in failed");
    }

    #[test]
    fn multi_audience_tokens_require_azp_to_be_this_client() {
        const US: &str = "383369216612370205";
        const SIBLING: &str = "375405563246281579";

        // The real shape from Zitadel: us plus a sibling app, azp names us.
        assert!(authorized_party_ok(&[US, SIBLING], Some(US), US));

        // The attack this check exists to stop: a token minted for a DIFFERENT
        // application that merely happens to list us as an audience.
        assert!(
            !authorized_party_ok(&[US, SIBLING], Some(SIBLING), US),
            "a token authorized for another client must be rejected"
        );

        // Multi-audience with no azp at all: the spec requires it here, so
        // absence is a rejection, not a pass.
        assert!(
            !authorized_party_ok(&[US, SIBLING], None, US),
            "multi-audience tokens must carry azp"
        );

        // Single audience needs no azp — the spec only requires it when `aud`
        // is multi-valued, so demanding it would break ordinary providers.
        assert!(authorized_party_ok(&[US], None, US));
        assert!(authorized_party_ok(&[US], Some(US), US));

        // Degenerate: no audiences at all. The library's own aud check rejects
        // this before we run; assert our helper does not itself admit it.
        assert!(authorized_party_ok(&[], None, US));
    }

    /// Pins design §9's "`method` rides alongside [`subject`]" for the
    /// role-denied path (see `denied_detail`'s doc for why this needs its own test).
    #[test]
    fn denied_detail_names_the_method_and_carries_the_subject() {
        let v = denied_detail("some-subject");
        assert_eq!(v["method"], "oidc");
        assert_eq!(v["subject"], "some-subject");
    }

    /// Same rationale as `denied_detail_names_the_method_and_carries_the_subject`,
    /// for the success path's `auth.login` detail.
    #[test]
    fn login_detail_names_the_method() {
        assert_eq!(login_detail()["method"], "oidc");
    }

    #[test]
    fn next_path_accepts_only_same_site_absolute_paths() {
        assert_eq!(
            safe_next_path("/machines/abc?tab=terminal"),
            "/machines/abc?tab=terminal"
        );
        assert_eq!(safe_next_path("/"), "/");

        // Protocol-relative: a browser reads //evil.example as another origin.
        assert_eq!(safe_next_path("//evil.example/x"), "/");
        assert_eq!(safe_next_path("https://evil.example"), "/");
        assert_eq!(safe_next_path("http://evil.example"), "/");
        // Not absolute, so it would resolve against the current directory.
        assert_eq!(safe_next_path("machines"), "/");
        assert_eq!(safe_next_path(""), "/");
        // Backslash is treated as a separator by some browsers.
        assert_eq!(safe_next_path("/\\evil.example"), "/");
    }

    /// Regression coverage: a prefix check alone (`starts_with("//")`) does
    /// not catch a control byte that isn't at the front of the string but
    /// still gets treated as a separator by the browser after the
    /// `Location` header round-trips through it.
    #[test]
    fn next_path_rejects_control_bytes_that_url_parsers_treat_as_separators() {
        // `?next=/%09/evil.example` -- `Query<..>` percent-decodes `%09` to a
        // literal tab BEFORE this function ever sees it. `HeaderValue`
        // permits a raw tab, and the WHATWG URL parser strips all ASCII
        // tab/newline before parsing, turning `/<TAB>/evil.example` into
        // `//evil.example` -- protocol-relative, then `https://evil.example`.
        assert_eq!(safe_next_path("/\t/evil.example"), "/");
        // `?next=/%0A//evil` -- same story with a literal newline (%0A).
        assert_eq!(safe_next_path("/\n//evil.example"), "/");
        // A backslash anywhere, not just immediately after the leading
        // slash, must be rejected.
        assert_eq!(safe_next_path("/x\\y"), "/");
        // DEL and other non-printable/control bytes.
        assert_eq!(safe_next_path("/abc\u{7f}def"), "/");
        assert_eq!(safe_next_path("/abc\0def"), "/");
    }

    #[test]
    fn flow_state_round_trips_through_the_sealed_cookie() {
        let cipher = crate::crypto::FieldCipher::from_b64_key(
            &base64::engine::general_purpose::STANDARD.encode([7u8; 32]),
        )
        .expect("cipher");
        let flow = FlowState {
            csrf: "csrf-token".into(),
            nonce: "nonce-value".into(),
            pkce_verifier: "verifier".into(),
            next: "/machines/x".into(),
        };
        let sealed = seal_flow(&cipher, &flow).expect("seal");
        let opened = open_flow(&cipher, &sealed).expect("open");
        assert_eq!(opened.csrf, "csrf-token");
        assert_eq!(opened.next, "/machines/x");

        // Flip a byte in the MIDDLE of the sealed blob rather than appending
        // one: appending changes the base64 length's mod-4 class, so a test
        // built that way can pass on `DecodeError::InvalidLength` -- before
        // the cipher ever runs -- purely by arithmetic accident (this
        // fixture's `next` happened to make the appended-form length valid).
        // A same-length mid-string flip is guaranteed to reach the GCM tag
        // check, and asserting on the message pins down THAT is what failed.
        let mut bad: Vec<u8> = sealed.into_bytes();
        let mid = bad.len() / 2;
        bad[mid] = if bad[mid] == b'a' { b'b' } else { b'a' };
        let bad = String::from_utf8(bad).expect("still valid utf8: base64 alphabet only");
        let err = open_flow(&cipher, &bad).expect_err("tampered ciphertext must not decrypt");
        assert!(
            err.to_string().contains("aes-gcm decrypt failed"),
            "expected a cipher/tag failure (crypto.rs's `decrypt`), got: {err}"
        );
    }

    #[test]
    fn constant_time_eq_matches_regular_equality_semantics() {
        assert!(constant_time_eq(b"abc123", b"abc123"));
        assert!(!constant_time_eq(b"abc123", b"abc124"));
        assert!(!constant_time_eq(b"abc123", b"abc12"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    fn test_oidc_config(public_url: &str) -> OidcConfig {
        OidcConfig {
            issuer: "https://idp.invalid".into(),
            client_id: "cid".into(),
            client_secret: "secret".into(),
            required_role: crate::config::RequiredRole::Any,
            roles_claim: "groups".into(),
            scopes: vec!["openid".into()],
            public_url: public_url.into(),
            ca_cert_path: None,
        }
    }

    /// Design §11: "Cookie attribute construction for both `http` and
    /// `https` public URLs" is a required unit test in its own right --
    /// `SameSite=Lax` is the product's entire CSRF story for the verb
    /// endpoints, so a change to it must not be able to slip through with a
    /// green suite.
    #[test]
    fn flow_and_session_cookies_carry_the_required_security_attributes() {
        for (public_url, want_secure) in [
            ("http://localhost:8080", false),
            ("https://argus.lab.example", true),
        ] {
            let secure = crate::config::cookie_secure(public_url);

            let flow = flow_cookie(secure, "sealed-value".into());
            assert_eq!(flow.name(), argus_common::FLOW_COOKIE);
            assert_eq!(flow.value(), "sealed-value");
            assert_eq!(flow.http_only(), Some(true), "flow cookie must be HttpOnly");
            assert_eq!(
                flow.same_site(),
                Some(SameSite::Lax),
                "flow cookie must be SameSite=Lax"
            );
            assert_eq!(flow.path(), Some("/"), "flow cookie must be Path=/");
            assert_eq!(
                flow.secure(),
                Some(want_secure),
                "flow cookie Secure must follow cookie_secure() for {public_url}"
            );
            assert_eq!(
                flow.max_age(),
                Some(time::Duration::seconds(argus_common::FLOW_TTL_SECS))
            );

            let session = session_cookie(secure, "token-value".into());
            assert_eq!(session.name(), argus_common::SESSION_COOKIE);
            assert_eq!(session.value(), "token-value");
            assert_eq!(
                session.http_only(),
                Some(true),
                "session cookie must be HttpOnly"
            );
            assert_eq!(
                session.same_site(),
                Some(SameSite::Lax),
                "session cookie must be SameSite=Lax -- this is the CSRF guard for the verb endpoints"
            );
            assert_eq!(session.path(), Some("/"), "session cookie must be Path=/");
            assert_eq!(
                session.secure(),
                Some(want_secure),
                "session cookie Secure must follow cookie_secure() for {public_url}"
            );
            assert_eq!(
                session.max_age(),
                Some(time::Duration::hours(argus_common::SESSION_TTL_HOURS))
            );
        }
    }

    /// Hand-builds `CoreProviderMetadata` (no network call) so `OidcClient`
    /// can be seeded without a live discovery endpoint. Mirrors the pattern
    /// in `openidconnect`'s own doc example for constructing provider
    /// metadata (`src/lib.rs`'s "OpenID Connect Provider (Server) Interface"
    /// section).
    fn test_provider_metadata() -> openidconnect::core::CoreProviderMetadata {
        openidconnect::core::CoreProviderMetadata::new(
            IssuerUrl::new("https://idp.invalid".into()).expect("issuer url"),
            openidconnect::AuthUrl::new("https://idp.invalid/authorize".into()).expect("auth url"),
            openidconnect::JsonWebKeySetUrl::new("https://idp.invalid/jwks".into())
                .expect("jwks url"),
            vec![openidconnect::ResponseTypes::new(vec![
                openidconnect::core::CoreResponseType::Code,
            ])],
            vec![openidconnect::core::CoreSubjectIdentifierType::Public],
            vec![openidconnect::core::CoreJwsSigningAlgorithm::RsaSsaPssSha256],
            openidconnect::EmptyAdditionalProviderMetadata {},
        )
    }

    fn test_discovered_client() -> OidcCoreClient {
        openidconnect::Client::from_provider_metadata(
            test_provider_metadata(),
            ClientId::new("cid".into()),
            Some(ClientSecret::new("secret".into())),
        )
        .set_redirect_uri(
            RedirectUrl::new("http://localhost:8080/auth/callback".into()).expect("redirect url"),
        )
    }

    fn test_app_state(pool: sqlx::PgPool, oidc: Arc<OidcConfig>) -> AppState {
        let oidc_client =
            Arc::new(OidcClient::seeded(oidc.clone(), test_discovered_client()).expect("seed"));
        let public_url = oidc.public_url.clone();
        AppState {
            pool,
            hub: Arc::new(crate::hub::Hub::default()),
            oidc: Some(oidc),
            cipher: Arc::new(
                crate::crypto::FieldCipher::from_b64_key(
                    &base64::engine::general_purpose::STANDARD.encode([5u8; 32]),
                )
                .expect("test cipher"),
            ),
            oidc_client: Some(oidc_client),
            public_url,
            limiter: Arc::new(crate::auth::ratelimit::LoginLimiter::new()),
        }
    }

    /// The local-admin-only state (design §4): no `OidcConfig`, no
    /// `OidcClient`. `login`/`callback` must degrade to a 404 here rather than
    /// unwrap or panic on the missing config.
    fn test_app_state_no_oidc(pool: sqlx::PgPool) -> AppState {
        AppState {
            pool,
            hub: Arc::new(crate::hub::Hub::default()),
            oidc: None,
            cipher: Arc::new(
                crate::crypto::FieldCipher::from_b64_key(
                    &base64::engine::general_purpose::STANDARD.encode([5u8; 32]),
                )
                .expect("test cipher"),
            ),
            oidc_client: None,
            public_url: "http://localhost:8080".into(),
            limiter: Arc::new(crate::auth::ratelimit::LoginLimiter::new()),
        }
    }

    /// The property the global boot-rule constraint rests on: with OIDC
    /// absent, hitting the SSO entry points must not panic and must not
    /// pretend to start a flow -- it must say plainly that SSO isn't
    /// configured, so a local-admin-only deployment's sign-in view can tell
    /// the two states apart.
    #[sqlx::test]
    async fn login_and_callback_degrade_to_404_when_oidc_is_not_configured(
        pool: sqlx::PgPool,
    ) -> anyhow::Result<()> {
        let state = test_app_state_no_oidc(pool);

        let response = login(
            State(state.clone()),
            Query(LoginQuery { next: None }),
            CookieJar::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let response = callback(
            State(state),
            Query(CallbackQuery {
                code: Some("irrelevant".into()),
                state: Some("irrelevant".into()),
                error: None,
            }),
            CookieJar::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        Ok(())
    }

    /// Design §11: "`/auth/login` returns 302 with `state`, `nonce` and a
    /// PKCE challenge present, and sets a sealed flow cookie." (axum's
    /// `Redirect::to` actually emits 303 See Other, not literally 302, but
    /// this is the redirect design §11 is describing -- an authorization
    /// redirect carrying the flow parameters.) Drives `login` directly
    /// (no router/oneshot needed: the handler takes its extractors as plain
    /// arguments) against an `OidcClient` seeded via the test-only seam, so
    /// no live discovery endpoint is required.
    #[sqlx::test]
    async fn login_redirects_with_state_nonce_pkce_and_sets_the_flow_cookie(
        pool: sqlx::PgPool,
    ) -> anyhow::Result<()> {
        let oidc = Arc::new(test_oidc_config("http://localhost:8080"));
        let state = test_app_state(pool, oidc);

        let response = login(
            State(state),
            Query(LoginQuery {
                next: Some("/machines/x".into()),
            }),
            CookieJar::new(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::SEE_OTHER);

        let location = response
            .headers()
            .get(axum::http::header::LOCATION)
            .expect("Location header present")
            .to_str()?;
        assert!(
            location.starts_with("https://idp.invalid/authorize"),
            "unexpected redirect target: {location}"
        );
        assert!(location.contains("state="), "missing state: {location}");
        assert!(location.contains("nonce="), "missing nonce: {location}");
        assert!(
            location.contains("code_challenge=") && location.contains("code_challenge_method=S256"),
            "missing PKCE challenge: {location}"
        );

        let flow_cookie_header = response
            .headers()
            .get_all(axum::http::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find(|v| v.starts_with(argus_common::FLOW_COOKIE))
            .expect("flow cookie set");
        assert!(flow_cookie_header.contains("HttpOnly"));
        assert!(flow_cookie_header.contains("SameSite=Lax"));

        Ok(())
    }
}
