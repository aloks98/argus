//! Host metrics sampler: reads CPU/mem/swap/load/disk/net/uptime via
//! `sysinfo` and fills a proto `MetricsSample`. Sending it over the Session
//! (multiplexed as an `AgentFrame::metrics` payload) happens elsewhere --
//! this module only produces the sample.
//!
//! `System::refresh_cpu_usage` reports a *delta* between two calls at least
//! `MINIMUM_CPU_UPDATE_INTERVAL` (~200ms) apart; the first call after
//! construction has no prior reading to diff against, hence `Sampler::new`
//! takes one throwaway reading so the next `sample()` gets a real delta.
//! `System::load_average()` and `System::uptime()` are **associated
//! functions** (no `&self`) that read fresh OS state on every call,
//! independent of any `System` instance's refresh state.
//! `NetworkData::total_received`/`total_transmitted` are cumulative
//! counters -- deltas are computed control-plane side, per the proto's
//! comment on `net_rx_bytes`/`net_tx_bytes`.
//!
//! The proto `MetricsSample` has no `agent_version` field -- that lives on
//! `AgentInfo`, sent once in the `Hello` frame at connect time (see
//! `info.rs::gather` and `session.rs`); `sample()` takes no such parameter.

use argus_proto::v1::MetricsSample;
use std::time::{SystemTime, UNIX_EPOCH};
use sysinfo::{Disks, Networks, System};

/// Long-lived host sampler. Keeps `System`/`Networks`/`Disks` around across
/// calls so repeated samples reuse refresh state -- most importantly, so
/// CPU-usage deltas are accurate -- instead of re-enumerating hardware every
/// tick.
pub struct Sampler {
    sys: System,
    nets: Networks,
    disks: Disks,
}

impl Sampler {
    /// Construct the sampler and take an initial CPU/memory reading so the
    /// *next* `sample()` call's CPU-usage delta is meaningful (see the module
    /// doc comment on why the first `refresh_cpu_usage` call is unreliable).
    pub fn new() -> Sampler {
        let mut sys = System::new();
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        let nets = Networks::new_with_refreshed_list();
        let disks = Disks::new_with_refreshed_list();
        Sampler { sys, nets, disks }
    }

    /// Refresh all sources and fill a `MetricsSample` snapshot.
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

/// Current wall-clock time in milliseconds since the Unix epoch. Saturates
/// rather than panicking in the pathological case of a clock set before 1970
/// (`duration_since` failing) -- this is telemetry, not a place to crash the
/// agent.
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
