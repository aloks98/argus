//! Identity-field validation (fleet-identity slice, design "Validation rules")
//! and agent-cert parsing (Task 6).
//!
//! The validation module (`normalize_tags`, `normalize_display_name`,
//! `validate_notes`) provides ONE implementation shared by every write path — the
//! PATCH handler, token minting, and enroll-time application all funnel through
//! here so the rules cannot drift apart. No regex crate: the character class is
//! trivial and the server has no regex dependency to justify.
//!
//! ## tonic 0.14 optional-client-auth API spike (Task 6)
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

// === Validation constants and functions (Task 2) ===

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

// === Agent certificate parsing (Task 6) ===

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

    // === Validation tests (Task 2) ===

    #[test]
    fn tags_are_trimmed_lowercased_and_deduped_in_order() {
        let raw = vec![" Infra ".into(), "media".into(), "infra".into()];
        assert_eq!(normalize_tags(&raw).unwrap(), vec!["infra", "media"]);
    }

    #[test]
    fn each_tag_rejection_class_names_the_offender() {
        // Whitespace-only normalizes to empty and is rejected (not dropped).
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
        // ...but 17 raw entries that dedupe to <= 16 are fine.
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
        // Multi-byte character "ä" (U+00E4) is 2 bytes but 1 character.
        // 64 characters of "ä" should be accepted (even though it's 128 bytes).
        assert!(normalize_display_name(&"ä".repeat(64)).is_ok());
        // 65 characters of "ä" should be rejected.
        assert!(normalize_display_name(&"ä".repeat(65)).is_err());
    }

    #[test]
    fn notes_counts_characters_not_bytes() {
        // Multi-byte character "ä" (U+00E4) is 2 bytes but 1 character.
        // 4000 characters of "ä" should be accepted (even though it's 8000 bytes).
        assert!(validate_notes(&"ä".repeat(4000)).is_ok());
        // 4001 characters of "ä" should be rejected.
        assert!(validate_notes(&"ä".repeat(4001)).is_err());
    }

    // === Agent certificate parsing tests (Task 6) ===

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
