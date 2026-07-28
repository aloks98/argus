//! Host facts sent as `AgentInfo` (PRD §5.2). `gather()` runs both at
//! enrollment AND again on every session (re)connect -- `session.rs`'s sender
//! task calls it fresh on each attempt (not just once at startup) so a
//! resent `Hello` lets the fleet view self-heal after a reconnect.
//!
//! Everything here is best-effort: a field the host doesn't expose (e.g. no
//! `/etc/os-release`, no default route) is reported as an empty string rather
//! than failing enrollment. `machine_id` is the one field the control plane
//! requires to be non-empty (`grpc.rs` rejects enrollment without it).

use anyhow::{Context, Result};
use argus_proto::v1::AgentInfo;
use std::sync::OnceLock;

/// CPU model/cores, boot time, and detected virtualization: all
/// process-lifetime-stable, so they're computed once (first `gather()` call)
/// and cached, rather than every call. Without this, a flapping connection --
/// at the reconnect loop's 500ms backoff floor, up to ~2 attempts/sec --
/// would turn into a `/proc` scan plus a `systemd-detect-virt` fork+exec on
/// every single attempt.
struct ProcessInventory {
    cpu_model: String,
    cpu_cores: u32,
    boot_time: i64,
    virt: String,
}

static INVENTORY: OnceLock<ProcessInventory> = OnceLock::new();

/// Compute (once) and return the cached process-lifetime inventory facts.
fn process_inventory() -> &'static ProcessInventory {
    INVENTORY.get_or_init(|| {
        let (cpu_model, cpu_cores) = cpu_info();
        ProcessInventory {
            cpu_model,
            cpu_cores,
            boot_time: sysinfo::System::boot_time() as i64,
            virt: detect_virt(),
        }
    })
}

/// Gather this host's facts for the `Enroll` RPC and for every `Hello`.
/// `agent_version` is passed in (baked into the binary at build time) rather
/// than read from the environment.
pub fn gather(agent_version: &str) -> Result<AgentInfo> {
    let uname = rustix::system::uname();
    let inventory = process_inventory();

    Ok(AgentInfo {
        hostname: uname.nodename().to_string_lossy().into_owned(),
        machine_id: read_machine_id().context("reading /etc/machine-id")?,
        os: read_os_pretty_name(),
        kernel: uname.release().to_string_lossy().into_owned(),
        primary_ip: primary_ip(),
        arch: uname.machine().to_string_lossy().into_owned(),
        agent_version: agent_version.to_string(),
        // The session layer overwrites these with the real probed values
        // after every gather() call (see session.rs's sender task) -- gather()
        // itself has no access to the docker/systemd clients needed to probe
        // them, so it always reports the "unknown" defaults here.
        capabilities: Vec::new(),
        capabilities_reported: false,
        cpu_model: inventory.cpu_model.clone(),
        cpu_cores: inventory.cpu_cores,
        boot_time: inventory.boot_time,
        virt: inventory.virt.clone(),
    })
}

/// CPU brand + logical core count via sysinfo (already a dependency for the
/// metrics sampler). Best-effort: an empty brand string is simply reported
/// empty and becomes NULL server-side. Called once, from `process_inventory`.
fn cpu_info() -> (String, u32) {
    let mut sys = sysinfo::System::new();
    sys.refresh_cpu_all();
    let cpus = sys.cpus();
    let model = cpus
        .first()
        .map(|c| c.brand().trim().to_string())
        .unwrap_or_default();
    (model, cpus.len() as u32)
}

/// `systemd-detect-virt` stdout, trimmed. CAREFUL: the command exits
/// NON-ZERO for its most interesting answer -- "none" (bare metal) -- so
/// failure is judged on spawn error / empty output, never on exit status.
/// A host without the binary (Alpine, containers without systemd) reports
/// empty -> NULL server-side. Called once, from `process_inventory` (a
/// fork+exec per reconnect would be wasteful -- see that struct's doc).
fn detect_virt() -> String {
    match std::process::Command::new("systemd-detect-virt").output() {
        Ok(out) => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        Err(_) => String::new(),
    }
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

    #[test]
    fn gather_returns_non_empty_cpu_boot_time_and_virt() {
        let info = gather("test").expect("gather should succeed on this host");
        assert!(!info.cpu_model.is_empty(), "cpu_model must not be empty");
        assert!(info.cpu_cores > 0, "cpu_cores must be > 0");
        assert!(info.boot_time > 0, "boot_time must be > 0");
        // `systemd-detect-virt` exists on the dev box and always produces
        // *some* answer -- "none" for bare metal is a real, non-empty
        // result, not a failure (see `detect_virt`'s doc comment).
        assert!(!info.virt.is_empty(), "virt must not be empty on this host");
    }
}
