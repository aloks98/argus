//! Constants and small shared types used by both the control plane and the agent.

/// Default heartbeat cadence the server advertises in `HelloAck`.
pub const DEFAULT_HEARTBEAT_SECS: u32 = 15;

/// Default metrics sampling cadence on the agent.
pub const DEFAULT_METRICS_SECS: u32 = 15;

/// Metrics retention window; rows older than this are pruned nightly (PRD §6.3).
pub const METRICS_RETENTION_HOURS: i64 = 48;

/// Where the agent persists its identity (private key + issued client cert).
pub const AGENT_DATA_DIR: &str = "/var/lib/argus";

/// Connection-level control frames use stream id 0; sub-streams use ids > 0.
pub const CONTROL_STREAM_ID: u64 = 0;

/// Idle timeout for an interactive terminal, reset only by keystroke input.
/// Input-only on purpose: it answers "is a human still driving this?", so a
/// walked-away root shell closes even while a `top` keeps producing output.
pub const TERMINAL_IDLE_SECS: u64 = 1800;

/// PTY output buffering (server side). The byte water marks drive `PtyFlow`;
/// `PTY_CHANNEL_CAP` (message count) is sized so the BYTE watermark is always
/// the binding constraint, never the message count -- the only way to fill
/// the channel before crossing high-water would be a sustained run of
/// sub-64-byte chunks, which a real pty doesn't produce under load.
/// `deliver_pty_output`'s full-channel teardown is a defensive assertion,
/// not a path a fast program reaches.
///
/// A live firehose (`seq 1 200000` in a real terminal) measured ~138
/// bytes/chunk, far below the 64 KiB a naive "big reads" assumption would
/// suggest. Sizing against a realistic 64-bytes/chunk floor, crossing
/// `PTY_HIGH_WATER` (1 MiB) takes 16,384 chunks; doubling for headroom gives
/// `PTY_CHANNEL_CAP = 32,768` -- more than 4x the measured trip point.
pub const PTY_CHANNEL_CAP: usize = 32_768; // messages
pub const PTY_HIGH_WATER: usize = 1 << 20; // 1 MiB buffered -> pause
pub const PTY_LOW_WATER: usize = 256 << 10; // 256 KiB buffered -> resume
/// Per-read chunk size on the agent's blocking PTY reader.
pub const PTY_READ_BUF: usize = 64 << 10; // 64 KiB

/// Browser session lifetime. A security property of the product rather than a
/// per-deployment knob, which is why it lives here beside the other timeouts
/// instead of in the environment.
pub const SESSION_TTL_HOURS: i64 = 12;

/// Cookie holding the opaque session token. Only its sha256 is ever stored.
pub const SESSION_COOKIE: &str = "argus_session";

/// Short-lived cookie holding the sealed in-flight login (state, nonce, PKCE
/// verifier, return path). Ten minutes; expires by itself, so it needs no table.
pub const FLOW_COOKIE: &str = "argus_login";
pub const FLOW_TTL_SECS: i64 = 600;

/// Capability names reported by the agent on `AgentInfo` and stored in
/// `machines.capabilities`. Both binaries import these so a capability is never
/// spelled as a string literal on either side of the wire.
pub const CAP_SYSTEMD: &str = "systemd";
pub const CAP_DOCKER: &str = "docker";
pub const CAP_JOURNAL: &str = "journal";

/// Environment variable names shared across both binaries.
pub mod env {
    // ---- Control plane ----
    /// Postgres connection string.
    pub const DATABASE_URL: &str = "ARGUS_DATABASE_URL";
    /// Base64 AES-256-GCM key that field-encrypts the CA private key at rest.
    pub const FIELD_KEY: &str = "ARGUS_FIELD_KEY";
    /// Listen address for the browser HTTP surface (Traefik upstream).
    pub const HTTP_ADDR: &str = "ARGUS_HTTP_ADDR";
    /// Listen address for the agent mTLS gRPC surface (MetalLB upstream).
    pub const AGENT_ADDR: &str = "ARGUS_AGENT_ADDR";
    /// Comma-separated SANs for the control plane's own agent-surface TLS leaf
    /// (hostnames/IPs agents dial). Defaults to localhost.
    pub const AGENT_SANS: &str = "ARGUS_AGENT_SANS";
    /// OIDC issuer URL; every endpoint is read from its discovery document.
    pub const OIDC_ISSUER: &str = "ARGUS_OIDC_ISSUER";
    pub const OIDC_CLIENT_ID: &str = "ARGUS_OIDC_CLIENT_ID";
    pub const OIDC_CLIENT_SECRET: &str = "ARGUS_OIDC_CLIENT_SECRET";
    /// Role required for admission, or the literal `any`.
    pub const OIDC_REQUIRED_ROLE: &str = "ARGUS_OIDC_REQUIRED_ROLE";
    /// Dot-path to the roles claim (Keycloak nests: `realm_access.roles`).
    pub const OIDC_ROLES_CLAIM: &str = "ARGUS_OIDC_ROLES_CLAIM";
    pub const OIDC_SCOPES: &str = "ARGUS_OIDC_SCOPES";
    /// PEM for an IdP behind an internal CA.
    pub const OIDC_CA_CERT: &str = "ARGUS_OIDC_CA_CERT";
    /// Externally reachable base URL; builds the redirect URI and decides the
    /// session cookie's `Secure` attribute.
    pub const PUBLIC_URL: &str = "ARGUS_PUBLIC_URL";

    // ---- Agent ----
    /// Control-plane agent endpoint, e.g. `https://agents.argus.lab.example`.
    pub const AGENT_ENDPOINT: &str = "ARGUS_AGENT_ENDPOINT";
    /// Enrollment join token.
    pub const JOIN_TOKEN: &str = "ARGUS_JOIN_TOKEN";
    /// Path to the baked-in Argus CA certificate (PEM).
    pub const CA_CERT_PATH: &str = "ARGUS_CA_CERT";
    /// Directory where the agent persists its key + issued cert. Overrides the
    /// default [`super::AGENT_DATA_DIR`] (useful for local dev / non-root runs).
    pub const DATA_DIR: &str = "ARGUS_DATA_DIR";
}
