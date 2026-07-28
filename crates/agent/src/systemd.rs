//! Systemd collection + unit verb execution. Talks to the local D-Bus
//! **system bus** via zbus — no network, no TLS, so the musl-static build
//! stays clean. Mapping and policy are factored into pure functions so they are
//! unit-testable without a running bus (mirrors `docker.rs`).
//!
//! Wired into `session.rs`: a fresh `SystemdClient` is dialed per session
//! attempt and its `list_units`/`run_verb` drive the `SystemdState` snapshot
//! and unit-verb routing.

use argus_proto::v1::{CommandResult, Unit as ProtoUnit, Verb};
use futures_util::StreamExt;
use std::time::Duration;
use zbus::proxy;
use zvariant::OwnedObjectPath;

/// One row of `ListUnitsByPatterns` — systemd's `(ssssssouso)` unit struct.
/// `zvariant`'s positional-tuple decoding requires every field the wire type
/// carries; `record_to_unit` only consumes 5 of the 10. The rest are real
/// dead code (never read), not a false positive -- allowed here, scoped to
/// the struct rather than the module, because dropping them would break
/// deserialization of the D-Bus reply.
#[derive(Debug, Clone, serde::Deserialize, zvariant::Type)]
#[allow(dead_code)]
pub struct UnitRecord {
    pub name: String,
    pub description: String,
    pub load_state: String,
    pub active_state: String,
    pub sub_state: String,
    pub followed: String,
    pub object_path: OwnedObjectPath,
    pub job_id: u32,
    pub job_type: String,
    pub job_object_path: OwnedObjectPath,
}

#[proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
pub trait Manager {
    fn subscribe(&self) -> zbus::Result<()>;

    fn list_units_by_patterns(
        &self,
        states: Vec<String>,
        patterns: Vec<String>,
    ) -> zbus::Result<Vec<UnitRecord>>;

    fn start_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    fn stop_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    fn restart_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;

    // The explicit `name` is load-bearing: zbus derives `GetUnitByPid` from the
    // snake_case fn, but systemd's method is `GetUnitByPID`. Without this the
    // call fails with `UnknownMethod` on every host, self-unit discovery
    // silently falls back to the compiled-in name, and the self-preservation
    // guard quietly stops being the runtime check it is meant to be.
    #[zbus(name = "GetUnitByPID")]
    fn get_unit_by_pid(&self, pid: u32) -> zbus::Result<OwnedObjectPath>;

    #[zbus(signal)]
    fn job_removed(
        &self,
        id: u32,
        job: OwnedObjectPath,
        unit: String,
        result: String,
    ) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.freedesktop.systemd1.Unit",
    default_service = "org.freedesktop.systemd1"
)]
pub trait Unit {
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
}

/// Unit-name suffixes systemd recognises. A target carrying none of these is a
/// bare service name, and gets `.service` appended — same rule `systemctl` uses.
const UNIT_SUFFIXES: &[&str] = &[
    ".service",
    ".socket",
    ".device",
    ".mount",
    ".automount",
    ".swap",
    ".target",
    ".path",
    ".timer",
    ".slice",
    ".scope",
];

/// Map a systemd `ListUnitsByPatterns` row onto the proto's five fields. Pure.
fn record_to_unit(r: UnitRecord) -> ProtoUnit {
    ProtoUnit {
        name: r.name,
        load_state: r.load_state,
        active_state: r.active_state,
        sub_state: r.sub_state,
        description: r.description,
    }
}

/// Union the two pattern queries, deduplicated by unit name. A failed
/// `*.service` matches both and must appear exactly once. Services come first
/// so the ordering is stable regardless of what the failed query returns.
fn merge_units(services: Vec<UnitRecord>, failed: Vec<UnitRecord>) -> Vec<ProtoUnit> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(services.len() + failed.len());
    for r in services.into_iter().chain(failed) {
        if seen.insert(r.name.clone()) {
            out.push(record_to_unit(r));
        }
    }
    out
}

/// Append `.service` unless the name already carries a unit suffix.
fn normalize_target(target: &str) -> String {
    if UNIT_SUFFIXES.iter().any(|s| target.ends_with(s)) {
        target.to_string()
    } else {
        format!("{target}.service")
    }
}

