//! The local admin break-glass credential: provisioning/rotation (design §6/§7).
//!
//! `reset_local_admin` is the ONE implementation of "rotate" -- both the
//! `argus local-admin reset` CLI (`main.rs`) and the in-app rotation endpoint
//! a later task adds call this same function, so there is never a second
//! generate-hash-store sequence to drift out of sync with the first.

use anyhow::Result;
use sqlx::PgPool;

/// Generate a new password, store only its hash, and return the password for
/// one-time display. Shared by the CLI and the in-app rotation endpoint so
/// there is exactly one implementation of "rotate".
pub async fn reset_local_admin(pool: &PgPool, username: &str) -> Result<String> {
    let password = crate::auth::password::generate_password();
    let hash = crate::auth::password::hash_password(&password)?;
    crate::repo::upsert_local_admin(pool, username, &hash).await?;
    Ok(password)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::PgPool;

    #[sqlx::test]
    async fn reset_generates_a_working_password_and_stores_only_its_hash(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let password = reset_local_admin(&pool, "admin").await?;
        assert_eq!(
            password.chars().count(),
            crate::auth::password::PASSWORD_LEN
        );

        let row = crate::repo::get_local_admin(&pool).await?.expect("row");
        assert_eq!(row.username, "admin");
        // The stored value must be a hash, and must not contain the password.
        assert!(row.password_hash.starts_with("$argon2id$"));
        assert!(!row.password_hash.contains(&password));
        assert!(crate::auth::password::verify_password(
            &password,
            &row.password_hash
        ));

        // Rotation issues a different password and invalidates the old one.
        let second = reset_local_admin(&pool, "admin").await?;
        assert_ne!(password, second);
        let row2 = crate::repo::get_local_admin(&pool).await?.expect("row");
        assert!(crate::auth::password::verify_password(
            &second,
            &row2.password_hash
        ));
        assert!(
            !crate::auth::password::verify_password(&password, &row2.password_hash),
            "the previous password must stop working after rotation"
        );
        Ok(())
    }
}
