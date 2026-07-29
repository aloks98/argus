//! Enrollment handshake (PRD §5.2).

use crate::config::Config;
use crate::identity;
use anyhow::{Context, Result};
use argus_proto::v1::agent_service_client::AgentServiceClient;
use argus_proto::v1::EnrollRequest;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint};

/// The agent's persisted mTLS identity: its private key + CA-signed client cert.
pub struct Identity {
    pub client_cert_pem: String,
    pub client_key_pem: String,
    pub ca_cert_pem: String,
    pub agent_id: String,
}

/// Loads an existing on-disk identity, or runs the enrollment handshake:
/// generate a keypair + CSR locally (the private key never leaves the
/// guest), call `Enroll` over server-authenticated TLS, persist the cert.
pub async fn ensure_enrolled(cfg: &Config) -> Result<Identity> {
    if let Some(existing) = identity::load(&cfg.data_dir).context("loading local identity")? {
        return Ok(existing);
    }

    let pending =
        identity::load_or_generate_csr(&cfg.data_dir).context("generating local keypair + CSR")?;
    let info = crate::info::gather(env!("CARGO_PKG_VERSION")).context("gathering host facts")?;

    // Server-authenticated TLS only: no client cert exists yet, so none is
    // presented; the control plane is verified against the baked-in CA cert
    // (PRD §5.2).
    let ca_cert_pem = std::fs::read(&cfg.ca_cert_path)
        .with_context(|| format!("reading CA cert at {}", cfg.ca_cert_path))?;
    let tls = ClientTlsConfig::new().ca_certificate(Certificate::from_pem(ca_cert_pem));
    let channel = Endpoint::from_shared(cfg.endpoint.clone())
        .context("parsing agent endpoint")?
        .tls_config(tls)
        .context("configuring enroll channel TLS")?
        .connect()
        .await
        .context("connecting to control plane for enrollment")?;

    let resp = AgentServiceClient::new(channel)
        .enroll(EnrollRequest {
            join_token: cfg.join_token.clone(),
            csr_pem: pending.csr_pem,
            info: Some(info),
        })
        .await
        .context("Enroll RPC failed")?
        .into_inner();

    identity::persist_cert(&cfg.data_dir, &resp.client_cert_pem, &resp.ca_cert_pem)
        .context("persisting issued identity")?;

    Ok(Identity {
        client_cert_pem: resp.client_cert_pem,
        client_key_pem: pending.key_pem,
        ca_cert_pem: resp.ca_cert_pem,
        agent_id: resp.agent_id,
    })
}
