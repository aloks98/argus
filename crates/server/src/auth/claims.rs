//! Pure claim handling. No I/O and no provider names: OIDC standardises
//! identity but not authorisation, so the variance between providers is
//! absorbed here by configuration plus tolerance, and is covered by a
//! table-driven test over real claim shapes from four providers.

use crate::config::RequiredRole;
use serde_json::{Map, Value};

/// Userinfo claims override ID-token claims of the same name. Providers
/// disagree about which of the two carries roles; merging removes a class of
/// "works on Keycloak, silently denies on Zitadel" failures.
pub fn merge_claims(
    id_token: Map<String, Value>,
    userinfo: Map<String, Value>,
) -> Map<String, Value> {
    let mut merged = id_token;
    for (k, v) in userinfo {
        merged.insert(k, v);
    }
    merged
}

/// Resolves a dot-separated path (Keycloak nests roles under
/// `realm_access.roles`). Deliberate consequence: a claim NAME containing
/// dots can't be addressed -- accepted since nesting is the common case.
pub fn claim_at_path<'a>(claims: &'a Map<String, Value>, path: &str) -> Option<&'a Value> {
    let mut parts = path.split('.');
    let mut current = claims.get(parts.next()?)?;
    for part in parts {
        current = current.as_object()?.get(part)?;
    }
    Some(current)
}

/// Extract role names from whichever of the three real-world shapes was sent.
/// Anything unrecognised yields no roles, which denies admission -- failing
/// closed is the only safe default for an authorisation input.
pub fn roles_from_claim(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        // Zitadel: the KEYS are the role names; the values are org metadata.
        Value::Object(map) => map.keys().cloned().collect(),
        Value::String(s) => s.split_whitespace().map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

pub fn is_admitted(roles: &[String], required: &RequiredRole) -> bool {
    match required {
        RequiredRole::Any => true,
        RequiredRole::Named(name) => roles.iter().any(|r| r == name),
    }
}

/// Claim NAMES only -- never values. Used to make a denial diagnosable in the
/// log without dumping email addresses and other claim contents.
pub fn claim_keys(claims: &Map<String, Value>) -> Vec<String> {
    claims.keys().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn roles_extracted_from_every_real_provider_shape() {
        let okta = json!({"groups": ["argus-admin", "everyone"]});
        assert_eq!(
            roles_from_claim(claim_at_path(okta.as_object().unwrap(), "groups").unwrap()),
            vec!["argus-admin", "everyone"]
        );

        let keycloak = json!({"realm_access": {"roles": ["argus-admin", "offline_access"]}});
        assert_eq!(
            roles_from_claim(
                claim_at_path(keycloak.as_object().unwrap(), "realm_access.roles").unwrap()
            ),
            vec!["argus-admin", "offline_access"]
        );

        let zitadel = json!({
            "urn:zitadel:iam:org:project:roles": {
                "argus-admin": {"orgid": "example.zitadel.cloud"}
            }
        });
        assert_eq!(
            roles_from_claim(
                claim_at_path(
                    zitadel.as_object().unwrap(),
                    "urn:zitadel:iam:org:project:roles"
                )
                .unwrap()
            ),
            vec!["argus-admin"]
        );

        let spaced = json!({"roles": "argus-admin operator"});
        assert_eq!(
            roles_from_claim(claim_at_path(spaced.as_object().unwrap(), "roles").unwrap()),
            vec!["argus-admin", "operator"]
        );
    }

    #[test]
    fn unusable_claim_shapes_yield_no_roles_and_therefore_deny() {
        assert!(roles_from_claim(&json!(42)).is_empty());
        assert!(roles_from_claim(&json!(null)).is_empty());
        assert!(roles_from_claim(&json!([])).is_empty());
        // Mixed arrays keep only the strings rather than failing outright.
        assert_eq!(roles_from_claim(&json!(["a", 7, "b"])), vec!["a", "b"]);
    }

    #[test]
    fn missing_path_returns_none_not_a_panic() {
        let claims = json!({"realm_access": {"roles": ["x"]}});
        let m = claims.as_object().unwrap();
        assert!(claim_at_path(m, "groups").is_none());
        assert!(claim_at_path(m, "realm_access.missing").is_none());
        // Intermediate node is a scalar, so the path cannot continue.
        let flat = json!({"realm_access": "nope"});
        assert!(claim_at_path(flat.as_object().unwrap(), "realm_access.roles").is_none());
    }

    #[test]
    fn userinfo_claims_override_the_id_token() {
        let id = json!({"sub": "s1", "email": "old@example.com"});
        let ui = json!({"email": "new@example.com", "groups": ["argus-admin"]});
        let merged = merge_claims(
            id.as_object().unwrap().clone(),
            ui.as_object().unwrap().clone(),
        );
        assert_eq!(merged.get("email").unwrap(), "new@example.com");
        assert_eq!(merged.get("sub").unwrap(), "s1");
        assert!(merged.contains_key("groups"));
    }

    #[test]
    fn admission_requires_the_named_role_but_any_admits_everyone() {
        let roles = vec!["operator".to_string()];
        assert!(!is_admitted(
            &roles,
            &RequiredRole::Named("argus-admin".into())
        ));
        assert!(is_admitted(&roles, &RequiredRole::Named("operator".into())));
        assert!(is_admitted(&roles, &RequiredRole::Any));
        // `Any` admits even a roleless caller -- providers with no roles concept must still work.
        assert!(is_admitted(&[], &RequiredRole::Any));
        assert!(!is_admitted(
            &[],
            &RequiredRole::Named("argus-admin".into())
        ));
    }

    #[test]
    fn claim_keys_lists_what_the_provider_actually_sent() {
        let claims = json!({"sub": "s", "groups": ["a"], "realm_access": {"roles": []}});
        let mut keys = claim_keys(claims.as_object().unwrap());
        keys.sort();
        assert_eq!(keys, vec!["groups", "realm_access", "sub"]);
    }
}