/// Whether `target` names the unit hosting this agent. Compared after
/// normalization so `argus-agent` and `argus-agent.service` both match, and by
/// full equality so `argus-agent-proxy.service` does not.
fn is_self_unit(target: &str, self_unit: Option<&str>) -> bool {
    match self_unit {
        Some(me) => normalize_target(target) == normalize_target(me),
        None => false,
    }
}

/// Build a `CommandResult` from systemd's own job-result string. Only `done`
/// means the job actually succeeded.
fn job_result_to_command_result(command_id: String, job_result: &str) -> CommandResult {
    if job_result == "done" {
        result(command_id, true, "done")
    } else {
        result(
            command_id,
            false,
            &format!("systemd job result: {job_result}"),
        )
    }
}

/// Shared `CommandResult` constructor, mirroring `docker.rs`.
fn result(command_id: String, ok: bool, message: &str) -> CommandResult {
    CommandResult {
        command_id,
        ok,
        exit_code: if ok { 0 } else { 1 },
        message: message.to_string(),
    }
}

/// Cap any single bus call so a wedged systemd can't stall the Session's
/// heartbeat/metrics sender. Matches `docker.rs`'s OP_TIMEOUT.
const OP_TIMEOUT: Duration = Duration::from_secs(5);

/// Verb jobs get far longer than a listing round-trip: this bounds a systemd
/// *job*, not a bus call, and systemd's own DefaultTimeoutStartSec is 90s. The
/// control plane's own bounded wait (10s) independently returns "pending" to the
/// browser, and the late CommandResult still resolves the audit row — so waiting
/// here costs the user nothing and avoids reporting real successes as failures.
const JOB_TIMEOUT: Duration = Duration::from_secs(90);

/// Fallback name for the agent's own unit, used only when `GetUnitByPID` fails
/// (i.e. the agent isn't running under systemd, where the guard is moot anyway).
/// Nothing in this repo pins the deployed unit name — the provisioning template
/// owns it (PRD §5.1) — which is exactly why the real value is discovered at
/// runtime rather than hard-coded here.
const FALLBACK_SELF_UNIT: &str = "argus-agent.service";

/// A cheaply-cloneable handle to the local systemd. `inner` is `None` on hosts
/// with no system bus (containers without systemd) — such a client reports an
/// empty unit list and fails verbs with a clear message, never panicking.
#[derive(Clone)]
pub struct SystemdClient {
    inner: Option<zbus::Connection>,
    /// The unit hosting this agent, discovered at connect. Verbs against it are
    /// refused: stopping it severs the session the result would return on.
    self_unit: Option<String>,
    /// Whether `Manager.Subscribe()` succeeded at connect. When false, systemd
    /// emits no job signals and every verb will burn `JOB_TIMEOUT` reporting
    /// "unconfirmed" — surfaced in that message so it's self-explaining rather
    /// than relying on the one startup warn.
    subscribed: bool,
}

impl SystemdClient {
    /// Whether a system-bus connection was established for this session.
    ///
    /// Consumed by `capabilities::probe()`, called once per session from
    /// `session::connect_and_serve` immediately before `Hello`.
    pub fn is_available(&self) -> bool {
        self.inner.is_some()
    }

