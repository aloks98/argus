//! The single persistent mTLS Session (PRD §4, §5.4).

use crate::config::Config;
use crate::enroll::Identity;
use anyhow::{Context, Result};
use argus_proto::v1::agent_service_client::AgentServiceClient;
use argus_proto::v1::{agent_frame, AgentFrame, Heartbeat, Hello};
use rand::Rng;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint};
use tonic::Request;

/// A session that stayed up at least this long counts as "stable" -- its
/// backoff streak resets. Roughly two heartbeat intervals, long enough that a
/// connect-then-immediately-die flap (rejected cert, `Hello` never sent, etc.)
/// can never masquerade as a healthy connection.
const STABLE: Duration = Duration::from_secs(30);

/// How long a single `.connect()` may hang before we give up and treat it as
/// a failed attempt, so a half-open TLS handshake can't stall the loop
/// forever.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Hold the persistent `Session` stream, multiplexing metrics, docker/systemd
/// state, log tails, PTY, and command results by `stream_id`. Reconnect with
/// exponential backoff + jitter; re-send a `Hello` snapshot on reconnect so the
/// fleet view self-heals (PRD §2.5, §5.4).
///
/// Under normal operation this never returns: a broken/ended session just
/// feeds back into the backoff+reconnect loop. It only returns `Err` if `cfg`
/// or `identity` are unusable in a way no amount of retrying will fix (there
/// currently is no such path, but the signature stays fallible for that
/// future case and to match `main`'s `?`-propagation).
pub async fn run(cfg: &Config, identity: Identity) -> Result<()> {
    let mut attempt = 0u32;
    loop {
        let started = tokio::time::Instant::now();
        let outcome = connect_and_serve(cfg, &identity).await;
        let lasted = started.elapsed();

        // A session that stayed up a while was healthy -> reset. A fast
        // connect-then-die (or an outright connect failure) grows backoff, so
        // a broken or rejected agent backs off instead of hammering the
        // control plane (PRD §2.5).
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

/// Pure stability decision, factored out so it's testable without any
/// network or timing flakiness: only a session that both succeeded *and*
/// lasted at least `STABLE` counts as healthy enough to reset the backoff
/// streak.
fn should_reset_backoff(outcome_ok: bool, lasted: Duration) -> bool {
    outcome_ok && lasted >= STABLE
}

/// Connect once, hold the bidi stream until it ends (cleanly or with an
/// error), then return so the caller can measure how long it lasted and back
/// off accordingly. `Ok(())` means the session was established and later
/// ended cleanly (server closed its side); `Err` means the connect itself
/// failed or the stream errored.
async fn connect_and_serve(cfg: &Config, identity: &Identity) -> Result<()> {
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

    // Sender task: Hello first (fresh snapshot, re-sent on every reconnect so
    // the fleet view self-heals), then a Heartbeat on every tick. It normally
    // exits on its own once `tx` can no longer deliver, but that only happens
    // once `rx` is dropped -- which requires this function to return first.
    // So it's also explicitly `.abort()`'d below on every exit path (success,
    // stream error, or the RPC never opening at all), rather than relying
    // solely on drop-timing.
    let sender_agent_id = identity.agent_id.clone();
    let sender = tokio::spawn(async move {
        let info = match crate::info::gather(env!("CARGO_PKG_VERSION")) {
            Ok(info) => info,
            Err(e) => {
                tracing::warn!(error = %e, "session: gathering AgentInfo for Hello failed");
                return;
            }
        };

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

        let start = tokio::time::Instant::now();
        let mut ticker = tokio::time::interval(Duration::from_secs(
            argus_common::DEFAULT_HEARTBEAT_SECS as u64,
        ));
        // If the sender is briefly backpressured (slow send, tokio scheduling
        // delay, etc.) don't fire a catch-up burst of missed ticks -- just
        // delay the next one from whenever we actually got back to polling.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick fires immediately; the Hello above already covers
        // "just connected," so skip it and wait for the first real interval.
        ticker.tick().await;

        loop {
            ticker.tick().await;
            let unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            // TODO(metrics slice): report real host uptime; process uptime is
            // a placeholder that's good enough for the Spine.
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
        }
    });

    // The fallible RPC-open + drain is captured in its own future so that,
    // whatever happens -- including the `?` on `.session(...).await` erroring
    // out immediately (e.g. a revoked/expired/mismatched cert: the server
    // rejects at RPC-open with `Status::unauthenticated`) -- control always
    // reaches `sender.abort()` below instead of early-returning around it.
    let result = async {
        let mut inbound = AgentServiceClient::new(channel)
            .session(Request::new(ReceiverStream::new(rx)))
            .await
            .context("opening Session stream")?
            .into_inner();

        loop {
            match inbound.next().await {
                Some(Ok(_frame)) => {
                    // Spine: HelloAck/Ping/etc. are not yet acted on -- later
                    // slices wire commands, PTY, etc. up here.
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

    // The bidi call is done either way (including if it never opened at
    // all); make sure the heartbeat sender isn't left running into the next
    // reconnect attempt.
    sender.abort();

    result
}

/// Exponential backoff with a 30s cap and +/-20% jitter, keyed by connection
/// attempt number. Pure and deterministic-shaped (modulo jitter) so it's
/// testable without any network.
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
        // Connects fine but dies almost immediately (e.g. the sender's
        // `info::gather()` fails, so `Hello` is never sent and the server
        // closes the session cleanly) -- must NOT reset, or backoff never
        // grows.
        assert!(!should_reset_backoff(true, Duration::from_millis(200)));
    }

    #[test]
    fn should_not_reset_backoff_on_error_regardless_of_duration() {
        assert!(!should_reset_backoff(false, Duration::from_secs(60)));
        assert!(!should_reset_backoff(false, Duration::from_millis(200)));
    }
}
