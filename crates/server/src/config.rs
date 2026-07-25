use anyhow::{Context, Result};

/// Who is admitted after a successful authentication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequiredRole {
    /// Any authenticated user. Reachable ONLY by setting the variable to the
    /// literal `any` -- an unset variable refuses to boot instead, so a
    /// forgotten config value can never mean "open to everyone".
    Any,
    Named(String),
}

/// OIDC settings. All required fields are validated at startup; the server
/// refuses to boot without them, because the alternative to authentication
/// here is an unauthenticated root shell.
///
/// Held (behind `Arc`) in `AppState.oidc`; every field is read by the OIDC
/// flow handlers (`crate::auth::oidc`).
#[derive(Clone)]
pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub required_role: RequiredRole,
    pub roles_claim: String,
    pub scopes: Vec<String>,
    pub public_url: String,
    pub ca_cert_path: Option<String>,
}

impl OidcConfig {
    pub fn redirect_uri(&self) -> String {
        redirect_uri(&self.public_url)
    }
    pub fn cookie_secure(&self) -> bool {
        cookie_secure(&self.public_url)
    }
}

fn parse_required_role(raw: &str) -> RequiredRole {
    let t = raw.trim();
    if t.eq_ignore_ascii_case("any") {
        RequiredRole::Any
    } else {
        RequiredRole::Named(t.to_string())
    }
}

fn parse_scopes(raw: Option<&str>) -> Vec<String> {
    let parsed: Vec<String> = raw
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect();
    if parsed.is_empty() {
        vec!["openid".into(), "profile".into(), "email".into()]
    } else {
        parsed
    }
}

/// Built from configuration rather than from request headers: `Host` and
/// `X-Forwarded-Proto` are client-controlled, and the redirect URI must match
/// what is registered at the provider exactly.
fn redirect_uri(public_url: &str) -> String {
    format!("{}/auth/callback", public_url.trim_end_matches('/'))
}

/// Derived from how the deployment is reachable, not a toggle. It can weaken a
/// cookie flag on localhost; it can never disable authentication.
fn cookie_secure(public_url: &str) -> bool {
    public_url.starts_with("https://")
}

/// Control-plane configuration, sourced from environment variables (PRD §13).
#[derive(Clone)]
pub struct Config {
    pub database_url: String,
    /// Base64 AES-256-GCM key that field-encrypts the CA private key at rest.
    pub field_key_b64: String,
    pub http_addr: String,
    pub agent_addr: String,
    /// SANs (hostnames/IPs) for the control plane's own agent-surface TLS leaf.
    pub agent_sans: Vec<String>,
    /// Read by `http::serve` to build `AppState.oidc` (Task 5); the OIDC flow
    /// handlers (Task 6) read the fields inside it.
    pub oidc: OidcConfig,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        use argus_common::env;
        Ok(Self {
            database_url: req(env::DATABASE_URL)?,
            field_key_b64: req(env::FIELD_KEY)?,
            http_addr: std::env::var(env::HTTP_ADDR).unwrap_or_else(|_| "0.0.0.0:8080".into()),
            agent_addr: std::env::var(env::AGENT_ADDR).unwrap_or_else(|_| "0.0.0.0:9443".into()),
            agent_sans: parse_agent_sans(std::env::var(env::AGENT_SANS).ok().as_deref()),
            oidc: OidcConfig {
                issuer: req(env::OIDC_ISSUER)?,
                client_id: req(env::OIDC_CLIENT_ID)?,
                client_secret: req(env::OIDC_CLIENT_SECRET)?,
                required_role: parse_required_role(&req(env::OIDC_REQUIRED_ROLE)?),
                roles_claim: std::env::var(env::OIDC_ROLES_CLAIM)
                    .unwrap_or_else(|_| "groups".into()),
                scopes: parse_scopes(std::env::var(env::OIDC_SCOPES).ok().as_deref()),
                public_url: req(env::PUBLIC_URL)?,
                ca_cert_path: std::env::var(env::OIDC_CA_CERT).ok(),
            },
        })
    }
}

/// Read a required env var, rejecting empty and whitespace-only values.
///
/// Design §5.2 is explicit that an unset value must never mean open, and an
/// empty string is morally unset too: `ARGUS_OIDC_REQUIRED_ROLE=""` would
/// otherwise parse to `RequiredRole::Named("")`, and a provider that ever
/// emits an empty string in a roles array (or as an object key, for the
/// Zitadel shape) would silently admit everyone.
fn req(key: &str) -> Result<String> {
    let value = std::env::var(key).with_context(|| format!("missing required env var {key}"))?;
    reject_empty(key, value)
}

