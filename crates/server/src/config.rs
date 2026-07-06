use anyhow::{Context, Result};

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
        })
    }
}

fn req(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("missing required env var {key}"))
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
}
