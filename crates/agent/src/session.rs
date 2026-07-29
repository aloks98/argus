//! The single persistent mTLS Session (PRD §4, §5.4).

use crate::config::Config;
use crate::docker::DockerClient;
use crate::enroll::Identity;
use crate::systemd::SystemdClient;
use anyhow::{Context, Result};
use argus_proto::v1::agent_service_client::AgentServiceClient;
use argus_proto::v1::{
    agent_frame, server_frame, AgentFrame, CommandResult, DockerState, Heartbeat, Hello, LogChunk,
    SystemdState, Verb,
};
use rand::Rng;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint};
use tonic::Request;

/// A session up at least this long counts "stable" and resets the backoff
/// streak -- roughly two heartbeat intervals, long enough that a
/// connect-then-immediately-die flap can't masquerade as healthy.
const STABLE: Duration = Duration::from_secs(30);

/// How long a single `.connect()` may hang before it's treated as a failed
/// attempt, so a half-open TLS handshake can't stall the loop forever.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Holds the persistent `Session` stream, multiplexing metrics, docker/
/// systemd state, log tails, PTY, and command results by `stream_id`.
/// Reconnects with backoff + jitter, re-sending `Hello` so the fleet view
/// self-heals (PRD §2.5, §5.4).
///
/// Never returns under normal operation -- a broken session just feeds back
/// into the reconnect loop. `Err` is reserved for a future unusable-`cfg`
/// path; none exists today.
pub async fn run(cfg: &Config, identity: Identity) -> Result<()> {
    let docker = DockerClient::connect();
    let mut attempt = 0u32;
    loop {
        // Re-dialed fresh every attempt (unlike `docker`, dialed once above):
        // a `zbus::Connection` doesn't self-heal. If dbus-daemon restarts, a
        // client dialed earlier would keep erroring silently, reporting an
        // empty unit list forever -- "nothing wrong" in the UI. Bollard
        // re-dials its socket per request, so `docker` doesn't share this.
        let systemd = SystemdClient::connect().await;
        let started = tokio::time::Instant::now();
        let outcome = connect_and_serve(cfg, &identity, &docker, &systemd).await;
        let lasted = started.elapsed();

        // A session that stayed up a while was healthy -> reset backoff. A
        // fast connect-then-die grows it instead, so a broken/rejected agent
        // backs off rather than hammering the control plane (PRD §2.5).
        if should_reset_backoff(outcome.is_ok(), lasted) {
            attempt = 0;
        } else {
            attempt = attempt.saturating_add(1);
        }

        match &outcome {
            Ok(()) => tracing::warn!(attempt, ?lasted, "session ended; reconnecting"),
            Err(e) => tracing::warn!(error = %e, attempt, ?lasted, "session failed; backing off"),
        }
        tokio::time::sleep(next_backoff(attempt)).await;
    }
}

fn should_reset_backoff(outcome_ok: bool, lasted: Duration) -> bool {
    outcome_ok && lasted >= STABLE
}

