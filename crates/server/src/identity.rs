//! Extracting `agent_id` from a validated agent client cert (Task 6).
//!
//! ## tonic 0.14 optional-client-auth API spike
//!
//! Confirmed by reading the vendored `tonic-0.14.6` source
//! (`~/.cargo/registry/src/*/tonic-0.14.6/src/transport/server/tls.rs` and
//! `src/request.rs`), since the exact call names are the highest-risk part of
//! this task:
//!
//! - `ServerTlsConfig` already supports OPTIONAL client auth directly -- the
//!   `tokio-rustls`-manual-acceptor fallback described in the task brief is
//!   NOT needed. `ServerTlsConfig::client_ca_root(cert: Certificate) -> Self`
//!   sets the trust anchor used to validate a client cert if one is
//!   presented, and `ServerTlsConfig::client_auth_optional(optional: bool) ->
//!   Self` (default `false`) is documented as "This option has effect only if
//!   CA certificate is set" -- i.e. setting both makes presenting a client
//!   cert optional-but-verified rather than mandatory.
//! - `Request::peer_certs(&self) -> Option<Arc<Vec<CertificateDer<'static>>>>`
//!   (`tonic-0.14.6/src/request.rs`) returns `Some` only on the server side of
//!   a TLS-enabled `transport::Server` connection when the peer presented a
//!   cert; `None` when no client cert was presented (the `Enroll` caller,
//!   once client auth is optional) or the connection isn't TLS.
//! - `CertificateDer` is `tokio_rustls::rustls::pki_types::CertificateDer`,
//!   which tonic re-exports as `tonic::transport::CertificateDer`
//!   (`tonic-0.14.6/src/transport/mod.rs`: `pub use
//!   tokio_rustls::rustls::pki_types::CertificateDer;`). This crate already
//!   depends on `rustls` 0.23 directly, and `cargo tree -p argus-server -i
//!   rustls` / `-i tokio-rustls` confirm the whole workspace unifies on a
//!   single `rustls v0.23.41` / `tokio-rustls v0.26.4` -- so
//!   `rustls::pki_types::CertificateDer` used below is the exact same type
//!   `Request::peer_certs()` yields; no re-export juggling required.
//!
//! **Path taken:** the simple `ServerTlsConfig` path (see `grpc::serve`). The
//! manual `tokio_rustls::TlsAcceptor` + `WebPkiClientVerifier` fallback was
//! not needed.
//!
//! `agent_id_from_peer` is called from `grpc::session` (Task 7), which requires
//! a client cert and cross-checks its CN-derived `agent_id` against
//! `repo::cert_is_active`'s fingerprint lookup before starting the bidi loop.

use anyhow::{anyhow, Context, Result};
use rustls::pki_types::CertificateDer;
use uuid::Uuid;
use x509_parser::prelude::{FromDer, X509Certificate};

/// Extract the `agent_id` from the leaf of a peer certificate chain. The
/// internal CA signs agent client certs with the `agent_id` UUID as the CN
/// (`ca::sign_csr`, PRD §5.3); this mirrors the CN-parsing idiom already used
/// in `ca.rs` and the `grpc` enroll tests.
///
/// SECURITY PRECONDITION: this function performs **no** certificate-chain
/// validation -- it is a pure ASN.1 CN parser. Callers MUST pass certs
/// obtained from [`tonic::Request::peer_certs`] on a connection served by
/// `grpc::serve` (which sets `client_ca_root`, so rustls has already validated
/// the chain against the internal CA). Feeding it cert bytes from any
/// unvalidated source (e.g. a client-populated proto field) would let an
/// attacker choose the returned `agent_id`.
pub fn agent_id_from_peer(certs: &[CertificateDer]) -> Result<Uuid> {
    let leaf = certs
        .first()
        .ok_or_else(|| anyhow!("no client certificate presented"))?;

    let (_, cert) =
        X509Certificate::from_der(leaf.as_ref()).context("parsing peer certificate DER")?;

    let cn = cert
        .subject()
        .iter_common_name()
        .next()
        .ok_or_else(|| anyhow!("peer certificate has no CN"))?
        .as_str()
        .context("peer certificate CN is not valid UTF-8")?;

    Uuid::parse_str(cn).context("peer certificate CN is not a valid uuid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};

    #[test]
    fn agent_id_from_peer_reads_the_uuid_from_the_leaf_cns_cn() {
        let agent_id = Uuid::new_v4();

        let kp = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, agent_id.to_string());
        let cert = params.self_signed(&kp).unwrap();

        let der = cert.der();
        let extracted = agent_id_from_peer(std::slice::from_ref(der)).unwrap();
        assert_eq!(extracted, agent_id);
    }

    #[test]
    fn agent_id_from_peer_errors_on_empty_slice() {
        let certs: Vec<CertificateDer<'static>> = Vec::new();
        assert!(agent_id_from_peer(&certs).is_err());
    }
}
