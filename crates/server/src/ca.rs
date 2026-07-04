//! Internal certificate authority.
//!
//! The CA private key is persisted in Postgres (`ca_material`), AES-256-GCM
//! encrypted with the field key from env, so a stateless pod can reschedule
//! without losing the fleet's identity root (PRD §2.3, §5). This is build slice #1
//! and the highest-risk component -- build and manually verify it first.

#![allow(dead_code)]

use crate::config::Config;
use anyhow::Result;
use sqlx::PgPool;

pub struct CertAuthority {
    pub ca_cert_pem: String,
}

impl CertAuthority {
    /// Load the singleton CA from `ca_material`, or generate + persist one on
    /// first boot (`rcgen`, encrypting the key with the field key).
    pub async fn load_or_init(_pool: &PgPool, _cfg: &Config) -> Result<Self> {
        todo!("CA load/init -- build slice #1, PRD §5")
    }

    /// Sign an agent CSR, returning the client-cert PEM. The caller records the
    /// `agent_certs` row and the audit entry (PRD §5.3).
    pub fn sign_csr(&self, _csr_pem: &str, _agent_id: &str) -> Result<String> {
        todo!("CSR signing -- build slice #1, PRD §5.3")
    }
}
