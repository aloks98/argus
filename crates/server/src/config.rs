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

/// Optional as a whole (invariant: "some auth method configured", not "OIDC
/// specifically", local-admin design §4) -- `oidc_from_env_values` validates
/// all fields together, so a partial `OidcConfig` is unreachable. OIDC
/// handlers degrade to 404 rather than panic when this is `None`.
#[derive(Clone, Debug)]
pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub required_role: RequiredRole,
    pub roles_claim: String,
    pub scopes: Vec<String>,
    /// Duplicate of `Config.public_url`, kept here to build `redirect_uri`
    /// (OIDC-specific). The cookie `Secure` decision reads `Config.public_url`
    /// instead -- that decision isn't OIDC-specific and applies even when
    /// this struct doesn't exist (local-admin design §4).
    pub public_url: String,
    pub ca_cert_path: Option<String>,
}

impl OidcConfig {
    pub fn redirect_uri(&self) -> String {
        redirect_uri(&self.public_url)
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

/// Derived from how the deployment is reachable, not a toggle: it can weaken
/// a cookie flag on localhost; it can never disable authentication. Called
/// directly by every auth flow handler (local-admin design §4) so even a
/// local-admin-only deployment (no `OidcConfig`) gets a correct `Secure`.
pub(crate) fn cookie_secure(public_url: &str) -> bool {
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
    /// Required unconditionally, independent of OIDC: decides the session
    /// cookie's `Secure` attribute for every login method, local admin
    /// included (local-admin design §4). `OidcConfig.public_url` mirrors this
    /// to build the OIDC-specific `redirect_uri`.
    pub public_url: String,
    /// `None` is a valid, boot-succeeding state -- see `oidc_from_env_values`
    /// and the boot rule in `main.rs`.
    pub oidc: Option<OidcConfig>,
    /// Optional path to a bundled `argus-agent` binary (`ARGUS_AGENT_BINARY`).
    /// `None` is a valid, boot-succeeding state -- self-update just 503s.
    pub agent_binary_path: Option<String>,
}

impl Config {
    /// The port agents dial, parsed from `agent_addr` ("0.0.0.0:9443" ->
    /// 9443). Used to compose the advertised agent endpoints
    /// (`http::AppState.agent_endpoints`) -- if a load balancer remaps the
    /// port externally, the operator's `ARGUS_AGENT_SANS` hostnames still
    /// resolve, and the Enroll page's value can be edited; this only has to
    /// be right for the direct-exposure default.
    pub fn agent_port(&self) -> u16 {
        self.agent_addr
            .rsplit_once(':')
            .and_then(|(_, p)| p.parse().ok())
            .unwrap_or(9443)
    }

    pub fn from_env() -> Result<Self> {
        use argus_common::env;
        let public_url = req(env::PUBLIC_URL)?;

        // OIDC-configured-or-not is judged by OIDC-specific vars alone -- see
        // `public_url_for_oidc`'s doc for why `ARGUS_PUBLIC_URL` doesn't count.
        let oidc_issuer = std::env::var(env::OIDC_ISSUER).ok();
        let oidc_client_id = std::env::var(env::OIDC_CLIENT_ID).ok();
        let oidc_client_secret = std::env::var(env::OIDC_CLIENT_SECRET).ok();
        let oidc_required_role = std::env::var(env::OIDC_REQUIRED_ROLE).ok();
        let oidc_roles_claim = std::env::var(env::OIDC_ROLES_CLAIM).ok();
        let oidc_scopes = std::env::var(env::OIDC_SCOPES).ok();
        let oidc_ca_cert = std::env::var(env::OIDC_CA_CERT).ok();
        let oidc_public_url = public_url_for_oidc(
            &oidc_issuer,
            &oidc_client_id,
            &oidc_client_secret,
            &oidc_required_role,
            &oidc_roles_claim,
            &oidc_scopes,
            &oidc_ca_cert,
            &public_url,
        );

        Ok(Self {
            database_url: req(env::DATABASE_URL)?,
            field_key_b64: req(env::FIELD_KEY)?,
            http_addr: std::env::var(env::HTTP_ADDR).unwrap_or_else(|_| "0.0.0.0:8080".into()),
            agent_addr: std::env::var(env::AGENT_ADDR).unwrap_or_else(|_| "0.0.0.0:9443".into()),
            agent_sans: parse_agent_sans(std::env::var(env::AGENT_SANS).ok().as_deref()),
            oidc: oidc_from_env_values(
                oidc_issuer,
                oidc_client_id,
                oidc_client_secret,
                oidc_required_role,
                oidc_public_url,
                oidc_roles_claim,
                oidc_scopes,
                oidc_ca_cert,
            )?,
            public_url,
            agent_binary_path: std::env::var(env::AGENT_BINARY).ok(),
        })
    }
}

/// Whether to hand `ARGUS_PUBLIC_URL` to `oidc_from_env_values` at all.
/// Without this gate, a local-admin-only deployment (which still sets
/// `ARGUS_PUBLIC_URL`) reads as "1 of 5 OIDC variables set" and gets rejected
/// as partial config -- so intent is judged by ANY `ARGUS_OIDC_*` variable,
/// required or optional, instead.
#[allow(clippy::too_many_arguments)]
fn public_url_for_oidc(
    issuer: &Option<String>,
    client_id: &Option<String>,
    client_secret: &Option<String>,
    required_role: &Option<String>,
    roles_claim: &Option<String>,
    scopes: &Option<String>,
    ca_cert_path: &Option<String>,
    public_url: &str,
) -> Option<String> {
    let oidc_intent = issuer.is_some()
        || client_id.is_some()
        || client_secret.is_some()
        || required_role.is_some()
        || roles_claim.is_some()
        || scopes.is_some()
        || ca_cert_path.is_some();
    oidc_intent.then(|| public_url.to_string())
}

/// Built from already-read values so the all-or-nothing rule is testable
/// without mutating process env (racy in parallel tests).
///
/// Three outcomes: all five present -> `Ok(Some(_))`; none present ->
/// `Ok(None)` (valid local-admin-only state, design §4); some present ->
/// `Err` naming the first missing one -- a half-configured provider is a
/// mistake, not a mode, so it must not silently disable SSO.
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
            "OIDC is partially configured: {missing} is missing. Set all four \
             ARGUS_OIDC_* variables (ARGUS_OIDC_ISSUER, ARGUS_OIDC_CLIENT_ID, \
             ARGUS_OIDC_CLIENT_SECRET, ARGUS_OIDC_REQUIRED_ROLE), or none of \
             them, to run with only a local admin."
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

/// Rejects empty and whitespace-only values too: an unset value must never
/// mean open (design §5.2), and `ARGUS_OIDC_REQUIRED_ROLE=""` would otherwise
/// parse to `RequiredRole::Named("")`, silently admitting everyone if a
/// provider ever emits an empty string in a roles array or object key.
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
    fn agent_port_parses_the_listen_addr_and_falls_back() {
        let mut cfg = test_config();
        cfg.agent_addr = "0.0.0.0:9443".into();
        assert_eq!(cfg.agent_port(), 9443);
        cfg.agent_addr = "127.0.0.1:12345".into();
        assert_eq!(cfg.agent_port(), 12345);
        cfg.agent_addr = "garbage".into();
        assert_eq!(cfg.agent_port(), 9443, "unparseable addr falls back");
    }

    fn test_config() -> super::Config {
        super::Config {
            database_url: String::new(),
            field_key_b64: String::new(),
            http_addr: String::new(),
            agent_addr: "0.0.0.0:9443".into(),
            agent_sans: vec![],
            public_url: String::new(),
            oidc: None,
            agent_binary_path: None,
        }
    }

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

    /// Guards the ordering in `req()` -- see its doc for why.
    #[test]
    fn empty_required_role_is_rejected_not_named_empty() {
        let rejected = reject_empty("ARGUS_OIDC_REQUIRED_ROLE", "".to_string());
        assert!(rejected.is_err());
        // What an un-rejected value would become -- fails loudly if `reject_empty` is bypassed.
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
        // Whitespace must not silently become a role nobody holds, locking everyone out.
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

    use super::oidc_from_env_values;

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

    /// The state a local-admin-only deployment boots in.
    #[test]
    fn absent_oidc_config_is_none_not_an_error() {
        let got = oidc_from_env_values(None, None, None, None, None, None, None, None)
            .expect("absent config is a valid state");
        assert!(got.is_none());
    }

    /// Partially present -> hard error (see `oidc_from_env_values`'s doc).
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

    use super::public_url_for_oidc;

    /// The regression this test guards against -- see `public_url_for_oidc`'s doc.
    #[test]
    fn public_url_is_withheld_from_oidc_detection_absent_other_intent() {
        assert_eq!(
            public_url_for_oidc(
                &None,
                &None,
                &None,
                &None,
                &None,
                &None,
                &None,
                "http://localhost:8080"
            ),
            None
        );
    }

    /// Covers all seven `ARGUS_OIDC_*` variables (four required, three
    /// optional) since an optional one alone must also register as intent
    /// (see `public_url_for_oidc`'s doc).
    #[test]
    fn public_url_is_included_once_any_oidc_variable_is_set() {
        for (issuer, client_id, client_secret, required_role, roles_claim, scopes, ca_cert) in [
            (Some("i".to_string()), None, None, None, None, None, None),
            (None, Some("c".to_string()), None, None, None, None, None),
            (None, None, Some("s".to_string()), None, None, None, None),
            (None, None, None, Some("r".to_string()), None, None, None),
            (
                None,
                None,
                None,
                None,
                Some("groups".to_string()),
                None,
                None,
            ),
            (
                None,
                None,
                None,
                None,
                None,
                Some("openid".to_string()),
                None,
            ),
            (
                None,
                None,
                None,
                None,
                None,
                None,
                Some("/etc/argus/idp-ca.pem".to_string()),
            ),
        ] {
            assert_eq!(
                public_url_for_oidc(
                    &issuer,
                    &client_id,
                    &client_secret,
                    &required_role,
                    &roles_claim,
                    &scopes,
                    &ca_cert,
                    "http://localhost:8080"
                ),
                Some("http://localhost:8080".to_string())
            );
        }
    }
}
