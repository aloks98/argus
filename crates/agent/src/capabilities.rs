//! What this host can actually do.
//!
//! Probed once per session, immediately before `Hello`, so the set re-reports on
//! every reconnect. Every probe is timeout-bounded and a timeout reports the
//! capability ABSENT: the agent's self-healing rests on reconnect being
//! reliable, so a capability probe must never become a new way for session open
//! to stall.
//!
//! Called from `session::connect_and_serve` immediately before the `Hello`
//! sender task is spawned.

use crate::docker::DockerClient;
use crate::systemd::SystemdClient;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// Per-probe ceiling. Generous enough for a busy host, short enough that the
/// probe as a whole cannot meaningfully delay a reconnect: the systemd check
/// is a synchronous field read (no wait), and the docker and journal checks
/// run concurrently, so the worst case for `probe()` is about one
/// `PROBE_TIMEOUT`, not the sum of all three.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Assemble the reported set. Order is stable so tests and audit output do not
/// depend on iteration order.
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

/// Is `journalctl` present and runnable? `--version` answers exactly that.
///
/// It deliberately does NOT prove the journal is readable: a non-root agent gets
/// a `journalctl` that exits 0 and prints nothing, which no exit status can
/// distinguish from an empty journal. A permission failure stays a runtime
/// concern and already surfaces as a marker line in the log stream.
async fn has_journal() -> bool {
    let fut = Command::new("journalctl")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status();
    matches!(tokio::time::timeout(PROBE_TIMEOUT, fut).await, Ok(Ok(s)) if s.success())
}

/// Probe every subsystem and return the capability set for `AgentInfo`.
pub async fn probe(systemd: &SystemdClient, docker: &DockerClient) -> Vec<String> {
    // systemd is already re-dialed fresh on each reconnect attempt (zbus does
    // not self-heal), so its availability is current without another round trip.
    let systemd_ok = systemd.is_available();
    // Docker needs a REAL ping: `connect_with_socket_defaults` only constructs a
    // client and never contacts the daemon, so trusting it would claim `docker`
    // on a host where dockerd is installed but stopped. Run it concurrently with
    // the journal check so the two async probes overlap instead of stacking --
    // the bound on `probe()` stays ~1x `PROBE_TIMEOUT`, not 2x.
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
        // A bare host (e.g. an Alpine LXC) reports an EMPTY set, which is
        // meaningfully different from an old agent reporting nothing at all --
        // the server distinguishes them via `capabilities_reported`.
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

    /// The real probes against this machine. This host runs systemd with a
    /// readable journal, so both must be reported; docker depends on whether a
    /// daemon is answering, so only assert the call completes and is consistent
    /// with a direct ping.
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