/// Connects once, holds the bidi stream until it ends, then returns so the
/// caller can measure how long it lasted. `Ok(())` = established and later
/// ended cleanly; `Err` = the connect failed or the stream errored.
async fn connect_and_serve(
    cfg: &Config,
    identity: &Identity,
    docker: &DockerClient,
    systemd: &SystemdClient,
) -> Result<()> {
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(&identity.ca_cert_pem))
        .identity(tonic::transport::Identity::from_pem(
            &identity.client_cert_pem,
            &identity.client_key_pem,
        ));

    let channel = Endpoint::from_shared(cfg.endpoint.clone())
        .context("parsing agent endpoint")?
        .tls_config(tls)
        .context("configuring session channel TLS")?
        .connect_timeout(CONNECT_TIMEOUT)
        .connect()
        .await
        .context("connecting to control plane for session")?;

    tracing::info!(agent_id = %identity.agent_id, endpoint = %cfg.endpoint, "session: connected");

    let (tx, rx) = mpsc::channel::<AgentFrame>(16);
    let inbound_tx = tx.clone();
    let inbound_docker = docker.clone();
    let sender_docker = docker.clone();
    let inbound_systemd = systemd.clone();
    let sender_systemd = systemd.clone();

    // request_id -> the task running that tail; must be cancellable by
    // LogTailStop and must not outlive this session (aborted on return).
    let tails: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<String, tokio::task::AbortHandle>>,
    > = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let inbound_tails = tails.clone();
    let inbound_docker_logs = docker.clone();

    // session_id -> the live PTY; mirrors `tails` (closeable by PtyClose,
    // must not outlive this session).
    let ptys: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<String, crate::pty::PtyHandle>>,
    > = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let inbound_ptys = ptys.clone();

    // Sender task: Hello first (re-sent each reconnect so the fleet view
    // self-heals), then Heartbeat per tick. It can't exit on its own until
    // `rx` drops, which needs this fn to return first -- so it's explicitly
    // `.abort()`'d below on every exit path instead.
    let sender_agent_id = identity.agent_id.clone();
    // Probed once per session; re-probing each reconnect reflects a gained/
    // lost subsystem without an agent restart.
    let caps = crate::capabilities::probe(systemd, docker).await;
    let sender = tokio::spawn(async move {
        let mut info = match crate::info::gather(env!("CARGO_PKG_VERSION")) {
            Ok(info) => info,
            Err(e) => {
                tracing::warn!(error = %e, "session: gathering AgentInfo for Hello failed");
                return;
            }
        };
        info.capabilities = caps;
        // Marks the capability set authoritative. An unset agent reports
        // `false`, which the control plane stores as NULL / "unknown, gate
        // nothing" -- correct for a pre-capability agent.
        info.capabilities_reported = true;

        if tx
            .send(AgentFrame {
                stream_id: argus_common::CONTROL_STREAM_ID,
                payload: Some(agent_frame::Payload::Hello(Hello { info: Some(info) })),
            })
            .await
            .is_err()
        {
            return;
        }

        // Initial Docker snapshot right after Hello so the panel populates promptly
        // (the first ticker tick is a full interval away).
        let containers = sender_docker.list_containers().await;
        if tx
            .send(AgentFrame {
                stream_id: argus_common::CONTROL_STREAM_ID,
                payload: Some(agent_frame::Payload::DockerState(DockerState {
                    containers,
                })),
            })
            .await
            .is_err()
        {
            return;
        }

        // Initial systemd snapshot alongside Docker's, so Units populates
        // promptly. `None` means collection failed; skip the send rather
        // than claim "no units" (see `SystemdClient::list_units`).
        if let Some(units) = sender_systemd.list_units().await {
            if tx
                .send(AgentFrame {
                    stream_id: argus_common::CONTROL_STREAM_ID,
                    payload: Some(agent_frame::Payload::SystemdState(SystemdState { units })),
                })
                .await
                .is_err()
            {
                return;
            }
        }

        let start = tokio::time::Instant::now();
        let mut ticker = tokio::time::interval(Duration::from_secs(
            argus_common::DEFAULT_HEARTBEAT_SECS as u64,
        ));
        // If briefly backpressured, don't fire a catch-up burst of missed
        // ticks -- delay the next one from whenever polling actually resumes.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick fires immediately; the Hello above already covers
        // "just connected," so skip it and wait for the first real interval.
        ticker.tick().await;

        // Lives across ticks (not per-tick) so CPU-usage is a real delta
        // between samples, not the unreliable first-call reading (metrics.rs).
        let mut sampler = crate::metrics::Sampler::new();

        loop {
            ticker.tick().await;
            let unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            // Heartbeat's uptime is process uptime, vestigial -- the server
            // only uses Heartbeat for last_seen. Real host uptime ships in
            // MetricsSample.uptime_secs.
            let uptime_secs = start.elapsed().as_secs();

            if tx
                .send(AgentFrame {
                    stream_id: argus_common::CONTROL_STREAM_ID,
                    payload: Some(agent_frame::Payload::Heartbeat(Heartbeat {
                        unix_ms,
                        uptime_secs,
                    })),
                })
                .await
                .is_err()
            {
                tracing::debug!(agent_id = %sender_agent_id, "session: heartbeat sender exiting, channel closed");
                return;
            }

            if tx
                .send(AgentFrame {
                    stream_id: argus_common::CONTROL_STREAM_ID,
                    payload: Some(agent_frame::Payload::Metrics(sampler.sample())),
                })
                .await
                .is_err()
            {
                tracing::debug!(agent_id = %sender_agent_id, "session: metrics sender exiting, channel closed");
                return;
            }

            let containers = sender_docker.list_containers().await;
            if tx
                .send(AgentFrame {
                    stream_id: argus_common::CONTROL_STREAM_ID,
                    payload: Some(agent_frame::Payload::DockerState(DockerState {
                        containers,
                    })),
                })
                .await
                .is_err()
            {
                tracing::debug!(agent_id = %sender_agent_id, "session: docker sender exiting, channel closed");
                return;
            }

            if let Some(units) = sender_systemd.list_units().await {
                if tx
                    .send(AgentFrame {
                        stream_id: argus_common::CONTROL_STREAM_ID,
                        payload: Some(agent_frame::Payload::SystemdState(SystemdState { units })),
                    })
                    .await
                    .is_err()
                {
                    tracing::debug!(agent_id = %sender_agent_id, "session: systemd sender exiting, channel closed");
                    return;
                }
            }
        }
    });

    // Wrapped in its own future so that whatever happens -- including the
    // `?` on `.session(...).await` erroring immediately (e.g. a rejected
    // cert) -- control always reaches `sender.abort()` below instead of
    // early-returning around it.
    let result = async {
        let mut inbound = AgentServiceClient::new(channel)
            .session(Request::new(ReceiverStream::new(rx)))
            .await
            .context("opening Session stream")?
            .into_inner();

        loop {
            match inbound.next().await {
                Some(Ok(frame)) => {
                    match frame.payload {
                        Some(server_frame::Payload::Command(cmd)) => {
                            // Verbs run in their own task (loss-tolerant fire-and-forget) so a
                            // slow stop can't stall the inbound loop or heartbeats.
                            let stream_id = frame.stream_id;
                            let docker = inbound_docker.clone();
                            let systemd = inbound_systemd.clone();
                            let out = inbound_tx.clone();
                            tokio::spawn(async move {
                                let verb = Verb::try_from(cmd.verb).unwrap_or(Verb::Unspecified);
                                let result = match verb {
                                    Verb::ContainerStart
                                    | Verb::ContainerStop
                                    | Verb::ContainerRestart => {
                                        docker
                                            .run_verb(cmd.command_id.clone(), verb, &cmd.target)
                                            .await
                                    }
                                    Verb::UnitStart | Verb::UnitStop | Verb::UnitRestart => {
                                        systemd
                                            .run_verb(cmd.command_id.clone(), verb, &cmd.target)
                                            .await
                                    }
                                    // Always reply: an unanswered command leaves the
                                    // control plane's bounded wait to time out into a
                                    // misleading "pending" for a verb that never ran.
                                    Verb::Unspecified => CommandResult {
                                        command_id: cmd.command_id.clone(),
                                        ok: false,
                                        exit_code: 1,
                                        message: format!("unsupported verb code {}", cmd.verb),
                                    },
                                };
                                let _ = out
                                    .send(AgentFrame {
                                        stream_id,
                                        payload: Some(agent_frame::Payload::CommandResult(result)),
                                    })
                                    .await;
                            });
                        }
                        Some(server_frame::Payload::LogTailStart(req)) => {
                            let stream_id = frame.stream_id;
                            let out = inbound_tx.clone();
                            let docker = inbound_docker_logs.clone();
                            let tails = inbound_tails.clone();
                            let request_id = req.request_id.clone();
                            match crate::logs::parse_source(&req.source) {
                                Ok(source) => {
                                    let rid = request_id.clone();
                                    let filters = crate::logs::JournalFilters {
                                        max_priority: req.max_priority,
                                        since_ms: req.since_ms,
                                        current_boot: req.current_boot,
                                    };
                                    let handle = tokio::spawn(async move {
                                        crate::logs::run_tail(
                                            source,
                                            req.tail_lines,
                                            req.follow,
                                            req.before_cursor,
                                            filters,
                                            docker,
                                            out,
                                            rid,
                                            stream_id,
                                        )
                                        .await;
                                    });
                                    // Aborts any tail this request_id already had:
                                    // a dropped AbortHandle does NOT abort its
                                    // task, so a displaced tail would leak,
                                    // unreachable by LogTailStop or teardown. A
                                    // correct server won't reuse a request_id,
                                    // but the agent doesn't trust it to.
                                    if let Some(old) = tails
                                        .lock()
                                        .unwrap()
                                        .insert(request_id, handle.abort_handle())
                                    {
                                        old.abort();
                                    }
                                }
                                Err(e) => {
                                    // The server validates too, so this is a bug
                                    // or an attack rather than ordinary input.
                                    tracing::warn!(source = %req.source, error = ?e, "log tail: rejected source");
                                    let _ = inbound_tx
                                        .try_send(AgentFrame {
                                            stream_id,
                                            payload: Some(agent_frame::Payload::LogChunk(LogChunk {
                                                request_id,
                                                data: Vec::new(),
                                                eof: true,
                                            })),
                                        });
                                }
                            }
                        }
                        Some(server_frame::Payload::LogTailStop(stop)) => {
                            if let Some(handle) =
                                inbound_tails.lock().unwrap().remove(&stop.request_id)
                            {
                                handle.abort();
                            }
                        }
                        Some(server_frame::Payload::PtyOpen(req)) => {
                            let out = inbound_tx.clone();
                            match crate::pty::open(
                                req.session_id.clone(),
                                frame.stream_id,
                                req.cols as u16,
                                req.rows as u16,
                                &req.shell,
                                out,
                            ) {
                                Ok(handle) => {
                                    if let Some(old) =
                                        inbound_ptys.lock().unwrap().insert(req.session_id, handle)
                                    {
                                        // `close()` blocks joining the reader thread
                                        // (which can itself be parked in
                                        // `blocking_send` against a full channel --
                                        // see pty.rs). Never run that join inline;
                                        // hand it to the blocking pool.
                                        tokio::task::spawn_blocking(move || old.close());
                                    }
                                }
                                Err(e) => {
                                    // Surface the failure in the terminal as an eof
                                    // notice, mirroring how a shell exit is reported.
                                    let _ = inbound_tx
                                        .send(AgentFrame {
                                            stream_id: frame.stream_id,
                                            payload: Some(agent_frame::Payload::PtyOutput(
                                                argus_proto::v1::PtyOutput {
                                                    session_id: req.session_id,
                                                    data: format!("could not start a shell: {e}\r\n")
                                                        .into_bytes(),
                                                    eof: true,
                                                },
                                            )),
                                        })
                                        .await;
                                }
                            }
                        }
                        Some(server_frame::Payload::PtyInput(inp)) => {
                            if let Some(h) = inbound_ptys.lock().unwrap().get(&inp.session_id) {
                                h.write_input(&inp.data);
                            }
                        }
                        Some(server_frame::Payload::PtyResize(rz)) => {
                            if let Some(h) = inbound_ptys.lock().unwrap().get(&rz.session_id) {
                                h.resize(rz.cols as u16, rz.rows as u16);
                            }
                        }
                        Some(server_frame::Payload::PtyFlow(fl)) => {
                            if let Some(h) = inbound_ptys.lock().unwrap().get(&fl.session_id) {
                                h.set_paused(fl.paused);
                            }
                        }
                        Some(server_frame::Payload::PtyClose(cl)) => {
                            if let Some(h) = inbound_ptys.lock().unwrap().remove(&cl.session_id) {
                                // See PtyOpen above: `close()` blocks on a thread
                                // join, so it must not run inline here either.
                                tokio::task::spawn_blocking(move || h.close());
                            }
                        }
                        _ => {}
                    }
                }
                Some(Err(status)) => {
                    return Err(anyhow::anyhow!("session stream error: {status}"));
                }
                None => {
                    return Ok(());
                }
            }
        }
    }
    .await;

    // The bidi call is done either way (even if it never opened); make sure
    // the sender isn't left running into the next reconnect attempt.
    sender.abort();

    // A tail belongs to the session that asked for it. Without this an ended
    // session would leave `journalctl -f` running until the agent restarts.
    for (_, handle) in tails.lock().unwrap().drain() {
        handle.abort();
    }

    // A PTY belongs to the session that opened it. `close()` blocks joining
    // the reader thread (see PtyOpen arm above / pty.rs), so teardown runs
    // on the blocking pool, not inline on this async fn.
    for (_, handle) in ptys.lock().unwrap().drain() {
        tokio::task::spawn_blocking(move || handle.close());
    }

    result
}