    /// Best-effort connect to the system bus. Never fails.
    ///
    /// Every bus round-trip here is bounded by `OP_TIMEOUT`. This is called
    /// once per session attempt from the reconnect loop (see `session::run`
    /// for why: a `zbus::Connection` does not self-heal, so each attempt
    /// re-dials). Worst case — bus connect + `Subscribe` + self-unit
    /// discovery, each timing out — this costs roughly 15s before that
    /// attempt proceeds.
    pub async fn connect() -> SystemdClient {
        let conn = match tokio::time::timeout(OP_TIMEOUT, zbus::Connection::system()).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "systemd: no system bus; unit features disabled");
                return SystemdClient {
                    inner: None,
                    self_unit: None,
                    subscribed: false,
                };
            }
            Err(_) => {
                tracing::debug!(
                    "systemd: connecting to system bus timed out; unit features disabled"
                );
                return SystemdClient {
                    inner: None,
                    self_unit: None,
                    subscribed: false,
                };
            }
        };

        // systemd only emits job signals while at least one client is
        // subscribed. Without this, run_verb's JobRemoved wait always times out.
        let subscribed = match ManagerProxy::new(&conn).await {
            Ok(mgr) => match tokio::time::timeout(OP_TIMEOUT, mgr.subscribe()).await {
                Ok(Ok(())) => true,
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "systemd: Subscribe failed; verb results may time out");
                    false
                }
                Err(_) => {
                    tracing::warn!("systemd: Subscribe timed out; verb results may time out");
                    false
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "systemd: creating manager proxy failed; verb results may time out");
                false
            }
        };

        let self_unit = match tokio::time::timeout(OP_TIMEOUT, discover_self_unit(&conn)).await {
            Ok(unit) => unit,
            Err(_) => {
                tracing::warn!(
                    fallback = FALLBACK_SELF_UNIT,
                    "systemd: discovering this agent's own unit timed out; using the fallback name"
                );
                Some(FALLBACK_SELF_UNIT.to_string())
            }
        };
        tracing::debug!(self_unit = ?self_unit, "systemd: connected to system bus");
        SystemdClient {
            inner: Some(conn),
            self_unit,
            subscribed,
        }
    }

    /// Loaded `*.service` units plus any failed unit of any type, deduplicated.
    ///
    /// Returns `Some(units)` when collection succeeded — `Some(vec![])` is a
    /// legitimate "this host really has no matching units" — and `None` when
    /// it could not be collected at all: no bus, a query error, or a timeout.
    /// Callers must treat `None` as "unknown, do not overwrite the control
    /// plane's cache": reporting an empty list on a collection failure would
    /// render as "nothing is wrong on this host", the one wrong answer this
    /// slice must never give. A partial list that dropped the failed query
    /// would give the same wrong answer, which is why any error here discards
    /// the whole attempt rather than returning what was gathered so far.
    ///
    /// Residual limitation: this only protects a bus that dies *mid-session*,
    /// where the control plane still has a prior snapshot to keep. If the bus
    /// is already dead when the agent starts, there is no prior snapshot, so
    /// that host still shows an empty unit list. Closing that gap would need
    /// a proto change (e.g. an explicit "unknown" state), and the proto is
    /// frozen for this slice.
    pub async fn list_units(&self) -> Option<Vec<ProtoUnit>> {
        let Some(conn) = &self.inner else {
            return None;
        };
        match tokio::time::timeout(OP_TIMEOUT, list_units_inner(conn)).await {
            Ok(Ok(units)) => Some(units),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "systemd: listing units failed");
                None
            }
            Err(_) => {
                tracing::warn!("systemd: listing units timed out");
                None
            }
        }
    }

    /// Run a unit verb, reporting the real job outcome (not merely that the job
    /// was enqueued).
    pub async fn run_verb(&self, command_id: String, verb: Verb, target: &str) -> CommandResult {
        // Checked before touching the bus so it holds on every host.
        if is_self_unit(target, self.self_unit.as_deref()) {
            return result(
                command_id,
                false,
                "refusing to operate on the unit hosting this agent",
            );
        }
        let Some(conn) = &self.inner else {
            return result(
                command_id,
                false,
                "systemd system bus not available on this host",
            );
        };
        let unit = normalize_target(target);

        match tokio::time::timeout(JOB_TIMEOUT, run_verb_inner(conn, verb, &unit)).await {
            Ok(Ok(job_result)) => job_result_to_command_result(command_id, &job_result),
            Ok(Err(e)) => result(command_id, false, &e.to_string()),
            Err(_) => {
                let mut message = format!(
                    "job enqueued; outcome unconfirmed within {}s",
                    JOB_TIMEOUT.as_secs()
                );
                if !self.subscribed {
                    message.push_str(
                        "; note: systemd Subscribe() failed at startup, so job completion \
                         signals are not being emitted",
                    );
                }
                result(command_id, false, &message)
            }
        }
    }
}

