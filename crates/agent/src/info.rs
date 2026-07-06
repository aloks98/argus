//! Host facts gathered at enrollment time and sent as `AgentInfo` (PRD §5.2).
//!
//! Everything here is best-effort: a field the host doesn't expose (e.g. no
//! `/etc/os-release`, no default route) is reported as an empty string rather
//! than failing enrollment. `machine_id` is the one field the control plane
//! requires to be non-empty (`grpc.rs` rejects enrollment without it).

use anyhow::{Context, Result};
use argus_proto::v1::AgentInfo;

/// Gather this host's facts for the `Enroll` RPC. `agent_version` is passed in
/// (baked into the binary at build time) rather than read from the environment.
pub fn gather(agent_version: &str) -> Result<AgentInfo> {
    let uname = rustix::system::uname();

    Ok(AgentInfo {
        hostname: uname.nodename().to_string_lossy().into_owned(),
        machine_id: read_machine_id().context("reading /etc/machine-id")?,
        os: read_os_pretty_name(),
        kernel: uname.release().to_string_lossy().into_owned(),
        primary_ip: primary_ip(),
        arch: uname.machine().to_string_lossy().into_owned(),
        agent_version: agent_version.to_string(),
    })
}

/// `/etc/machine-id` -- stable identity across reboots (present on Debian and
/// on Flatcar). Trimmed of trailing whitespace/newline.
pub(crate) fn read_machine_id() -> Result<String> {
    let raw = std::fs::read_to_string("/etc/machine-id")?;
    Ok(raw.trim().to_string())
}

/// `PRETTY_NAME` out of `/etc/os-release`, quotes stripped. Empty string if the
/// file is missing or the key isn't present -- this is a display nicety, not a
/// required field.
fn read_os_pretty_name() -> String {
    let Ok(contents) = std::fs::read_to_string("/etc/os-release") else {
        return String::new();
    };
    for line in contents.lines() {
        if let Some(value) = line.strip_prefix("PRETTY_NAME=") {
            return value.trim().trim_matches('"').to_string();
        }
    }
    String::new()
}

/// The local address that would be used to reach the outside world, via the
/// classic UDP-connect trick (no packets are actually sent since UDP is
/// connectionless -- this just asks the kernel to pick a route). Empty string
/// on any failure; this is best-effort informational data, not load-bearing.
fn primary_ip() -> String {
    (|| -> std::io::Result<String> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
        socket.connect("192.168.150.1:80")?;
        Ok(socket.local_addr()?.ip().to_string())
    })()
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gather_returns_non_empty_hostname_arch_and_machine_id() {
        let info = gather("test").expect("gather should succeed on this host");
        assert!(!info.hostname.is_empty(), "hostname must not be empty");
        assert!(!info.arch.is_empty(), "arch must not be empty");
        assert!(!info.machine_id.is_empty(), "machine_id must not be empty");
        assert_eq!(info.agent_version, "test");
    }
}
