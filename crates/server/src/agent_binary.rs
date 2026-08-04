// The bundled agent binary the control plane can push to machines
// (ARGUS_AGENT_BINARY). Loaded once at boot; the version is the workspace
// version by construction -- server and agent are released together.
use anyhow::Context;

pub struct AgentBinary {
    pub bytes: Vec<u8>,
    pub sha256_hex: String,
    pub version: &'static str,
    pub total_bytes: u64,
}

impl AgentBinary {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {path}"))?;
        Ok(Self::from_bytes(bytes))
    }

    fn from_bytes(bytes: Vec<u8>) -> Self {
        let digest = ring::digest::digest(&ring::digest::SHA256, &bytes);
        let sha256_hex = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        let total_bytes = bytes.len() as u64;
        AgentBinary {
            bytes,
            sha256_hex,
            version: env!("CARGO_PKG_VERSION"),
            total_bytes,
        }
    }

    #[cfg(test)]
    pub fn for_tests(bytes: Vec<u8>) -> Self {
        Self::from_bytes(bytes)
    }
}
