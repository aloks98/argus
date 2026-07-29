//! Local mTLS identity: keypair + CSR generation, and on-disk persistence of
//! the CA-signed client cert returned by `Enroll` (PRD §5.2, §5.3).
//!
//! The private key never leaves this file's I/O boundary: `ensure_enrolled`
//! only ever sends the CSR PEM (public) over the wire, never `agent.key`.

use crate::enroll::Identity;
use anyhow::{Context, Result};
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// A freshly generated (or reused) local keypair plus the CSR built from it,
/// ready to send to `Enroll`. Only `csr_pem` crosses the network.
pub struct PendingIdentity {
    pub key_pem: String,
    pub csr_pem: String,
}

/// Loads the private key from `<data_dir>/agent.key` if present (reused
/// across enroll attempts), else generates one and persists it `0600`.
/// Either way, the CSR's CommonName is this host's `/etc/machine-id` -- just
/// a human-readable hint, since the control plane overwrites it with the
/// assigned agent UUID when signing (`ca.rs::sign_csr`).
pub fn load_or_generate_csr(data_dir: &str) -> Result<PendingIdentity> {
    let key_path = Path::new(data_dir).join("agent.key");

    let key_pair = if key_path.exists() {
        let pem = fs::read_to_string(&key_path)
            .with_context(|| format!("reading {}", key_path.display()))?;
        KeyPair::from_pem(&pem).context("parsing existing agent key")?
    } else {
        fs::create_dir_all(data_dir).with_context(|| format!("creating data dir {data_dir}"))?;
        let key_pair = KeyPair::generate().context("generating agent key pair")?;

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&key_path)
            .with_context(|| format!("creating {}", key_path.display()))?;
        file.write_all(key_pair.serialize_pem().as_bytes())
            .with_context(|| format!("writing {}", key_path.display()))?;

        key_pair
    };

    let machine_id = crate::info::read_machine_id().context("reading machine id for CSR CN")?;
    let mut params = CertificateParams::new(Vec::<String>::new()).context("building CSR params")?;
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, machine_id);

    let csr_pem = params
        .serialize_request(&key_pair)
        .context("building CSR")?
        .pem()
        .context("PEM-encoding CSR")?;

    Ok(PendingIdentity {
        key_pem: key_pair.serialize_pem(),
        csr_pem,
    })
}

/// Persist the CA-signed client cert and the CA's own cert (so the agent can
/// verify the server going forward), alongside the private key written by
/// `load_or_generate_csr`.
pub fn persist_cert(data_dir: &str, cert_pem: &str, ca_pem: &str) -> Result<()> {
    fs::create_dir_all(data_dir).with_context(|| format!("creating data dir {data_dir}"))?;
    fs::write(Path::new(data_dir).join("agent.crt"), cert_pem).context("writing agent.crt")?;
    fs::write(Path::new(data_dir).join("ca.crt"), ca_pem).context("writing ca.crt")?;
    Ok(())
}

/// Loads a previously-persisted identity if all three files (`agent.key`,
/// `agent.crt`, `ca.crt`) are present. `agent_id` is parsed from the client
/// cert's CommonName, which the control plane sets to the agent UUID.
pub fn load(data_dir: &str) -> Result<Option<Identity>> {
    let dir = Path::new(data_dir);
    let key_path = dir.join("agent.key");
    let cert_path = dir.join("agent.crt");
    let ca_path = dir.join("ca.crt");

    if !(key_path.exists() && cert_path.exists() && ca_path.exists()) {
        return Ok(None);
    }

    let client_key_pem =
        fs::read_to_string(&key_path).with_context(|| format!("reading {}", key_path.display()))?;
    let client_cert_pem = fs::read_to_string(&cert_path)
        .with_context(|| format!("reading {}", cert_path.display()))?;
    let ca_cert_pem =
        fs::read_to_string(&ca_path).with_context(|| format!("reading {}", ca_path.display()))?;

    let agent_id = common_name_from_cert_pem(&client_cert_pem)?;

    Ok(Some(Identity {
        client_cert_pem,
        client_key_pem,
        ca_cert_pem,
        agent_id,
    }))
}

