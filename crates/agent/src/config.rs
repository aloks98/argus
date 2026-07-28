use anyhow::{Context, Result};
use std::collections::HashMap;

/// Agent configuration, sourced from environment variables (baked into the
/// Ignition/cloud-init template alongside the join token and CA cert -- PRD
/// §5.1) or, via `--config <path>`, from an env-file -- the one-shot
/// `sudo -n env VAR=... ./argus-agent` recipe the enroll page hands out does
/// not survive a restart, but a file on disk does.
///
/// **Real environment variables always win.** The file is a fallback source,
/// consulted only for keys the real environment does not already set -- so
/// an operator can override a single value at the shell (e.g. to point one
/// run at a different endpoint) without touching the file. Proven by
/// [`tests::real_env_var_overrides_the_same_key_in_the_config_file`].
#[derive(Clone)]
pub struct Config {
    pub endpoint: String,
    pub join_token: String,
    pub ca_cert_path: String,
    pub data_dir: String,
}

impl Config {
    /// Build from real environment variables only. Unchanged behavior for
    /// callers that never pass `--config`.
    pub fn from_env() -> Result<Self> {
        Self::from_sources(&HashMap::new())
    }

    /// Build from `argv`: if `--config <path>` is present, parse that file
    /// and use it as a fallback source (env vars still win per key, see the
    /// struct doc comment); otherwise this is identical to
    /// [`Config::from_env`].
    ///
    /// A `--config` path that cannot be read is a hard startup error, not a
    /// silent fallback to bare env vars -- an operator who pointed at a file
    /// wants that file.
    pub fn load(args: &[String]) -> Result<Self> {
        match config_file_path(args) {
            Some(path) => {
                let contents = std::fs::read_to_string(&path)
                    .with_context(|| format!("failed to read --config file {path}"))?;
                Self::from_sources(&parse_env_file(&contents))
            }
            None => Self::from_env(),
        }
    }

    /// Resolve every field with env-first lookup against `file`: a real
    /// environment variable, when set, always wins over the same key in
    /// `file`.
    fn from_sources(file: &HashMap<String, String>) -> Result<Self> {
        use argus_common::env;
        let lookup = |key: &str| std::env::var(key).ok().or_else(|| file.get(key).cloned());

        let data_dir =
            lookup(env::DATA_DIR).unwrap_or_else(|| argus_common::AGENT_DATA_DIR.to_string());
        Ok(Self {
            endpoint: lookup(env::AGENT_ENDPOINT)
                .with_context(|| format!("missing required env var {}", env::AGENT_ENDPOINT))?,
            join_token: lookup(env::JOIN_TOKEN)
                .with_context(|| format!("missing required env var {}", env::JOIN_TOKEN))?,
            ca_cert_path: lookup(env::CA_CERT_PATH).unwrap_or_else(|| format!("{data_dir}/ca.crt")),
            data_dir,
        })
    }
}

/// Look for `--config <path>` anywhere in the trailing args (mirrors
/// `argus`'s `--username` flag parsing in `crates/server/src/main.rs`).
fn config_file_path(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--config" {
            return iter.next().cloned();
        }
    }
    None
}

/// Keys `--config` recognizes. Anything else in the file is ignored (with a
/// warning) rather than rejected: a hard error on an unknown key would break
/// the file's other purpose as a systemd `EnvironmentFile=`, which also
/// tolerates stray keys, and an operator's typo shouldn't stop the agent from
/// starting on every OTHER key it got right.
const KNOWN_KEYS: &[&str] = &[
    argus_common::env::AGENT_ENDPOINT,
    argus_common::env::JOIN_TOKEN,
    argus_common::env::CA_CERT_PATH,
    argus_common::env::DATA_DIR,
];

/// Parse an `EnvironmentFile=`-compatible env-file: one `KEY=VALUE` per line;
/// blank lines and `#`-comment lines ignored; split on the FIRST `=` so a
/// value may itself contain `=`; keys trimmed; a value's surrounding matching
/// quotes (single OR double) stripped. This is deliberately a subset of
/// systemd's `EnvironmentFile=` syntax so the same file `--config` reads can
/// later be dropped in unchanged as a unit's `EnvironmentFile=`.
///
/// Keys not in [`KNOWN_KEYS`] are logged via `tracing::warn!` naming the key
/// (typo detection) and otherwise ignored -- see [`KNOWN_KEYS`]'s doc comment
/// for why that is a warning and not a hard error.
fn parse_env_file(contents: &str) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if !KNOWN_KEYS.contains(&key) {
            tracing::warn!(
                key,
                "unknown key in --config env-file, ignoring (check for a typo)"
            );
            continue;
        }
        vars.insert(key.to_string(), strip_matching_quotes(value));
    }
    vars
}

