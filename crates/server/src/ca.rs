//! The CA private key is persisted in Postgres (`ca_material`), AES-256-GCM
//! encrypted with the field key from env, so a stateless pod can reschedule
//! without losing the fleet's identity root (PRD §2.3, §5).

use crate::crypto::FieldCipher;
use anyhow::{Context, Result};
use rand::RngCore;
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName,
    DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose, SerialNumber,
};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;
use x509_parser::prelude::{FromDer, X509Certificate};

/// Validity window for issued leaf certs (agent client certs and the server's
/// own leaf): one year (PRD §5).
const LEAF_VALIDITY_DAYS: i64 = 365;
/// Backdate `not_before` slightly to tolerate clock skew between the control
/// plane and the machine that will present the cert.
const CLOCK_SKEW_BUFFER_HOURS: i64 = 1;

/// A freshly-signed certificate plus the metadata the caller persists into
/// `agent_certs` (PRD §5.3, §6.1).
pub struct SignedCert {
    pub cert_pem: String,
    /// Decimal serial number (matches the `numeric` column in `agent_certs`).
    pub serial: String,
    /// hex(sha256(DER)) -- matches the `fingerprint` column in `agent_certs`.
    pub fingerprint_hex: String,
    pub not_before: OffsetDateTime,
    pub not_after: OffsetDateTime,
}

pub struct CertAuthority {
    cert_pem: String,
    key_pem: String,
}

impl CertAuthority {
    /// Loads the singleton CA (`ca_material.id = 1`), or generates + persists
    /// one on first boot.
    pub async fn load_or_init(pool: &PgPool, cipher: &FieldCipher) -> Result<Self> {
        let existing = sqlx::query!(
            "SELECT cert_pem, key_ciphertext, key_nonce FROM ca_material WHERE id = 1"
        )
        .fetch_optional(pool)
        .await
        .context("querying ca_material")?;

        if let Some(row) = existing {
            let key_pem_bytes = cipher
                .decrypt(&row.key_ciphertext, &row.key_nonce)
                .context("decrypting CA private key")?;
            let key_pem =
                String::from_utf8(key_pem_bytes).context("decrypted CA key is not valid UTF-8")?;
            tracing::info!("loaded existing CA from ca_material");
            return Ok(Self {
                cert_pem: row.cert_pem,
                key_pem,
            });
        }

        let (cert_pem, key_pem) = Self::generate()?;
        let (key_ciphertext, key_nonce) = cipher
            .encrypt(key_pem.as_bytes())
            .context("encrypting CA private key")?;
        sqlx::query!(
            "INSERT INTO ca_material (id, cert_pem, key_ciphertext, key_nonce) VALUES (1, $1, $2, $3)",
            cert_pem,
            key_ciphertext,
            key_nonce,
        )
        .execute(pool)
        .await
        .context("persisting new CA to ca_material")?;
        tracing::info!("generated and persisted new CA");
        Ok(Self { cert_pem, key_pem })
    }

    #[cfg(test)]
    fn self_signed_for_test() -> Self {
        let (cert_pem, key_pem) = Self::generate().expect("generate CA for test");
        Self { cert_pem, key_pem }
    }

    /// Served to agents so they can verify the server's leaf and pin the
    /// identity root.
    pub fn ca_cert_pem(&self) -> &str {
        &self.cert_pem
    }

    fn generate() -> Result<(String, String)> {
        let key_pair = KeyPair::generate().context("generating CA key pair")?;
        let mut params = CertificateParams::new(Vec::<String>::new())
            .context("building CA certificate params")?;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "Argus Internal CA");

