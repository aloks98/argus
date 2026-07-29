use anyhow::{Context, Result};
use std::collections::HashMap;

/// Agent config, sourced from env vars (baked into the Ignition/cloud-init
/// template with the join token + CA cert -- PRD §5.1) or, via `--config
/// <path>`, an env-file -- the `sudo -n env VAR=... ./argus-agent` one-shot
/// recipe doesn't survive a restart, but a file does.
///
/// **Real env vars always win** over the same key in the file, letting an
/// operator override one value at the shell without touching it.
#[derive(Clone)]
pub struct Config {
    pub endpoint: String,
    pub join_token: String,
    pub ca_cert_path: String,
    pub data_dir: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_sources(&HashMap::new())
    }

    /// Build from `argv`: `--config <path>` if present, else
    /// [`Config::from_env`]. Env vars still win per key over the file.
    ///
    /// An unreadable or DANGLING (no path) `--config` is a hard startup
    /// error, never a silent fallback to bare env vars -- an operator who
    /// pointed at a file wants that file.
    pub fn load(args: &[String]) -> Result<Self> {
        match config_file_path(args)? {
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

/// Finds `--config <path>` in the trailing args (mirrors `argus`'s
/// `--username` parsing), except a DANGLING `--config` is a hard error --
/// see [`Config::load`].
fn config_file_path(args: &[String]) -> Result<Option<String>> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--config" {
            return match iter.next() {
                Some(path) => Ok(Some(path.clone())),
                None => anyhow::bail!("--config requires a path argument"),
            };
        }
    }
    Ok(None)
}

/// Keys `--config` recognizes. Unknown keys are warned-and-ignored, not
/// rejected: a hard error would break the file's dual use as a systemd
/// `EnvironmentFile=`, which also tolerates stray keys.
const KNOWN_KEYS: &[&str] = &[
    argus_common::env::AGENT_ENDPOINT,
    argus_common::env::JOIN_TOKEN,
    argus_common::env::CA_CERT_PATH,
    argus_common::env::DATA_DIR,
];

/// Parses a subset of systemd's `EnvironmentFile=` syntax (`KEY=VALUE`
/// lines, `#` comments, quote-stripping) -- deliberately so the same file
/// `--config` reads can later be dropped in unchanged as a unit's
/// `EnvironmentFile=`.
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

/// Strips one layer of matching surrounding quotes; anything else is left
/// unchanged, matching `EnvironmentFile=`'s tolerance of unquoted values
/// that contain a quote byte.
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

    // `std::env::var`/`set_var` are process-global: tests that touch real
    // env vars must not run concurrently, or one test's `set_var` leaks into
    // another's lookup, producing flakiness that depends on `cargo test`'s
    // thread interleaving, not the code under test.
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
        // A join token/URL can itself contain `=`; only the FIRST `=` is
        // the separator.
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
        // Only the structural half of "warn and ignore" is asserted here --
        // the warning itself is tracing output, not captured by this test.
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
            config_file_path(&args).expect("path is present"),
            Some("/tmp/argus-agent.env".to_string())
        );
    }

    #[test]
    fn config_file_path_is_none_when_the_flag_is_absent() {
        let args: Vec<String> = ["argus-agent"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            config_file_path(&args).expect("flag is absent, not an error"),
            None
        );
    }

    /// See [`Config::load`] for why a dangling `--config` must be a hard
    /// error rather than treated as absent.
    #[test]
    fn config_file_path_errors_when_the_flag_is_dangling() {
        let args: Vec<String> = ["argus-agent", "--config"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let err = config_file_path(&args).expect_err("dangling --config must be an error");
        assert!(
            err.to_string()
                .contains("--config requires a path argument"),
            "error must name the problem, got: {err}"
        );
    }

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
