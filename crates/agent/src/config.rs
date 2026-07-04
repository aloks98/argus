use anyhow::{Context, Result};

/// Agent configuration, sourced from environment variables (baked into the
/// Ignition/cloud-init template alongside the join token and CA cert -- PRD §5.1).
#[derive(Clone)]
#[allow(dead_code)] // fields are consumed by the enrollment + session slices
pub struct Config {
    pub endpoint: String,
    pub join_token: String,
    pub ca_cert_path: String,
    pub data_dir: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        use argus_common::env;
        Ok(Self {
            endpoint: req(env::AGENT_ENDPOINT)?,
            join_token: req(env::JOIN_TOKEN)?,
            ca_cert_path: std::env::var(env::CA_CERT_PATH)
                .unwrap_or_else(|_| format!("{}/ca.crt", argus_common::AGENT_DATA_DIR)),
            data_dir: argus_common::AGENT_DATA_DIR.to_string(),
        })
    }
}

fn req(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("missing required env var {key}"))
}