/// Which unit hosts this process, via PID -> unit object path -> its `Id`.
/// Falls back to the compiled-in name when systemd can't tell us.
async fn discover_self_unit(conn: &zbus::Connection) -> Option<String> {
    async fn inner(conn: &zbus::Connection) -> zbus::Result<String> {
        let mgr = ManagerProxy::new(conn).await?;
        let path = mgr.get_unit_by_pid(std::process::id()).await?;
        let unit = UnitProxy::builder(conn)
            .path(path)?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?;
        unit.id().await
    }
    match inner(conn).await {
        Ok(id) => Some(id),
        Err(e) => {
            tracing::warn!(
                error = %e,
                fallback = FALLBACK_SELF_UNIT,
                "systemd: could not resolve this agent's own unit; using the fallback name"
            );
            Some(FALLBACK_SELF_UNIT.to_string())
        }
    }
}

/// The two pattern queries, unioned. Any error propagates so the caller can
/// return an empty list rather than a misleading partial one.
async fn list_units_inner(conn: &zbus::Connection) -> zbus::Result<Vec<ProtoUnit>> {
    let mgr = ManagerProxy::new(conn).await?;
    let services = mgr
        .list_units_by_patterns(vec![], vec!["*.service".to_string()])
        .await?;
    let failed = mgr
        .list_units_by_patterns(vec!["failed".to_string()], vec![])
        .await?;
    Ok(merge_units(services, failed))
}

