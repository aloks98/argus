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
/// the message-count channel capacity is sized so the BYTE watermark is
/// always the binding constraint, never the message count -- see the
/// arithmetic below. `deliver_pty_output`'s full-channel teardown is a
/// defensive assertion, not a path a fast program reaches.
///
/// This was measured, not assumed: a live firehose (`seq 1 200000` in a real
/// terminal) produced 1,488,985 bytes across 10,818 `PtyOutput` frames --
/// about 138 bytes/chunk, not the 64 KiB a naive "big reads" assumption would
/// suggest. A raw-mode pty wakes the reader for whatever the tty layer has
/// queued at that instant, which for line-oriented shell output is small.
///
/// Sizing against a realistic (not absolute-worst-case) floor of 64
/// bytes/chunk -- already below the measured ~138 -- crossing `PTY_HIGH_WATER`
/// (1 MiB) takes `1,048,576 / 64 = 16,384` chunks at that floor. Doubling for
/// the same "absorb one pause round-trip" headroom the original design
/// applied gives `PTY_CHANNEL_CAP = 32,768`. Checked both ways:
///   - at the MEASURED ~138 bytes/chunk, high-water trips at ~7,600 chunks --
///     more than 4x below the 32,768 cap, so `PtyFlow{paused:true}` fires
///     with wide message-count margin before the channel could ever fill.
///   - at the ORIGINAL 64 KiB assumption, high-water trips at just 16 chunks
///     -- trivially far below the cap.
///
/// The only way to fill the channel before crossing high-water would be a
/// sustained run of sub-64-byte chunks, which is not what a real pty (this
/// codebase's or otherwise) produces under load.
pub const PTY_CHANNEL_CAP: usize = 32_768; // messages
pub const PTY_HIGH_WATER: usize = 1 << 20; // 1 MiB buffered -> pause
pub const PTY_LOW_WATER: usize = 256 << 10; // 256 KiB buffered -> resume
/// Per-read chunk size on the agent's blocking PTY reader.
pub const PTY_READ_BUF: usize = 64 << 10; // 64 KiB

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