        let cert = params
            .self_signed(&key_pair)
            .context("self-signing CA root")?;
        Ok((cert.pem(), key_pair.serialize_pem()))
    }

    /// `KeyPair` isn't `Clone`, so it's re-derived from the stored PEM on every call.
    fn issuer(&self) -> Result<Issuer<'static, KeyPair>> {
        let key_pair = KeyPair::from_pem(&self.key_pem).context("parsing CA private key")?;
        Issuer::from_ca_cert_pem(&self.cert_pem, key_pair).context("building issuer from CA cert")
    }

    /// Embeds `agent_id` as the cert's CN (PRD §5.3). Caller records the
    /// `agent_certs` row and the audit entry.
    pub fn sign_csr(&self, csr_pem: &str, agent_id: Uuid) -> Result<SignedCert> {
        let mut csr =
            CertificateSigningRequestParams::from_pem(csr_pem).context("parsing/verifying CSR")?;

        csr.params.distinguished_name = DistinguishedName::new();
        csr.params
            .distinguished_name
            .push(DnType::CommonName, agent_id.to_string());
        // A malicious/misconfigured CSR could request a BasicConstraints
        // extension asking for CA rights; an agent client cert must never be
        // treated as a CA regardless of what the CSR asked for.
        csr.params.is_ca = IsCa::ExplicitNoCa;
        // `from_pem` copies the CSR's SANs/keyUsages/EKU verbatim; unchecked, an
        // agent could request `EKU=serverAuth` + a SAN matching the control
        // plane's hostname and mint a server-impersonation cert. Client-only, always.
        csr.params.subject_alt_names = Vec::new();
        csr.params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        csr.params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];

        let now = OffsetDateTime::now_utc();
        csr.params.not_before = now - Duration::hours(CLOCK_SKEW_BUFFER_HOURS);
        csr.params.not_after = now + Duration::days(LEAF_VALIDITY_DAYS);
        csr.params.serial_number = Some(SerialNumber::from_slice(&random_serial_bytes()));

        let issuer = self.issuer()?;
        let cert = csr.signed_by(&issuer).context("signing CSR")?;

        let (cert_pem, serial, fingerprint_hex, not_before, not_after) = describe_cert(&cert)?;
        Ok(SignedCert {
            cert_pem,
            serial,
            fingerprint_hex,
            not_before,
            not_after,
        })
    }

    /// Issue the server's own TLS leaf for the given SANs (PRD §5).
    pub fn issue_server_cert(&self, sans: &[String]) -> Result<(String, String)> {
        let leaf_key = KeyPair::generate().context("generating server key pair")?;
        let mut params =
            CertificateParams::new(sans.to_vec()).context("building server certificate params")?;
        params.is_ca = IsCa::ExplicitNoCa;
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "argus-server");

        let now = OffsetDateTime::now_utc();
        params.not_before = now - Duration::hours(CLOCK_SKEW_BUFFER_HOURS);
        params.not_after = now + Duration::days(LEAF_VALIDITY_DAYS);

        let issuer = self.issuer()?;
        let cert = params
            .signed_by(&leaf_key, &issuer)
            .context("signing server leaf")?;

        Ok((cert.pem(), leaf_key.serialize_pem()))
    }
}

/// rcgen/yasna DER-encode this as a positive bignum regardless of the top
/// bit, so no manual masking is needed.
fn random_serial_bytes() -> [u8; 16] {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes
}

