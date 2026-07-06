use anyhow::{Context, Result};

/// Agent configuration, sourced from environment variables (baked into the
/// Ignition/cloud-init template alongside the join token and CA cert -- PRD §5.1).
#[derive(Clone)]
pub struct Config {
    pub endpoint: String,
    pub join_token: String,
    pub ca_cert_path: String,
    pub data_dir: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        use argus_common::env;
        let data_dir = std::env::var(env::DATA_DIR)
            .unwrap_or_else(|_| argus_common::AGENT_DATA_DIR.to_string());
        Ok(Self {
            endpoint: req(env::AGENT_ENDPOINT)?,
            join_token: req(env::JOIN_TOKEN)?,
            ca_cert_path: std::env::var(env::CA_CERT_PATH)
                .unwrap_or_else(|_| format!("{data_dir}/ca.crt")),
            data_dir,
        })
    }
}

fn req(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("missing required env var {key}"))
}