/// Strip one layer of surrounding quotes from `value` IF the first and last
/// byte are the same quote character (both `"` or both `'`). Anything else --
/// no quotes, mismatched quotes, or a single stray quote character -- is
/// returned unchanged, matching `EnvironmentFile=`'s tolerance of unquoted
/// values that happen to contain a quote byte.
fn strip_matching_quotes(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // `std::env::var`/`set_var` are process-global, so tests that touch real
    // env vars must not run concurrently with each other -- otherwise one
    // test's `set_var` leaks into another's `from_sources` lookup and the
    // failure is nondeterministic (flaky depending on `cargo test`'s thread
    // interleaving, not on the code under test). A single mutex serializes
    // just this module's env-touching tests; every other agent test module is
    // unaffected.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn parses_the_four_known_keys() {
        let vars = parse_env_file(
            "ARGUS_AGENT_ENDPOINT=https://agents.example:9443\n\
             ARGUS_JOIN_TOKEN=devtoken\n\
             ARGUS_CA_CERT=/etc/argus/ca.crt\n\
             ARGUS_DATA_DIR=/var/lib/argus-agent\n",
        );
        assert_eq!(
            vars.get("ARGUS_AGENT_ENDPOINT").map(String::as_str),
            Some("https://agents.example:9443")
        );
        assert_eq!(
            vars.get("ARGUS_JOIN_TOKEN").map(String::as_str),
            Some("devtoken")
        );
        assert_eq!(
            vars.get("ARGUS_CA_CERT").map(String::as_str),
            Some("/etc/argus/ca.crt")
        );
        assert_eq!(
            vars.get("ARGUS_DATA_DIR").map(String::as_str),
            Some("/var/lib/argus-agent")
        );
    }

    #[test]
    fn splits_on_the_first_equals_only() {
        // A join token or endpoint URL can itself contain `=` (e.g. a query
        // string or base64-ish token); only the FIRST `=` may be the
        // key/value separator.
        let vars = parse_env_file("ARGUS_JOIN_TOKEN=abc=def=ghi\n");
        assert_eq!(
            vars.get("ARGUS_JOIN_TOKEN").map(String::as_str),
            Some("abc=def=ghi")
        );
    }

    #[test]
    fn strips_matching_surrounding_quotes() {
        let vars = parse_env_file(
            "ARGUS_JOIN_TOKEN=\"double-quoted\"\n\
             ARGUS_CA_CERT='single-quoted'\n\
             ARGUS_AGENT_ENDPOINT=unquoted\n",
        );
        assert_eq!(
            vars.get("ARGUS_JOIN_TOKEN").map(String::as_str),
            Some("double-quoted")
        );
        assert_eq!(
            vars.get("ARGUS_CA_CERT").map(String::as_str),
            Some("single-quoted")
        );
        assert_eq!(
            vars.get("ARGUS_AGENT_ENDPOINT").map(String::as_str),
            Some("unquoted")
        );
    }

    #[test]
    fn leaves_mismatched_or_partial_quotes_alone() {
        // Mismatched quote characters, and a single stray quote byte, are not
        // a matching pair -- left exactly as written rather than guessed at.
        let vars = parse_env_file(
            "ARGUS_JOIN_TOKEN=\"mismatched'\n\
             ARGUS_CA_CERT=\"\n",
        );
        assert_eq!(
            vars.get("ARGUS_JOIN_TOKEN").map(String::as_str),
            Some("\"mismatched'")
        );
        assert_eq!(vars.get("ARGUS_CA_CERT").map(String::as_str), Some("\""));
    }

    #[test]
    fn ignores_blank_lines_and_comments() {
        let vars = parse_env_file(
            "# a leading comment\n\
             \n\
             ARGUS_JOIN_TOKEN=devtoken\n\
             \n\
             # ARGUS_CA_CERT=/should/not/parse\n\
             ARGUS_CA_CERT=/etc/argus/ca.crt\n",
        );
        assert_eq!(vars.len(), 2);
        assert_eq!(
            vars.get("ARGUS_JOIN_TOKEN").map(String::as_str),
            Some("devtoken")
        );
        assert_eq!(
            vars.get("ARGUS_CA_CERT").map(String::as_str),
            Some("/etc/argus/ca.crt")
        );
    }

    #[test]
    fn unknown_keys_are_dropped_not_stored() {
        // The warning itself is only observable via tracing output, which
        // this unit test does not capture -- but the structural half of "warn
        // and ignore" (the key never reaches the returned map, so it can
        // never masquerade as a known field) is exactly what this asserts.
        let vars = parse_env_file(
            "ARGUS_AGENT_ENDPOIN=https://typo.example\n\
             ARGUS_JOIN_TOKEN=devtoken\n",
        );
        assert_eq!(vars.len(), 1);
        assert!(!vars.contains_key("ARGUS_AGENT_ENDPOIN"));
        assert_eq!(
            vars.get("ARGUS_JOIN_TOKEN").map(String::as_str),
            Some("devtoken")
        );
    }

    #[test]
    fn keys_are_trimmed() {
        let vars = parse_env_file("  ARGUS_JOIN_TOKEN =devtoken\n");
        assert_eq!(
            vars.get("ARGUS_JOIN_TOKEN").map(String::as_str),
            Some("devtoken")
        );
    }

    #[test]
    fn config_file_path_finds_the_flag_value() {
        let args: Vec<String> = ["argus-agent", "--config", "/tmp/argus-agent.env"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            config_file_path(&args),
            Some("/tmp/argus-agent.env".to_string())
        );
    }

    #[test]
    fn config_file_path_is_none_when_the_flag_is_absent() {
        let args: Vec<String> = ["argus-agent"].iter().map(|s| s.to_string()).collect();
        assert_eq!(config_file_path(&args), None);
    }

    /// The precedence rule this module exists to guarantee: a real
    /// environment variable for a key beats a config-file value for the SAME
    /// key, even when the file also sets it. Serialized on `ENV_LOCK` (see
    /// its doc comment) since it mutates real process env vars.
    #[test]
    fn real_env_var_overrides_the_same_key_in_the_config_file() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let key = argus_common::env::AGENT_ENDPOINT;

        // SAFETY: serialized by ENV_LOCK above, so no other test/thread reads
        // or mutates this process's environment concurrently with this call.
        unsafe { std::env::set_var(key, "https://from-real-env:9443") };

        let mut file = HashMap::new();
        file.insert(key.to_string(), "https://from-file:9443".to_string());
        file.insert(
            argus_common::env::JOIN_TOKEN.to_string(),
            "devtoken".to_string(),
        );

        let cfg = Config::from_sources(&file).expect("both required keys are present");
        assert_eq!(cfg.endpoint, "https://from-real-env:9443");

        // SAFETY: still serialized by ENV_LOCK.
        unsafe { std::env::remove_var(key) };
    }

    /// The complementary half of the same precedence rule: when the real
    /// environment does NOT set a key, the file value is used as the
    /// fallback rather than the lookup failing outright.
    #[test]
    fn file_value_is_used_when_the_real_env_var_is_absent() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let key = argus_common::env::CA_CERT_PATH;

        // SAFETY: serialized by ENV_LOCK; ensures a stray value from the test
        // runner's own environment cannot masquerade as "absent".
        unsafe { std::env::remove_var(key) };

        let mut file = HashMap::new();
        file.insert(
            argus_common::env::AGENT_ENDPOINT.to_string(),
            "https://agents.example:9443".to_string(),
        );
        file.insert(
            argus_common::env::JOIN_TOKEN.to_string(),
            "devtoken".to_string(),
        );
        file.insert(key.to_string(), "/etc/argus/from-file-ca.crt".to_string());

        let cfg = Config::from_sources(&file).expect("both required keys are present");
        assert_eq!(cfg.ca_cert_path, "/etc/argus/from-file-ca.crt");
    }

    #[test]
    fn missing_required_key_in_both_sources_is_an_error() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialized by ENV_LOCK.
        unsafe { std::env::remove_var(argus_common::env::AGENT_ENDPOINT) };

        let mut file = HashMap::new();
        file.insert(
            argus_common::env::JOIN_TOKEN.to_string(),
            "devtoken".to_string(),
        );
        match Config::from_sources(&file) {
            Ok(_) => panic!("endpoint is set nowhere; from_sources should have errored"),
            Err(err) => assert!(err.to_string().contains(argus_common::env::AGENT_ENDPOINT)),
        }
    }
}
