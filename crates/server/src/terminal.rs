//! The browser side of the interactive terminal, bridged to the agent's PTY
//! through the `Hub`. `Binary` = keystrokes; `Text` = a tiny JSON control
//! channel (resize).
//!
//! The periodic Ping is a liveness *probe*, not a detector: Pongs aren't
//! tracked, so a dead peer is only noticed when a write finally errors (a
//! half-open TCP connection can take the kernel ~15 min to give up) -- the
//! 30-minute idle timer is the real backstop.

use crate::auth::AuthUser;
use crate::http::AppState;
use crate::hub::Hub;
use crate::repo::{self, Identity};
use axum::{
    extract::{
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use uuid::Uuid;

pub async fn terminal_ws(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    AuthUser(identity): AuthUser,
    ws: WebSocketUpgrade,
) -> Response {
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    if !origin_allowed(origin, host) {
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }
    ws.on_upgrade(move |socket| handle(state, id, identity, socket))
}

/// Same-origin check for the terminal's WS upgrade: browsers apply neither
/// same-origin policy nor CORS preflight to a WS handshake, so without this
/// any open page could silently open a root shell to a machine.
///
/// Deliberately reuses the request's own `Host` header rather than a
/// configured allowlist: Traefik forwards the browser's original `Host`
/// unchanged, so comparing `Origin`'s authority against `Host` needs no new
/// config surface. A missing `Origin` (non-browser client, e.g. `curl`) is
/// let through; only a PRESENT and MISMATCHED `Origin` is rejected.
fn origin_allowed(origin: Option<&str>, host: Option<&str>) -> bool {
    let (Some(origin), Some(host)) = (origin, host) else {
        return true;
    };
    // Strip the scheme to compare against the schemeless `Host` authority.
    let origin_authority = origin
        .split_once("://")
        .map_or(origin, |(_, authority)| authority);
    origin_authority.eq_ignore_ascii_case(host)
}

/// Distinct reasons the three concurrent loops in `handle` can end a
/// session, each mapped to a WS close code + text sent to the browser: a
/// bare `sink.close()` would make every case read as a generic "session
/// closed" in the UI.
enum CloseReason {
    /// The agent sent the `eof` marker: the shell exited on its own.
    ShellExited,
    /// `deliver_pty_output`'s full-channel backstop tore down just this ONE
    /// session; the agent connection itself stayed up.
    Overrun,
    /// The agent's whole Session ended (redeploy, reconnect, crash):
    /// `close_ptys_for` closed every PTY for the machine, this one included.
    AgentDisconnected,
    /// No keystroke for `TERMINAL_IDLE_SECS`.
    IdleTimeout,
    /// The browser closed its side (tab/nav) or sent a WS Close frame; it
    /// already knows why it closed, so this text is never actually read.
    ClientInitiated,
    /// A send to the browser's own socket failed -- the peer is presumably
    /// already gone, so nothing will receive this close frame either.
    SendFailed,
}

impl CloseReason {
    fn frame(&self) -> (u16, &'static str) {
        match self {
            CloseReason::ShellExited => (1000, "shell exited"),
            CloseReason::Overrun => (1011, "terminal closed: output overrun"),
            CloseReason::AgentDisconnected => (1001, "session closed (agent disconnected)"),
            CloseReason::IdleTimeout => (1000, "closed after 30 min idle"),
            CloseReason::ClientInitiated => (1000, "session closed"),
            CloseReason::SendFailed => (1006, "session closed"),
        }
    }
}

/// Outcome of the ordering-sensitive dispatch-then-audit sequence in
/// `open_and_audit`: two failure states because they need different
/// teardown -- a dispatch failure never reached the agent (nothing to close);
/// an audit failure means a live PTY exists and still owes a `PtyClose`.
enum OpenOutcome {
    /// The agent was offline (or gone) — `PtyOpen` never reached it. The
    /// session was minted and already unwound (`close_pty` already called).
    DispatchFailed,
    /// `PtyOpen` reached the agent, but the `terminal.open` audit write
    /// failed. Carries `session_id` so the caller can still send `PtyClose`
    /// for the PTY that IS live on the agent.
    AuditFailed { session_id: String },
    /// Both succeeded; the terminal is live.
    Ready {
        session_id: String,
        rx: mpsc::Receiver<Vec<u8>>,
    },
}

/// Mints a session, dispatches `PtyOpen`, and only writes the `terminal.open`
/// audit row if dispatch reached a live agent (dispatch before audit, fail
/// closed -- mirrors `logs.open`). Factored out of `handle` so a test can
/// drive this exact ordering directly: a regression that swapped the two
/// calls would audit a session whose dispatch never reached the agent.
async fn open_and_audit(state: &AppState, machine_id: Uuid, identity: &Identity) -> OpenOutcome {
    let (session_id, rx, _pty_state) = state.hub.open_pty(machine_id);
    if state
        .hub
        .send_pty_open(machine_id, session_id.clone(), 80, 24)
        .await
        .is_err()
    {
        state.hub.close_pty(&session_id);
        return OpenOutcome::DispatchFailed;
    }

    let command_id = Uuid::new_v4();
    if let Err(e) = repo::audit_command(
        &state.pool,
        repo::Actor::User(identity),
        "terminal.open",
        Some(machine_id),
        &session_id,
        command_id,
        "ok",
    )
    .await
    {
        tracing::error!(error = %e, "terminal: audit write failed; closing");
        return OpenOutcome::AuditFailed { session_id };
    }

    OpenOutcome::Ready { session_id, rx }
}

/// Cleans up a PTY session (`close_pty` + best-effort `PtyClose` dispatch) on
/// every exit past a successful `PtyOpen` dispatch, including a panic unwind
/// that a plain call after the joint `select!` would miss. `Drop` is sync,
/// so the `PtyClose` send is spawned and only logged, never awaited.
///
/// This is the ONLY place `close_pty`/`send_pty_close` are called for a
/// session that made it this far, so teardown cannot run twice.
struct PtyCloseGuard {
    hub: Arc<Hub>,
    machine_id: Uuid,
    session_id: String,
}

impl Drop for PtyCloseGuard {
    fn drop(&mut self) {
        self.hub.close_pty(&self.session_id);
        let hub = self.hub.clone();
        let machine_id = self.machine_id;
        let session_id = self.session_id.clone();
        tokio::spawn(async move {
            if let Err(e) = hub.send_pty_close(machine_id, session_id).await {
                tracing::debug!(error = ?e, "terminal: PtyClose not delivered (agent gone)");
            }
        });
    }
}

async fn handle(state: AppState, machine_id: Uuid, identity: Identity, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();

    let (session_id, mut rx) = match open_and_audit(&state, machine_id, &identity).await {
        OpenOutcome::DispatchFailed => {
            let _ = sink.send(Message::Text("agent not connected".into())).await;
            let _ = sink.close().await;
            return;
        }
        OpenOutcome::AuditFailed { session_id } => {
            // A PTY genuinely exists despite the audit failure; the guard's
            // Drop sends PtyClose even though this scope never reaches the
            // joint select! below.
            let _guard = PtyCloseGuard {
                hub: state.hub.clone(),
                machine_id,
                session_id,
            };
            let _ = sink.close().await;
            return;
        }
        OpenOutcome::Ready { session_id, rx } => (session_id, rx),
    };

    // From here the PTY is live and fully audited; `_guard`'s Drop is the
    // single teardown place for this session -- see its doc comment.
    let _guard = PtyCloseGuard {
        hub: state.hub.clone(),
        machine_id,
        session_id: session_id.clone(),
    };

    // Idle timer shared with the input loop: reset on each keystroke.
    let last_input = Arc::new(std::sync::Mutex::new(tokio::time::Instant::now()));

    // Outbound: drains the buffer to the socket (accounting flow-control
    // bytes) and emits keepalive pings on the same sink -- both share one
    // task since `WebSocket::split` gives a single sink. The ping is what
    // detects a crashed tab or slept laptop that never sent a clean close.
    //
    // `sink` stays owned by `handle` (not moved into `outbound`) so it's
    // still usable after the joint select! below to send a final Close
    // frame; `outbound` only reborrows it.
    let hub_out = state.hub.clone();
    let sid_out = session_id.clone();
    let sink_ref = &mut sink;
    let outbound = async move {
        let mut ping = tokio::time::interval(Duration::from_secs(30));
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ping.tick().await; // the first tick is immediate; skip it
        loop {
            tokio::select! {
                next = rx.recv() => {
                    let Some(bytes) = next else {
                        // No eof: either the whole Session ended or just this
                        // one overran -- `Hub::is_connected`'s doc explains
                        // how these are told apart.
                        break if hub_out.is_connected(machine_id) {
                            CloseReason::Overrun
                        } else {
                            CloseReason::AgentDisconnected
                        };
                    };
                    if bytes.is_empty() {
                        break CloseReason::ShellExited; // eof marker
                    }
                    let n = bytes.len();
                    if sink_ref.send(Message::Binary(bytes.into())).await.is_err() {
                        break CloseReason::SendFailed;
                    }
                    hub_out.on_pty_drained(&sid_out, n).await;
                }
                _ = ping.tick() => {
                    // A dead peer fails the send (or the TCP keepalive kills the
                    // connection), which ends the loop and tears the session down.
                    if sink_ref.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break CloseReason::SendFailed;
                    }
                }
            }
        }
    };

    // Inbound: keystrokes (Binary) -> PtyInput (+reset idle); resize (Text) ->
    // PtyResize; pong resets nothing here (handled by the keepalive loop below).
    let hub_in = state.hub.clone();
    let sid_in = session_id.clone();
    let last_input_in = last_input.clone();
    let inbound = async move {
        while let Some(Ok(msg)) = stream.next().await {
            match msg {
                Message::Binary(data) => {
                    *last_input_in.lock().unwrap() = tokio::time::Instant::now();
                    if hub_in
                        .send_pty_input(machine_id, sid_in.clone(), data.into())
                        .await
                        .is_err()
                    {
                        return CloseReason::AgentDisconnected;
                    }
                }
                Message::Text(t) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                        if let Some(r) = v.get("resize") {
                            // Clamped: a hostile client can send 0 or near
                            // u64::MAX. The agent truncates to u16 and
                            // swallows ioctl errors, but no reason to forward nonsense.
                            let cols = r
                                .get("cols")
                                .and_then(|c| c.as_u64())
                                .unwrap_or(80)
                                .clamp(1, 1000) as u32;
                            let rows = r
                                .get("rows")
                                .and_then(|c| c.as_u64())
                                .unwrap_or(24)
                                .clamp(1, 1000) as u32;
                            let _ = hub_in
                                .send_pty_resize(machine_id, sid_in.clone(), cols, rows)
                                .await;
                        }
                    }
                }
                Message::Close(_) => return CloseReason::ClientInitiated,
                _ => {}
            }
        }
        // The stream ended with no explicit Close frame (e.g. the TCP
        // connection just dropped): the browser side is gone either way.
        CloseReason::ClientInitiated
    };

    // Idle watchdog: close once TERMINAL_IDLE_SECS pass with no keystroke.
    let idle = async move {
        let window = Duration::from_secs(argus_common::TERMINAL_IDLE_SECS);
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            if last_input.lock().unwrap().elapsed() >= window {
                break CloseReason::IdleTimeout;
            }
        }
    };

    // Whichever finishes first ends the session; send a distinct,
    // human-readable close reason first (see `CloseReason`). `sink` is still
    // owned by this scope (see the comment above `outbound`).
    let reason = tokio::select! {
        r = outbound => r,
        r = inbound => r,
        r = idle => r,
    };
    let (code, text) = reason.frame();
    let _ = sink
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: text.into(),
        })))
        .await;
    let _ = sink.close().await;

    // `_guard` drops here (and on any earlier return or panic unwind above),
    // sending PtyClose and calling close_pty exactly once.
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use sqlx::PgPool;

    fn app_state_with_hub(pool: PgPool) -> (AppState, Arc<Hub>) {
        let hub = Arc::new(Hub::new());
        let oidc = Arc::new(crate::config::OidcConfig {
            issuer: "https://idp.invalid".into(),
            client_id: "cid".into(),
            client_secret: "secret".into(),
            required_role: crate::config::RequiredRole::Named("argus-admin".into()),
            roles_claim: "groups".into(),
            scopes: vec!["openid".into()],
            public_url: "http://localhost:8080".into(),
            ca_cert_path: None,
        });
        // Never triggers discovery: building the client is local-only, so
        // this is cheap and touches no network.
        let oidc_client = Arc::new(
            crate::auth::oidc::OidcClient::new(oidc.clone()).expect("build test OIDC client"),
        );
        (
            AppState {
                pool,
                hub: hub.clone(),
                oidc: Some(oidc),
                cipher: Arc::new(
                    crate::crypto::FieldCipher::from_b64_key(
                        &base64::engine::general_purpose::STANDARD.encode([9u8; 32]),
                    )
                    .expect("build test field cipher"),
                ),
                oidc_client: Some(oidc_client),
                public_url: "http://localhost:8080".into(),
                limiter: Arc::new(crate::auth::ratelimit::LoginLimiter::new()),
                agent_endpoints: vec!["https://agents.test:9443".into()],
                agent_binary: None,
            },
            hub,
        )
    }

    fn test_identity() -> Identity {
        Identity {
            subject: "term-user".into(),
            email: Some("term-user@example.com".into()),
            display_name: None,
        }
    }

    /// Drives the REAL entry point (`open_and_audit`) against an offline
    /// machine -- not `Hub::open_pty`/`send_pty_open` directly, which would
    /// trivially show no audit row regardless of internal ordering. A
    /// regression that swapped dispatch and audit would leave a row here.
    #[sqlx::test]
    async fn open_and_audit_dispatches_before_auditing(pool: PgPool) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('term-offline','h','offline') RETURNING id"
        ).fetch_one(&pool).await?.id;
        let (state, _hub) = app_state_with_hub(pool.clone());

        let outcome = open_and_audit(&state, machine_id, &test_identity()).await;
        assert!(
            matches!(outcome, OpenOutcome::DispatchFailed),
            "no agent connected -> dispatch must fail"
        );

        let rows = sqlx::query!(
            "SELECT count(*) as n FROM audit_log WHERE machine_id=$1 AND action='terminal.open'",
            machine_id
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            rows.n,
            Some(0),
            "open_and_audit returned before writing the terminal.open row on a failed \
             dispatch -- swapping the dispatch and audit calls inside it would leave a row here"
        );

        Ok(())
    }

    #[test]
    fn origin_allowed_accepts_a_same_origin_request() {
        assert!(origin_allowed(
            Some("https://argus.lab.example"),
            Some("argus.lab.example")
        ));
        // Host header carries a port; Origin must match it exactly.
        assert!(origin_allowed(
            Some("http://localhost:8080"),
            Some("localhost:8080")
        ));
    }

    #[test]
    fn origin_allowed_rejects_a_mismatched_origin() {
        assert!(!origin_allowed(
            Some("https://evil.example"),
            Some("argus.lab.example")
        ));
        // Same host, different port -- browsers treat this as a different
        // origin, and so must this check.
        assert!(!origin_allowed(
            Some("http://argus.lab.example:1234"),
            Some("argus.lab.example")
        ));
    }

    #[test]
    fn origin_allowed_rejects_the_opaque_null_origin() {
        // Sent by e.g. a sandboxed iframe or a `file://` page -- never a
        // legitimate same-origin browser request.
        assert!(!origin_allowed(Some("null"), Some("argus.lab.example")));
    }

    #[test]
    fn origin_allowed_lets_a_missing_origin_through() {
        // A missing Origin is let through -- see `origin_allowed`'s doc.
        assert!(origin_allowed(None, Some("argus.lab.example")));
        assert!(origin_allowed(None, None));
    }

    #[test]
    fn origin_allowed_is_case_insensitive_on_the_host() {
        assert!(origin_allowed(
            Some("https://Argus.Lab.Example"),
            Some("argus.lab.example")
        ));
    }
}