/// Parse the CommonName out of a PEM-encoded X.509 certificate. Uses
/// `rustls-pemfile` (already a dependency for the session TLS stack) to strip
/// the PEM armor down to DER, then `x509-parser` to read the subject.
fn common_name_from_cert_pem(cert_pem: &str) -> Result<String> {
    let der = rustls_pemfile::certs(&mut cert_pem.as_bytes())
        .next()
        .context("agent.crt contains no certificate")?
        .context("parsing agent.crt PEM")?;

    use x509_parser::prelude::{FromDer, X509Certificate};
    let (_, cert) = X509Certificate::from_der(der.as_ref()).context("parsing agent.crt DER")?;
    let cn = cert
        .subject()
        .iter_common_name()
        .next()
        .context("agent.crt has no CommonName")?
        .as_str()
        .context("agent.crt CommonName is not valid UTF-8")?;
    Ok(cn.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A fresh, unique scratch directory under the OS temp dir -- no
    /// `tempfile` dependency, so cleanup is the test's own responsibility.
    fn unique_temp_dir(tag: &str) -> String {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("argus-agent-identity-{tag}-{nanos}-{n}"));
        fs::create_dir_all(&dir).expect("create unique temp dir");
        dir.to_string_lossy().into_owned()
    }

    #[test]
    fn load_or_generate_csr_writes_key_and_reuses_it_on_second_call() {
        let dir = unique_temp_dir("csr");

        let first = load_or_generate_csr(&dir).expect("first call generates a key + csr");

        let key_path = Path::new(&dir).join("agent.key");
        assert!(key_path.exists(), "agent.key should be written");

        // The private key must never be group/world-readable: a future
        // refactor (e.g. switching to `fs::write`) could silently drop the
        // `0600` mode and this guards against that regression.
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&key_path)
            .expect("stat agent.key")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "agent.key must be private (0600)");

        rcgen::CertificateSigningRequestParams::from_pem(&first.csr_pem)
            .expect("csr_pem should be a parseable, self-consistent CSR");

        let first_key_bytes = fs::read(&key_path).expect("read agent.key");

        let second = load_or_generate_csr(&dir).expect("second call reuses the existing key");
        let second_key_bytes = fs::read(&key_path).expect("read agent.key again");

        assert_eq!(
            first_key_bytes, second_key_bytes,
            "agent.key bytes must be unchanged -- the key must be reused, not regenerated"
        );
        assert_eq!(first.key_pem, second.key_pem);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_returns_none_when_no_identity_is_persisted() {
        let dir = unique_temp_dir("load-none");

        let identity = load(&dir).expect("load on an empty dir should not error");
        assert!(identity.is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_parses_agent_id_from_persisted_cert_common_name() {
        let dir = unique_temp_dir("load-some");

        load_or_generate_csr(&dir).expect("generate key + csr");

        // A self-signed cert standing in for the CA-signed client cert the
        // control plane would normally return from `Enroll`; only its CN is
        // relevant to `load`, so the same PEM can double as the "CA" cert.
        let known_agent_id = "5b1f6c2e-8f2a-4c1a-9e3b-2a7d6c9e4f10";
        let kp = KeyPair::generate().expect("generate cert key pair");
        let mut params =
            CertificateParams::new(Vec::<String>::new()).expect("building cert params");
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, known_agent_id);
        let cert = params.self_signed(&kp).expect("self-sign cert");
        let cert_pem = cert.pem();

        persist_cert(&dir, &cert_pem, &cert_pem).expect("persist cert + ca pem");

        let identity = load(&dir)
            .expect("load should not error")
            .expect("load should return Some once all three files are present");
        assert_eq!(identity.agent_id, known_agent_id);

        let _ = fs::remove_dir_all(&dir);
    }
}