/// The validation half of `req`, split out so it's testable without mutating
/// real process env vars (racy under parallel tests, and `std::env::set_var`
/// is `unsafe` as of Rust 1.82).
fn reject_empty(key: &str, value: String) -> Result<String> {
    if value.trim().is_empty() {
        anyhow::bail!("env var {key} is set but empty");
    }
    Ok(value)
}

/// Parse a comma-separated SAN list, trimming whitespace and dropping empties.
/// Falls back to `localhost` + `127.0.0.1` when unset or empty.
fn parse_agent_sans(raw: Option<&str>) -> Vec<String> {
    let sans: Vec<String> = raw
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if sans.is_empty() {
        vec!["localhost".to_string(), "127.0.0.1".to_string()]
    } else {
        sans
    }
}

#[cfg(test)]
mod tests {
    use super::parse_agent_sans;

    #[test]
    fn parse_agent_sans_defaults_when_unset() {
        assert_eq!(parse_agent_sans(None), vec!["localhost", "127.0.0.1"]);
    }

    #[test]
    fn parse_agent_sans_defaults_when_empty() {
        assert_eq!(parse_agent_sans(Some("")), vec!["localhost", "127.0.0.1"]);
        assert_eq!(
            parse_agent_sans(Some("  ,  ,")),
            vec!["localhost", "127.0.0.1"]
        );
    }

    #[test]
    fn parse_agent_sans_trims_and_drops_empties() {
        assert_eq!(
            parse_agent_sans(Some("agents.argus.lab.example, 10.0.0.5 ,,")),
            vec!["agents.argus.lab.example", "10.0.0.5"]
        );
    }

    use super::{
        cookie_secure, parse_required_role, parse_scopes, redirect_uri, reject_empty, RequiredRole,
    };

    #[test]
    fn reject_empty_rejects_empty_and_whitespace() {
        assert!(reject_empty("ARGUS_OIDC_REQUIRED_ROLE", "".to_string()).is_err());
        assert!(reject_empty("ARGUS_OIDC_REQUIRED_ROLE", "   ".to_string()).is_err());
        assert!(reject_empty("ARGUS_OIDC_REQUIRED_ROLE", "\t\n".to_string()).is_err());
    }

    /// The property the fix exists for: `req()` -- `reject_empty` composed
    /// with the env lookup -- must reject `ARGUS_OIDC_REQUIRED_ROLE=""`
    /// BEFORE it ever reaches `parse_required_role`, so it can never become
    /// `RequiredRole::Named("")` (a theoretical open-admit path if a provider
    /// ever emits an empty string in a roles array or as an object key).
    #[test]
    fn empty_required_role_is_rejected_not_named_empty() {
        let rejected = reject_empty("ARGUS_OIDC_REQUIRED_ROLE", "".to_string());
        assert!(rejected.is_err());
        // Confirm what an un-rejected empty value WOULD have become, so this
        // test fails loudly if `reject_empty` is ever bypassed.
        assert_eq!(parse_required_role(""), RequiredRole::Named(String::new()));
    }

    #[test]
    fn reject_empty_passes_through_non_empty() {
        assert_eq!(
            reject_empty("ARGUS_OIDC_REQUIRED_ROLE", "argus-admin".to_string()).unwrap(),
            "argus-admin"
        );
    }

    #[test]
    fn required_role_any_is_explicit_not_absent() {
        assert_eq!(parse_required_role("any"), RequiredRole::Any);
        assert_eq!(parse_required_role("ANY"), RequiredRole::Any);
        assert_eq!(
            parse_required_role("argus-admin"),
            RequiredRole::Named("argus-admin".into())
        );
        // Whitespace around a real role name must not silently become a role
        // nobody holds, which would lock everyone out with no error.
        assert_eq!(
            parse_required_role("  argus-admin  "),
            RequiredRole::Named("argus-admin".into())
        );
    }

    #[test]
    fn cookie_secure_follows_the_public_url_scheme() {
        assert!(cookie_secure("https://argus.lab.example"));
        assert!(!cookie_secure("http://localhost:8080"));
    }

    #[test]
    fn redirect_uri_tolerates_a_trailing_slash() {
        assert_eq!(
            redirect_uri("https://argus.lab.example"),
            "https://argus.lab.example/auth/callback"
        );
        assert_eq!(
            redirect_uri("https://argus.lab.example/"),
            "https://argus.lab.example/auth/callback"
        );
    }

    #[test]
    fn scopes_default_and_split_on_whitespace() {
        assert_eq!(parse_scopes(None), vec!["openid", "profile", "email"]);
        assert_eq!(parse_scopes(Some("openid roles")), vec!["openid", "roles"]);
        assert_eq!(
            parse_scopes(Some("   ")),
            vec!["openid", "profile", "email"]
        );
    }
}
