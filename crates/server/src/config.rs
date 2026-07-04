use anyhow::{Context, Result};

/// Control-plane configuration, sourced from environment variables (PRD §13).
#[derive(Clone)]
#[allow(dead_code)] // fields are consumed as slices are built (e.g. field_key_b64 by the CA)
pub struct Config {
    pub database_url: String,
    /// Base64 AES-256-GCM key that field-encrypts the CA private key at rest.
    pub field_key_b64: String,
    pub http_addr: String,
    pub agent_addr: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        use argus_common::env;
        Ok(Self {
            database_url: req(env::DATABASE_URL)?,
            field_key_b64: req(env::FIELD_KEY)?,
            http_addr: std::env::var(env::HTTP_ADDR).unwrap_or_else(|_| "0.0.0.0:8080".into()),
            agent_addr: std::env::var(env::AGENT_ADDR).unwrap_or_else(|_| "0.0.0.0:9443".into()),
        })
    }
}

fn req(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("missing required env var {key}"))
}
