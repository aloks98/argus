//! Passwords are always generated, never chosen (design §7): 24 random
//! characters is unguessable online, demoting rate limiting to a backstop.
//!
//! `verify_password`'s dummy-hash path (wired via `auth::local::login`) pays
//! the argon2id cost even with "no admin configured", so response timing
//! never reveals whether a local admin exists.
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

/// A real argon2id hash of a value nobody can present -- paid on the
/// no-admin path so it costs the same ~100ms as a real verification;
/// without it, "no local admin configured" returns in microseconds, telling
/// an attacker the credential doesn't exist.
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
        // Catches a bug that silently narrowed the alphabet.
        assert!(a.chars().all(|c| PASSWORD_ALPHABET.contains(c)));
    }

    #[test]
    fn hash_verifies_the_right_password_and_rejects_others() {
        let phc = hash_password("correct horse battery staple").expect("hash");
        // Must be a PHC string, not the plaintext password.
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
        // A malformed DUMMY_PHC would return early and reopen the timing signal.
        assert!(DUMMY_PHC.starts_with("$argon2id$"));
        assert!(!verify_password("", DUMMY_PHC));
        assert!(!verify_password("admin", DUMMY_PHC));
    }

    /// Pins `DUMMY_PHC`'s argon2 PARAMETERS (`m`/`t`/`p`) to a freshly
    /// generated hash's -- `verify_password` would happily return `false`
    /// against a weaker, cheaper dummy with every other test still green,
    /// silently reopening the timing side-channel.
    #[test]
    fn the_dummy_hash_shares_argon2_parameters_with_a_freshly_generated_hash() {
        // PHC layout: "$argon2id$v=19$m=..,t=..,p=..$salt$hash" -- parameters are field index 3.
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
