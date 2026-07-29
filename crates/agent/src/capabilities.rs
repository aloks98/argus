//! What this host can actually do, probed once per session immediately
//! before `Hello` so it re-reports on every reconnect. Every probe is
//! timeout-bounded; a timeout reports the capability ABSENT rather than
//! risking a stall, since reconnect must stay reliable for self-healing.

use crate::docker::DockerClient;
use crate::systemd::SystemdClient;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// Per-probe ceiling. Systemd's check is synchronous (no wait); docker and
/// journal checks run concurrently, so `probe()`'s worst case is about one
/// `PROBE_TIMEOUT`, not the sum of all three.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Order is stable (systemd, docker, journal) so tests and audit output
/// don't depend on iteration order.
pub fn build_set(has_systemd: bool, has_docker: bool, has_journal: bool) -> Vec<String> {
    let mut caps = Vec::new();
    if has_systemd {
        caps.push(argus_common::CAP_SYSTEMD.to_string());
    }
    if has_docker {
        caps.push(argus_common::CAP_DOCKER.to_string());
    }
    if has_journal {
        caps.push(argus_common::CAP_JOURNAL.to_string());
    }
    caps
}

/// `--version` proves `journalctl` is runnable, not that the journal is
/// readable -- a non-root agent gets exit 0 and no output, indistinguishable
/// from an empty journal (surfaces later as a log-stream marker line).
async fn has_journal() -> bool {
    let fut = Command::new("journalctl")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status();
    matches!(tokio::time::timeout(PROBE_TIMEOUT, fut).await, Ok(Ok(s)) if s.success())
}

pub async fn probe(systemd: &SystemdClient, docker: &DockerClient) -> Vec<String> {
    // zbus doesn't self-heal, but systemd is re-dialed fresh each reconnect
    // attempt, so this field read reflects current availability.
    let systemd_ok = systemd.is_available();
    // `connect_with_socket_defaults` only constructs a client, never contacts
    // the daemon, so this pings for real. Run concurrently with the journal
    // check to keep `probe()`'s bound at ~1x `PROBE_TIMEOUT`, not 2x.
    let (docker_ok, journal_ok) = tokio::join!(docker.ping_ok(PROBE_TIMEOUT), has_journal());
    build_set(systemd_ok, docker_ok, journal_ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_set_lists_only_what_is_present() {
        assert_eq!(
            build_set(true, false, true),
            vec![
                argus_common::CAP_SYSTEMD.to_string(),
                argus_common::CAP_JOURNAL.to_string()
            ]
        );
    }

    #[test]
    fn build_set_is_empty_when_nothing_is_available() {
        // A bare host (e.g. Alpine LXC) reports an EMPTY set -- distinct
        // from an old agent reporting nothing, which the server tells apart
        // via `capabilities_reported`.
        assert!(build_set(false, false, false).is_empty());
    }

    #[test]
    fn build_set_lists_all_three_when_everything_is_present() {
        let all = build_set(true, true, true);
        assert!(all.contains(&argus_common::CAP_SYSTEMD.to_string()));
        assert!(all.contains(&argus_common::CAP_DOCKER.to_string()));
        assert!(all.contains(&argus_common::CAP_JOURNAL.to_string()));
        assert_eq!(all.len(), 3);
    }

    /// This host has systemd + a readable journal, so both must be reported;
    /// docker depends on whether a daemon answers, so only assert
    /// consistency with a direct ping.
    #[tokio::test]
    #[ignore = "probes the real host; run under sudo on a systemd host"]
    async fn live_probe_reports_this_hosts_capabilities() {
        let systemd = crate::systemd::SystemdClient::connect().await;
        let docker = crate::docker::DockerClient::connect();
        let caps = probe(&systemd, &docker).await;

        assert!(
            caps.contains(&argus_common::CAP_SYSTEMD.to_string()),
            "systemd is present on this host, got {caps:?}"
        );
        assert!(
            caps.contains(&argus_common::CAP_JOURNAL.to_string()),
            "journalctl is present on this host, got {caps:?}"
        );
        assert_eq!(
            caps.contains(&argus_common::CAP_DOCKER.to_string()),
            docker.ping_ok(PROBE_TIMEOUT).await,
            "the reported docker capability must match a direct ping"
        );
    }
}
