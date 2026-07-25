//! Local admin password generation and argon2id hashing.
//!
//! Passwords are always generated, never chosen (design §7): 24 random
//! characters is arithmetically unguessable online, which is what demotes rate
//! limiting to a backstop rather than the primary control.
//!
//! `generate_password`/`hash_password` are wired in via
//! `auth::local::reset_local_admin` (the CLI). `verify_password` and the
//! dummy-hash helpers below are wired in via `auth::local::login` (`POST
//! /auth/local`), which is what pays the argon2id cost on every path --
//! including "no admin configured" -- so response timing never reveals
//! whether a local admin exists.
use anyhow::{anyhow, Result};
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use rand::Rng;

pub const PASSWORD_LEN: usize = 24;

/// Deliberately excludes visually ambiguous characters (0/O, 1/l/I). This is a
/// credential a human transcribes from a terminal under pressure during an
/// outage, and a misread character is indistinguishable from a wrong password.
pub const PASSWORD_ALPHABET: &str = "ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";

pub fn generate_password() -> String {
    let alphabet: Vec<char> = PASSWORD_ALPHABET.chars().collect();
    let mut rng = rand::rng();
    (0..PASSWORD_LEN)
        .map(|_| alphabet[rng.random_range(0..alphabet.len())])
        .collect()
}

pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow!("argon2 hash failed: {e}"))
}

/// Returns false for a malformed stored hash rather than propagating: a corrupt
/// row must deny the login, never crash the handler or admit the caller.
pub fn verify_password(password: &str, phc: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// A real argon2id hash of a value nobody can present, used when no local admin
/// row exists so the handler still pays the verification cost. Without this,
/// "no local admin configured" returns in microseconds while a wrong password
/// takes ~100ms, and that difference tells an attacker whether the credential
/// exists at all.
pub const DUMMY_PHC: &str = "$argon2id$v=19$m=19456,t=2,p=1$9tkWNBxDNecw0mDcgXHBVA$9+5x76AgJroS3Mf4jCcOxF3tUuUhPjpIpon0meok1b4";

pub fn verify_against_dummy(password: &str) {
    let _ = verify_password(password, DUMMY_PHC);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_passwords_are_long_and_unique() {
        let a = generate_password();
        let b = generate_password();
        assert_ne!(a, b, "every generated password must be fresh");
        assert_eq!(a.chars().count(), PASSWORD_LEN);
        // Every character must come from the declared alphabet -- a bug that
        // silently narrowed it would otherwise pass unnoticed.
        assert!(a.chars().all(|c| PASSWORD_ALPHABET.contains(c)));
    }

    #[test]
    fn hash_verifies_the_right_password_and_rejects_others() {
        let phc = hash_password("correct horse battery staple").expect("hash");
        // A PHC string, not the password: a bug that stored the plaintext must fail here.
        assert!(
            phc.starts_with("$argon2id$"),
            "expected argon2id PHC, got {phc}"
        );
        assert!(!phc.contains("correct horse"));

        assert!(verify_password("correct horse battery staple", &phc));
        assert!(!verify_password("wrong", &phc));
        assert!(!verify_password("", &phc));
    }

    #[test]
    fn each_hash_uses_a_fresh_salt() {
        let a = hash_password("same").expect("hash");
        let b = hash_password("same").expect("hash");
        assert_ne!(
            a, b,
            "identical passwords must not produce identical hashes"
        );
        assert!(verify_password("same", &a) && verify_password("same", &b));
    }

    #[test]
    fn verify_rejects_a_malformed_phc_instead_of_panicking() {
        assert!(!verify_password("anything", "not-a-phc-string"));
        assert!(!verify_password("anything", ""));
    }

    #[test]
    fn the_dummy_hash_is_valid_and_matches_nothing_usable() {
        // The no-admin-configured path verifies against this so that response
        // timing cannot distinguish "no local admin exists" from "wrong
        // password". If it were malformed, verification would return early and
        // the timing signal would reappear.
        assert!(DUMMY_PHC.starts_with("$argon2id$"));
        assert!(!verify_password("", DUMMY_PHC));
        assert!(!verify_password("admin", DUMMY_PHC));
    }

    /// What actually makes the no-admin path cost the same as a real
    /// verification: the argon2 PARAMETERS (`m`/`t`/`p`) must match, not
    /// merely "some argon2id string". `verify_password` only needs a
    /// well-formed PHC to run -- it would happily "succeed" (return `false`,
    /// same as today) against a hash with WEAKER parameters, at a fraction of
    /// the cost, and every existing status/body/cookie assertion in
    /// `http.rs` would stay green while the timing gap design §11 exists to
    /// close quietly reopened. This pins the parameters segment of
    /// `DUMMY_PHC` to always match a freshly generated hash's, so a future
    /// argon2 version/parameter bump that updates one and not the other
    /// fails loudly here instead of only in production timing.
    #[test]
    fn the_dummy_hash_shares_argon2_parameters_with_a_freshly_generated_hash() {
        // PHC layout: "$argon2id$v=19$m=..,t=..,p=..$salt$hash" -- the
        // parameters are the 4th '$'-delimited field (index 3).
        fn params(phc: &str) -> Option<&str> {
            phc.split('$').nth(3)
        }

        let fresh = hash_password("whatever").expect("hash");
        let dummy_params = params(DUMMY_PHC);
        let fresh_params = params(&fresh);
        assert!(
            dummy_params.is_some(),
            "DUMMY_PHC must parse as a PHC string"
        );
        assert_eq!(
            dummy_params, fresh_params,
            "DUMMY_PHC's argon2 parameters must match a freshly generated hash's, \
             or the no-admin path stops costing what a real verification costs"
        );
    }
}