/// Dispatch the verb and wait for ITS job to complete, returning systemd's own
/// result string. Unsupported verbs are rejected before any bus call — matching
/// `docker.rs` — but once we proceed, the signal stream is opened BEFORE the
/// start/stop/restart call — a fast job can otherwise complete before we are
/// listening, and that ordering must not move.
async fn run_verb_inner(conn: &zbus::Connection, verb: Verb, unit: &str) -> zbus::Result<String> {
    if !matches!(verb, Verb::UnitStart | Verb::UnitStop | Verb::UnitRestart) {
        return Err(zbus::Error::Failure(format!("unsupported verb {verb:?}")));
    }

    let mgr = ManagerProxy::new(conn).await?;
    let mut jobs = mgr.receive_job_removed().await?;

    let job_path = match verb {
        Verb::UnitStart => mgr.start_unit(unit, "replace").await?,
        Verb::UnitStop => mgr.stop_unit(unit, "replace").await?,
        Verb::UnitRestart => mgr.restart_unit(unit, "replace").await?,
        _ => unreachable!("verb validated above"),
    };

    while let Some(signal) = jobs.next().await {
        // A single malformed signal must not abort the wait: our job may still
        // be pending even though some unrelated signal failed to deserialize.
        let Ok(args) = signal.args() else { continue };
        if *args.job() == job_path {
            return Ok(args.result().to_string());
        }
    }
    Err(zbus::Error::Failure(
        "job signal stream ended before the job completed".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str, active: &str) -> UnitRecord {
        let path = OwnedObjectPath::try_from("/").expect("root object path");
        UnitRecord {
            name: name.to_string(),
            description: format!("desc for {name}"),
            load_state: "loaded".to_string(),
            active_state: active.to_string(),
            sub_state: if active == "active" {
                "running"
            } else {
                "dead"
            }
            .to_string(),
            followed: String::new(),
            object_path: path.clone(),
            job_id: 0,
            job_type: String::new(),
            job_object_path: path,
        }
    }

    #[test]
    fn record_to_unit_maps_the_five_proto_fields() {
        let u = record_to_unit(record("nginx.service", "active"));
        assert_eq!(u.name, "nginx.service");
        assert_eq!(u.load_state, "loaded");
        assert_eq!(u.active_state, "active");
        assert_eq!(u.sub_state, "running");
        assert_eq!(u.description, "desc for nginx.service");
    }

    #[test]
    fn merge_units_deduplicates_a_failed_service_present_in_both_queries() {
        // A failed *.service matches BOTH ListUnitsByPatterns calls; it must
        // appear exactly once.
        let services = vec![
            record("nginx.service", "failed"),
            record("cron.service", "active"),
        ];
        let failed = vec![
            record("nginx.service", "failed"),
            record("data.mount", "failed"),
        ];

        let merged = merge_units(services, failed);

        let names: Vec<&str> = merged.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names.iter().filter(|n| **n == "nginx.service").count(), 1);
        assert_eq!(merged.len(), 3, "nginx + cron + the failed .mount");
        assert!(
            names.contains(&"data.mount"),
            "failed non-service units must survive"
        );
    }

    #[test]
    fn normalize_target_appends_service_only_when_there_is_no_unit_suffix() {
        assert_eq!(normalize_target("nginx"), "nginx.service");
        assert_eq!(normalize_target("nginx.service"), "nginx.service");
        assert_eq!(normalize_target("data.mount"), "data.mount");
        assert_eq!(normalize_target("backup.timer"), "backup.timer");
        assert_eq!(normalize_target("getty@tty1"), "getty@tty1.service");
    }

    #[test]
    fn is_self_unit_matches_both_forms_but_not_a_lookalike() {
        let me = Some("argus-agent.service");
        assert!(is_self_unit("argus-agent.service", me));
        assert!(
            is_self_unit("argus-agent", me),
            "bare name normalizes to .service"
        );
        assert!(
            !is_self_unit("argus-agent-proxy.service", me),
            "prefix match must not count"
        );
        assert!(!is_self_unit("nginx.service", me));
    }

    #[test]
    fn is_self_unit_is_false_when_the_self_unit_is_unknown() {
        // Not running under systemd: there is no session-hosting unit to protect.
        assert!(!is_self_unit("argus-agent.service", None));
    }

    #[test]
    fn job_result_done_is_ok_everything_else_is_not() {
        let ok = job_result_to_command_result("c1".into(), "done");
        assert!(ok.ok);
        assert_eq!(ok.exit_code, 0);

        for bad in ["failed", "timeout", "canceled", "dependency", "skipped"] {
            let r = job_result_to_command_result("c1".into(), bad);
            assert!(!r.ok, "job result {bad} must not be ok");
            assert!(
                r.message.contains(bad),
                "message must carry systemd's own result string"
            );
        }
    }

    /// No system bus in the test environment: the client must degrade to
    /// `None` (unknown, not "no units") and a clearly-failed verb rather than
    /// panicking. This is the LXC-without-systemd / container path.
    #[tokio::test]
    async fn client_without_a_bus_reports_empty_and_fails_verbs_cleanly() {
        let client = SystemdClient {
            inner: None,
            self_unit: None,
            subscribed: false,
        };
        assert!(client.list_units().await.is_none());

        let r = client
            .run_verb("c1".into(), argus_proto::v1::Verb::UnitStart, "nginx")
            .await;
        assert!(!r.ok);
        assert!(
            r.message.contains("systemd"),
            "message must say why: {}",
            r.message
        );
    }

    /// The self-preservation guard must hold before any bus call, so it applies
    /// even on a host with no bus at all.
    #[tokio::test]
    async fn run_verb_refuses_the_agents_own_unit() {
        let client = SystemdClient {
            inner: None,
            self_unit: Some("argus-agent.service".to_string()),
            subscribed: false,
        };
        let r = client
            .run_verb("c1".into(), argus_proto::v1::Verb::UnitStop, "argus-agent")
            .await;
        assert!(!r.ok);
        assert!(
            r.message.contains("refusing"),
            "must explain the refusal, got: {}",
            r.message
        );
    }

    /// Live-bus unit listing. Ignored by default like the repo's other
    /// live-dependency tests; run with `--ignored` on a systemd host.
    #[tokio::test]
    #[ignore = "needs a live D-Bus system bus"]
    async fn live_bus_lists_units() {
        let client = SystemdClient::connect().await;
        let units = client
            .list_units()
            .await
            .expect("a live bus must yield Some(units)");
        assert!(
            !units.is_empty(),
            "a systemd host must report at least one unit"
        );
        assert!(
            units.iter().any(|u| u.name.ends_with(".service")),
            "the *.service query must return something"
        );
    }

    /// Live-bus verb execution: proves Subscribe + stream-before-call ordering +
    /// job-path correlation end to end. `paths.target` is already active, so
    /// starting it is a no-op for the host, but systemd still creates a real job
    /// that completes with result "done".
    #[tokio::test]
    #[ignore = "needs a live D-Bus system bus + root; polkit denies the verb otherwise"]
    async fn live_bus_run_verb_reports_a_real_job_result() {
        let client = SystemdClient::connect().await;
        let r = client
            .run_verb(
                "c1".into(),
                argus_proto::v1::Verb::UnitStart,
                "paths.target",
            )
            .await;
        assert!(
            r.ok,
            "starting an already-active target must succeed: {}",
            r.message
        );
        assert_eq!(r.message, "done");
    }
}