/// Re-parses the signed DER rather than trusting what we asked rcgen for --
/// rcgen may normalize the validity window, and the caller needs the decimal
/// serial (`agent_certs.serial` is `numeric`) plus the DER's SHA-256 fingerprint.
fn describe_cert(
    cert: &rcgen::Certificate,
) -> Result<(String, String, String, OffsetDateTime, OffsetDateTime)> {
    let der = cert.der();
    let fingerprint_hex = hex::encode(Sha256::digest(der.as_ref()));
    let (_, parsed) =
        X509Certificate::from_der(der.as_ref()).context("re-parsing signed certificate")?;
    let serial = parsed.tbs_certificate.serial.to_string();
    let not_before = parsed.validity().not_before.to_datetime();
    let not_after = parsed.validity().not_after.to_datetime();
    Ok((cert.pem(), serial, fingerprint_hex, not_before, not_after))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{CertificateParams, KeyPair};
    use uuid::Uuid;

    fn make_csr() -> String {
        let kp = KeyPair::generate().unwrap();
        let params = CertificateParams::new(vec!["ignored".into()]).unwrap();
        params.serialize_request(&kp).unwrap().pem().unwrap()
    }

    #[test]
    fn signs_csr_with_agent_id_in_cn_and_chains_to_ca() {
        let ca = CertAuthority::self_signed_for_test();
        let agent_id = Uuid::from_u128(0x1234);
        let signed = ca.sign_csr(&make_csr(), agent_id).unwrap();

        // Leading `::pem::` forces the extern `pem` crate: `x509_parser::prelude::*`
        // re-exports its own internal `pem` submodule, which otherwise shadows it.
        let pem = ::pem::parse(signed.cert_pem.as_bytes()).unwrap();
        let (_, cert) = X509Certificate::from_der(pem.contents()).unwrap();
        let cn = cert
            .subject()
            .iter_common_name()
            .next()
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(cn, agent_id.to_string());

        // Issuer must equal the CA's subject. Full crypto chain verification would
        // need x509-parser's `verify` feature (not enabled here); this plus a
        // successful `signed_by` call is the available check without it.
        let ca_pem = ::pem::parse(ca.ca_cert_pem().as_bytes()).unwrap();
        let (_, ca_cert) = X509Certificate::from_der(ca_pem.contents()).unwrap();
        assert_eq!(cert.issuer(), ca_cert.subject());

        let expected_fp = hex::encode(Sha256::digest(pem.contents()));
        assert_eq!(signed.fingerprint_hex, expected_fp);
        assert!(signed.not_after - signed.not_before > Duration::days(360));
    }

    #[test]
    fn issue_server_cert_embeds_sans_and_server_auth_eku() {
        let ca = CertAuthority::self_signed_for_test();
        let (cert_pem, key_pem) = ca
            .issue_server_cert(&["argus.local".to_string(), "127.0.0.1".to_string()])
            .unwrap();

        // The returned key PEM actually parses back as a key pair.
        KeyPair::from_pem(&key_pem).unwrap();

        let pem = ::pem::parse(cert_pem.as_bytes()).unwrap();
        let (_, cert) = X509Certificate::from_der(pem.contents()).unwrap();
        let sans = cert.subject_alternative_name().unwrap().unwrap();
        assert!(sans
            .value
            .general_names
            .iter()
            .any(|n| n.to_string().contains("argus.local")));

        let eku = cert.extended_key_usage().unwrap().unwrap();
        assert!(eku.value.server_auth);
    }

    /// A hostile agent's CSR requests `serverAuth` EKU and a SAN for a server
    /// hostname it doesn't own -- `sign_csr` must strip these so the issued cert
    /// can never be presented as a valid server identity.
    #[test]
    fn sign_csr_neutralizes_hostile_csr_extensions() {
        let ca = CertAuthority::self_signed_for_test();
        let agent_id = Uuid::from_u128(0xdead_beef);

        let kp = KeyPair::generate().unwrap();
        let mut hostile_params = CertificateParams::new(vec!["evil.example".to_string()]).unwrap();
        hostile_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let hostile_csr_pem = hostile_params
            .serialize_request(&kp)
            .unwrap()
            .pem()
            .unwrap();

        let signed = ca.sign_csr(&hostile_csr_pem, agent_id).unwrap();

        let pem = ::pem::parse(signed.cert_pem.as_bytes()).unwrap();
        let (_, cert) = X509Certificate::from_der(pem.contents()).unwrap();

        let eku = cert.extended_key_usage().unwrap().unwrap();
        assert!(!eku.value.server_auth);
        assert!(eku.value.client_auth);

        // subject_alt_names cleared -> rcgen omits the SAN extension entirely
        // (see `write_subject_alt_names`), so it must be absent, not empty.
        assert!(cert.subject_alternative_name().unwrap().is_none());
    }

    /// Proves persistence + decrypt round-trip against a live Postgres:
    /// `load_or_init` twice should yield the same CA cert both times, and the
    /// second call must hit the decrypt path, not `generate()` again.
    #[tokio::test]
    #[ignore = "requires DATABASE_URL / a live Postgres; run with `-- --ignored`"]
    async fn load_or_init_persists_and_reloads_the_same_ca() {
        use base64::{engine::general_purpose::STANDARD, Engine};

        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for this test");
        let pool = sqlx::PgPool::connect(&database_url)
            .await
            .expect("connect to postgres");
        sqlx::query!("DELETE FROM ca_material")
            .execute(&pool)
            .await
            .expect("reset ca_material");

        let cipher = FieldCipher::from_b64_key(&STANDARD.encode([9u8; 32])).unwrap();

        let first = CertAuthority::load_or_init(&pool, &cipher)
            .await
            .expect("first load_or_init generates + persists");
        let second = CertAuthority::load_or_init(&pool, &cipher)
            .await
            .expect("second load_or_init loads the persisted CA");

        assert_eq!(first.ca_cert_pem(), second.ca_cert_pem());

        sqlx::query!("DELETE FROM ca_material")
            .execute(&pool)
            .await
            .expect("cleanup ca_material");
    }
}
