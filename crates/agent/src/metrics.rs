//! Host metrics sampler: reads CPU/mem/swap/load/disk/net/uptime via
//! `sysinfo` into a proto `MetricsSample`; sending happens elsewhere.
//!
//! `refresh_cpu_usage` reports a *delta* since the last call (min
//! `MINIMUM_CPU_UPDATE_INTERVAL`, ~200ms); `Sampler::new` takes a throwaway
//! reading so the first real `sample()` has one to diff against.
//! `load_average()`/`uptime()` read fresh OS state regardless of refresh
//! state. Network counters are cumulative; deltas are computed
//! control-plane side (see the proto's `net_rx_bytes` comment).
//!
//! `agent_version` lives on `AgentInfo` (sent once in `Hello`), not on
//! `MetricsSample` -- `sample()` takes no such parameter.

use argus_proto::v1::MetricsSample;
use std::time::{SystemTime, UNIX_EPOCH};
use sysinfo::{Disks, Networks, System};

/// Long-lived host sampler: keeps `System`/`Networks`/`Disks` across calls
/// so repeated samples reuse refresh state, most importantly for accurate
/// CPU-usage deltas.
pub struct Sampler {
    sys: System,
    nets: Networks,
    disks: Disks,
}

impl Sampler {
    /// Takes an initial CPU/memory reading so the *next* `sample()`'s delta
    /// is meaningful (module doc: why the first `refresh_cpu_usage` is
    /// unreliable).
    pub fn new() -> Sampler {
        let mut sys = System::new();
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        let nets = Networks::new_with_refreshed_list();
        let disks = Disks::new_with_refreshed_list();
        Sampler { sys, nets, disks }
    }

    pub fn sample(&mut self) -> MetricsSample {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        self.nets.refresh(true);
        self.disks.refresh(true);

        let load = System::load_average();

        let mut disk_used = 0u64;
        let mut disk_total = 0u64;
        for disk in self.disks.iter() {
            disk_total = disk_total.saturating_add(disk.total_space());
            disk_used =
                disk_used.saturating_add(disk.total_space().saturating_sub(disk.available_space()));
        }

        let mut net_rx_bytes = 0u64;
        let mut net_tx_bytes = 0u64;
        for net in self.nets.values() {
            net_rx_bytes = net_rx_bytes.saturating_add(net.total_received());
            net_tx_bytes = net_tx_bytes.saturating_add(net.total_transmitted());
        }

        MetricsSample {
            unix_ms: unix_ms_now(),
            cpu_pct: self.sys.global_cpu_usage(),
            mem_used: self.sys.used_memory(),
            mem_total: self.sys.total_memory(),
            swap_used: self.sys.used_swap(),
            swap_total: self.sys.total_swap(),
            load1: load.one as f32,
            load5: load.five as f32,
            load15: load.fifteen as f32,
            disk_used,
            disk_total,
            net_rx_bytes,
            net_tx_bytes,
            uptime_secs: System::uptime(),
            extra_json: String::new(),
        }
    }
}

impl Default for Sampler {
    fn default() -> Self {
        Sampler::new()
    }
}

/// Wall-clock ms since epoch. Saturates rather than panicking if the clock
/// is set before 1970 (`duration_since` failing) -- telemetry is not worth
/// crashing the agent over.
fn unix_ms_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_populates_core_fields() {
        let mut s = Sampler::new();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let m = s.sample();

        assert!(m.mem_total > 0, "mem_total should be non-zero");
        assert!(m.uptime_secs > 0, "uptime_secs should be non-zero");
        // CPU may legitimately read 0.0 on an idle host -- assert
        // non-negative, not non-zero.
        assert!(m.cpu_pct >= 0.0, "cpu_pct should be non-negative");
    }
}
