//! Enrollment handshake (PRD §5.2). The highest-risk slice -- build first.

#![allow(dead_code)]

use crate::config::Config;
use anyhow::Result;

/// The agent's persisted mTLS identity: its private key + CA-signed client cert.
pub struct Identity {
    pub client_cert_pem: String,
    pub client_key_pem: String,
    pub ca_cert_pem: String,
    pub agent_id: String,
}

/// Load an existing on-disk identity, or run the enrollment handshake to obtain
/// one: generate a keypair + CSR locally (the private key never leaves the guest),
/// call `Enroll` over server-authenticated TLS, and persist the returned cert.
pub async fn ensure_enrolled(_cfg: &Config) -> Result<Identity> {
    todo!("agent enrollment -- build slice #1, PRD §5.2")
}
