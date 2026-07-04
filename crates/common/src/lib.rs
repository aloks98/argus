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

    // ---- Agent ----
    /// Control-plane agent endpoint, e.g. `https://agents.argus.lab.example`.
    pub const AGENT_ENDPOINT: &str = "ARGUS_AGENT_ENDPOINT";
    /// Enrollment join token.
    pub const JOIN_TOKEN: &str = "ARGUS_JOIN_TOKEN";
    /// Path to the baked-in Argus CA certificate (PEM).
    pub const CA_CERT_PATH: &str = "ARGUS_CA_CERT";
}