/// Exponential backoff: 500ms * 2^attempt, capped at 30s, +/-20% jitter.
fn next_backoff(attempt: u32) -> Duration {
    let capped = Duration::from_millis(500)
        .saturating_mul(2u32.saturating_pow(attempt.min(6)))
        .min(Duration::from_secs(30));

    let jitter = rand::rng().random_range(0.8..1.2);
    capped.mul_f64(jitter)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_backoff_first_attempt_is_short() {
        assert!(next_backoff(0) < Duration::from_secs(2));
    }

    #[test]
    fn next_backoff_never_exceeds_cap_plus_jitter() {
        for attempt in 0..20 {
            let backoff = next_backoff(attempt);
            assert!(
                backoff <= Duration::from_millis(36_000),
                "attempt {attempt} produced {backoff:?}, exceeding the ~36s cap+jitter ceiling"
            );
        }
    }

    #[test]
    fn next_backoff_caps_growth_for_large_attempts() {
        for attempt in [10u32, 19u32] {
            let backoff = next_backoff(attempt);
            assert!(
                backoff <= Duration::from_millis(36_000),
                "attempt {attempt} produced {backoff:?}, expected <= 36s"
            );
            assert!(
                backoff >= Duration::from_millis(24_000),
                "attempt {attempt} produced {backoff:?}, expected >= ~24s (30s cap - 20% jitter)"
            );
        }
    }

    #[test]
    fn should_reset_backoff_on_stable_session() {
        assert!(should_reset_backoff(true, Duration::from_secs(60)));
    }

    #[test]
    fn should_not_reset_backoff_on_fast_ok_flap() {
        // Connects fine but dies almost immediately (e.g. `info::gather()`
        // fails, so `Hello` never sends) -- must NOT reset, or backoff never
        // grows.
        assert!(!should_reset_backoff(true, Duration::from_millis(200)));
    }

    #[test]
    fn should_not_reset_backoff_on_error_regardless_of_duration() {
        assert!(!should_reset_backoff(false, Duration::from_secs(60)));
        assert!(!should_reset_backoff(false, Duration::from_millis(200)));
    }
}
