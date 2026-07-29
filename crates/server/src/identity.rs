//! `normalize_tags`, `normalize_display_name`, and `validate_notes` are the
//! ONE implementation shared by every write path (PATCH handler, token
//! minting, enroll-time), so the rules cannot drift apart. No regex crate:
//! the character class is trivial.
//!
//! `ServerTlsConfig::client_ca_root` + `client_auth_optional` (see
//! `grpc::serve`) make presenting a client cert optional-but-verified.
//! `Request::peer_certs()` returns `None` when none was presented (the
//! `Enroll` caller), `Some` otherwise.

use anyhow::{anyhow, Context, Result};
use rustls::pki_types::CertificateDer;
use uuid::Uuid;
use x509_parser::prelude::{FromDer, X509Certificate};

pub const MAX_TAGS: usize = 16;
const MAX_TAG_LEN: usize = 32;
const MAX_DISPLAY_NAME_LEN: usize = 64;
const MAX_NOTES_LEN: usize = 4000;

/// trim → lowercase → order-preserving dedupe → validate each. Errors carry
/// the offending value so the 400 is actionable; nothing is silently dropped.
pub fn normalize_tags(raw: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for r in raw {
        let t = r.trim().to_lowercase();
        if t.is_empty() {
            return Err("tags must not be empty".into());
        }
        if t.len() > MAX_TAG_LEN {
            return Err(format!("tag too long (max {MAX_TAG_LEN} chars): {t}"));
        }
        let mut chars = t.chars();
        let head_ok = chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
        let tail_ok = chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
        if !head_ok || !tail_ok {
            return Err(format!(
                "invalid tag (lowercase letters, digits, '.', '_', '-'; must start with a letter or digit): {t}"
            ));
        }
        if !out.contains(&t) {
            out.push(t);
        }
    }
    if out.len() > MAX_TAGS {
        return Err(format!("too many tags (max {MAX_TAGS})"));
    }
    Ok(out)
}

/// `Ok(None)` = clear back to "display the hostname".
pub fn normalize_display_name(raw: &str) -> Result<Option<String>, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Ok(None);
    }
    if t.chars().count() > MAX_DISPLAY_NAME_LEN {
        return Err(format!(
            "display name too long (max {MAX_DISPLAY_NAME_LEN} chars)"
        ));
    }
    Ok(Some(t.to_string()))
}

pub fn validate_notes(raw: &str) -> Result<(), String> {
    if raw.chars().count() > MAX_NOTES_LEN {
        return Err(format!("notes too long (max {MAX_NOTES_LEN} chars)"));
    }
    Ok(())
}

/// Extracts the `agent_id` from a peer cert's CN (the internal CA signs agent
/// certs with the `agent_id` UUID as CN, `ca::sign_csr`, PRD §5.3).
///
/// SECURITY PRECONDITION: performs **no** chain validation -- pure ASN.1 CN
/// parsing. Callers MUST pass certs from [`tonic::Request::peer_certs`] on a
/// connection served by `grpc::serve` (chain already validated against the
/// internal CA via `client_ca_root`); an unvalidated source (e.g. a
/// client-populated proto field) would let an attacker choose the `agent_id`.
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
    fn tags_are_trimmed_lowercased_and_deduped_in_order() {
        let raw = vec![" Infra ".into(), "media".into(), "infra".into()];
        assert_eq!(normalize_tags(&raw).unwrap(), vec!["infra", "media"]);
    }

    #[test]
    fn each_tag_rejection_class_names_the_offender() {
        assert!(normalize_tags(&["  ".into()]).is_err());
        let long = "a".repeat(33);
        let long_vec = vec![long.clone()];
        assert!(normalize_tags(&long_vec).unwrap_err().contains(&long));
        assert!(normalize_tags(&["has space".into()])
            .unwrap_err()
            .contains("has space"));
        assert!(normalize_tags(&["-leading".into()]).is_err()); // must start [a-z0-9]
        assert!(normalize_tags(&["ok_tag.v1-x".into()]).is_ok());
    }

    #[test]
    fn more_than_max_tags_is_rejected_after_dedupe() {
        let raw: Vec<String> = (0..MAX_TAGS + 1).map(|i| format!("t{i}")).collect();
        assert!(normalize_tags(&raw).is_err());
        let mut dup = vec!["same".to_string(); 2];
        dup.extend((0..MAX_TAGS - 1).map(|i| format!("t{i}")));
        assert_eq!(dup.len(), MAX_TAGS + 1);
        assert!(normalize_tags(&dup).is_ok());
    }

    #[test]
    fn display_name_trims_clears_and_caps() {
        assert_eq!(
            normalize_display_name("  Media box  ").unwrap(),
            Some("Media box".into())
        );
        assert_eq!(normalize_display_name("   ").unwrap(), None);
        assert!(normalize_display_name(&"x".repeat(65)).is_err());
        assert!(normalize_display_name(&"x".repeat(64)).is_ok());
    }

    #[test]
    fn notes_cap_at_4000() {
        assert!(validate_notes(&"x".repeat(4000)).is_ok());
        assert!(validate_notes(&"x".repeat(4001)).is_err());
    }

    #[test]
    fn display_name_counts_characters_not_bytes() {
        assert!(normalize_display_name(&"ä".repeat(64)).is_ok());
        assert!(normalize_display_name(&"ä".repeat(65)).is_err());
    }

    #[test]
    fn notes_counts_characters_not_bytes() {
        assert!(validate_notes(&"ä".repeat(4000)).is_ok());
        assert!(validate_notes(&"ä".repeat(4001)).is_err());
    }

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
